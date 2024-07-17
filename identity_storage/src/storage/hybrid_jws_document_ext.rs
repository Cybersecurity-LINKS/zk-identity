use std::borrow::Cow;
use std::ops::Deref;

use crypto::hashes::Digest;
use identity_core::common::Object;
use identity_core::convert::ToJson;
use identity_credential::credential::{Credential, Jws, Jwt};
use identity_credential::presentation::{JwtPresentationOptions, Presentation};
use identity_did::{DIDUrl, DID};
use identity_document::document::{self, CoreDocument};
use identity_verification::jws::{CharSet, CompactJwsEncoder, CompactJwsEncodingOptions, JwsHeader};
use identity_verification::{jwk::Jwk, jws::JwsAlgorithm, CustomMethodData, MethodBuilder, MethodScope, MethodType, VerificationMethod};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use crate::{JwsSignatureOptions, KeyIdStorageError, KeyIdStorageErrorKind};
use crate::{try_undo_key_generation, JwkGenOutput, JwkMemStore, JwkStorage, JwkStoragePQ, KeyId, KeyIdStorage, KeyType, MethodDigest, Storage};
use async_trait::async_trait;
use super::JwkStorageDocumentError as Error;
use identity_verification::{CompositeAlgId, MethodData};
use identity_verification::CompositePublicKey;

pub type StorageResultHybrid<T> = Result<T, Error>;




macro_rules! generate_method_hybrid_for_document_type {
    ($t:ty, $name:ident) => {
async fn $name<K, I>(
    document: &mut $t,
    storage: &Storage<K, I>,
    alg_id: CompositeAlgId,
    fragment: Option<&str>,
    scope: MethodScope,
  ) -> StorageResultHybrid<String>
  where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage,
{
    let (pq_key_type, pq_alg, trad_key_type, trad_alg) = match alg_id {
        CompositeAlgId::IdMldsa44Ed25519Sha512 => (
            JwkMemStore::ML_DSA_KEY_TYPE, JwsAlgorithm::ML_DSA_44, JwkMemStore::ED25519_KEY_TYPE, JwsAlgorithm::EdDSA
        ),
        CompositeAlgId::IdMldsa65Ed25519Sha512 => (
            JwkMemStore::ML_DSA_KEY_TYPE, JwsAlgorithm::ML_DSA_65, JwkMemStore::ED25519_KEY_TYPE, JwsAlgorithm::EdDSA
        ),
    };

    let JwkGenOutput { key_id: t_key_id, jwk: t_jwk } = K::generate(storage.key_storage(), trad_key_type, trad_alg)
    .await
    .map_err(Error::KeyStorageError)?;

    let JwkGenOutput { key_id: pq_key_id, jwk: pq_jwk } = K::generate_pq_key(storage.key_storage(), pq_key_type, pq_alg)
    .await
    .map_err(Error::KeyStorageError)?;

    // Can i use ~ in a URI safely since is an unreserved character (https://www.rfc-editor.org/rfc/rfc3986#section-2.3)
    let composite_fragment =  t_jwk.kid().map(|s| s.to_string()).or_else(|| pq_jwk.kid().map(|s| s.to_string())).map(|s| {
        if let (Some(str1), Some(str2)) = (t_jwk.kid(), pq_jwk.kid()) {
            format!("{}~{}", str1, str2)
        } else {
            s
        }
    });

    let composite_kid = KeyId::new(format!("{}~{}", t_key_id.as_str(), pq_key_id.as_str()));

    let fragment: Cow<'_, str> = {
        let given_fragment: &str = fragment
          .or_else(|| composite_fragment.as_deref())
          .ok_or(identity_verification::Error::InvalidMethod(
            "an explicit fragment or JWK kid is required",
          ))
          .map_err(Error::VerificationMethodConstructionError)?;
        // Make sure the fragment starts with "#"
        if given_fragment.starts_with('#') {
          Cow::Borrowed(given_fragment)
        } else {
          Cow::Owned(format!("#{given_fragment}"))
        }
      };
  
    let id: DIDUrl = document.id().to_url().join(fragment)
    .map_err(identity_verification::Error::DIDUrlConstructionError)
    .map_err(Error::VerificationMethodConstructionError)?;


    let composite_pk = CompositePublicKey::new(alg_id, t_jwk, pq_jwk );

    // Produce a new verification method containing the generated JWK. If this operation fails we handle the error
    // by attempting to revert key generation before returning an error.
    let method: VerificationMethod = {
        match MethodBuilder::default()
            .id(id)
            .type_(MethodType::custom("CompositeSignaturePublicKey"))
            .controller(document.id().clone().into())
            .data(identity_verification::MethodData::CompositePublicKey(composite_pk))
            .build()
            .map_err(Error::VerificationMethodConstructionError)
        {
            Ok(method) => method,
            Err(source) => {
                let error = try_undo_key_generation(storage, &t_key_id, source).await;
                let error = try_undo_key_generation(storage, &pq_key_id, error).await;
                return Err(error)
            }
        }
    };




    // Extract data from method before inserting it into the DID document.
    let method_digest: MethodDigest = MethodDigest::new(&method).map_err(Error::MethodDigestConstructionError)?;
    let method_id: DIDUrl = method.id().clone();

    // The fragment is always set on a method, so this error will never occur.
    let fragment: String = method_id
    .fragment()
    .ok_or(identity_verification::Error::MissingIdFragment)
    .map_err(Error::VerificationMethodConstructionError)?
    .to_owned();

    // Insert method into document and handle error upon failure.
    if let Err(error) = document
    .insert_method(method, scope)
    .map_err(|_| Error::FragmentAlreadyExists)
    {
        let error = try_undo_key_generation(storage, &t_key_id, error).await;
        let error = try_undo_key_generation(storage, &pq_key_id, error).await;
        return Err(error)
    };

    // Insert the generated `KeyId` into storage under the computed method digest and handle the error if the
    // operation fails.
    if let Err(error) = <I as KeyIdStorage>::insert_key_id(&storage.key_id_storage(), method_digest, composite_kid)
    .await
    .map_err(Error::KeyIdStorageError)
    {
        // Remove the method from the document as it can no longer be used.
        let _ = document.remove_method(&method_id);
        let error = try_undo_key_generation(storage, &t_key_id, error).await;
        let error = try_undo_key_generation(storage, &pq_key_id, error).await;
        return Err(error)
    }

    Ok(fragment)
}
    }
}


///New trait to handle JWP-based operations on DID Documents
#[cfg_attr(not(feature = "send-sync-storage"), async_trait(?Send))]
#[cfg_attr(feature = "send-sync-storage", async_trait)]
pub trait JwkDocumentExtHybrid {

  /// Generate new key material in the given `storage` and insert a new verification method with the corresponding
  /// public key material into the DID document. This support BBS+ keys.
  async fn generate_method_hybrid<K, I>(
    &mut self,
    storage: &Storage<K, I>,
    alg_id: CompositeAlgId,
    fragment: Option<&str>,
    scope: MethodScope,
  ) -> StorageResultHybrid<String>
  where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage;

    async fn create_jws<K, I>(
        &self,
        storage: &Storage<K, I>,
        fragment: &str,
        payload: &[u8],
        options: &JwsSignatureOptions,
      ) -> StorageResultHybrid<Jws>
      where
        K: JwkStorage + JwkStoragePQ,
        I: KeyIdStorage;

    async fn create_credential_jwt_hybrid<K, I, T>(
        &self,
        credential: &Credential<T>,
        storage: &Storage<K, I>,
        fragment: &str,
        options: &JwsSignatureOptions,
        custom_claims: Option<Object>,
        ) -> StorageResultHybrid<Jwt>
        where
        K: JwkStorage + JwkStoragePQ,
        I: KeyIdStorage,
        T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync;


    async fn create_presentation_jwt_hybrid<K, I, CRED, T>(
      &self,
      presentation: &Presentation<CRED, T>,
      storage: &Storage<K, I>,
      fragment: &str,
      signature_options: &JwsSignatureOptions,
      presentation_options: &JwtPresentationOptions,
    ) -> StorageResultHybrid<Jwt>
    where
      K: JwkStorage + JwkStoragePQ,
      I: KeyIdStorage,
      T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync,
      CRED: ToOwned<Owned = CRED> + Serialize + DeserializeOwned + Clone + Sync;

}

generate_method_hybrid_for_document_type!(CoreDocument, generate_method_hybrid_core_document);

#[cfg_attr(not(feature = "send-sync-storage"), async_trait(?Send))]
#[cfg_attr(feature = "send-sync-storage", async_trait)]
impl JwkDocumentExtHybrid for CoreDocument {
  async fn generate_method_hybrid<K, I>(
    &mut self,
    storage: &Storage<K, I>,
    alg_id: CompositeAlgId,
    fragment: Option<&str>,
    scope: MethodScope,
  ) -> StorageResultHybrid<String>
  where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage,
  {
    generate_method_hybrid_core_document(self, storage, alg_id, fragment, scope).await
  }

  async fn create_jws<K, I>(
    &self,
    storage: &Storage<K, I>,
    fragment: &str,
    payload: &[u8],
    options: &JwsSignatureOptions,
  ) -> StorageResultHybrid<Jws>
  where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage {
        // Obtain the method corresponding to the given fragment.
        let method: &VerificationMethod = self.resolve_method(fragment, None).ok_or(Error::MethodNotFound)?;
        let MethodData::CompositePublicKey(ref composite) = method.data() else {
            return Err(Error::NotCompositePublicKey);
        };

        let alg_id = composite.alg_id();
        let t_jwk = composite.traditional_public_key();
        let pq_jwk = composite.pq_public_key();

        // Extract JwsAlgorithm.
        let alg: JwsAlgorithm = alg_id
        .name()
        .parse()
        .map_err(|_| Error::InvalidJwsAlgorithm)?;

        // Create JWS header in accordance with options.
        let header: JwsHeader = {
        let mut header = JwsHeader::new();

        header.set_alg(alg);
        if let Some(custom) = &options.custom_header_parameters {
            header.set_custom(custom.clone())
        }

        if let Some(ref kid) = options.kid {
            header.set_kid(kid.clone());
        } else {
            header.set_kid(method.id().to_string());
        }

        if let Some(b64) = options.b64 {
            // Follow recommendation in https://datatracker.ietf.org/doc/html/rfc7797#section-7.
            if !b64 {
            header.set_b64(b64);
            header.set_crit(["b64"]);
            }
        };

        if let Some(typ) = &options.typ {
            header.set_typ(typ.clone())
        } else {
            // https://www.w3.org/TR/vc-data-model/#jwt-encoding
            header.set_typ("JWT")
        }

        if let Some(cty) = &options.cty {
            header.set_cty(cty.clone())
        };

        if let Some(url) = &options.url {
            header.set_url(url.clone())
        };

        if let Some(nonce) = &options.nonce {
            header.set_nonce(nonce.clone())
        };

        header
        };

        // Get the key identifier corresponding to the given method from the KeyId storage.
        let method_digest: MethodDigest = MethodDigest::new(method).map_err(Error::MethodDigestConstructionError)?;
        let key_id: KeyId = <I as KeyIdStorage>::get_key_id(storage.key_id_storage(), &method_digest)
        .await
        .map_err(Error::KeyIdStorageError)?;

        let (t_key_id, pq_key_id) = key_id.as_str().split_once("~")
        .map(|v| (KeyId::new(v.0), KeyId::new(v.1))).ok_or(Error::KeyIdStorageError(KeyIdStorageErrorKind::Unspecified.into()))?;

        // Extract Compact JWS encoding options.
        let encoding_options: CompactJwsEncodingOptions = if !options.detached_payload {
        // We use this as a default and don't provide the extra UrlSafe check for now.
        // Applications that require such checks can easily do so after JWS creation.
        CompactJwsEncodingOptions::NonDetached {
            charset_requirements: CharSet::Default,
        }
        } else {
        CompactJwsEncodingOptions::Detached
        };

        let jws_encoder: CompactJwsEncoder<'_> = CompactJwsEncoder::new_with_options(payload, &header, encoding_options)
        .map_err(|err| Error::EncodingError(err.into()))?;

        let signing_input = match alg {
          JwsAlgorithm::IdMldsa44Ed25519Sha512 => {
            //TODO: hybrid - DER OID
            let mut input = vec![0x06, 0x0B, 0x60, 0x86, 0x48, 0x01, 0x86, 0xFA, 0x6B, 0x50, 0x08, 0x01, 0x03];
            input.extend(crypto::hashes::sha::Sha512::digest(jws_encoder.signing_input()).deref().to_vec());
            input
          },
          JwsAlgorithm::IdMldsa65Ed25519Sha512 => {
            let mut input = vec![0x06, 0x0B, 0x60, 0x86, 0x48, 0x01, 0x86, 0xFA, 0x6B, 0x50, 0x08, 0x01, 0x0A];
            input.extend(crypto::hashes::sha::Sha512::digest(jws_encoder.signing_input()).deref().to_vec());
            input
          },
          _ => return Err(Error::InvalidJwsAlgorithm)
        };

        let signature_t = <K as JwkStorage>::sign(storage.key_storage(), &t_key_id, &signing_input, t_jwk)
        .await
        .map_err(Error::KeyStorageError)?;

        let signature_pq = <K as JwkStoragePQ>::pq_sign(storage.key_storage(), &pq_key_id, &signing_input, pq_jwk)
        .await
        .map_err(Error::KeyStorageError)?;

        let signature = [signature_t, signature_pq].concat();

        Ok(Jws::new(jws_encoder.into_jws(&signature)))
    }

    async fn create_credential_jwt_hybrid<K, I, T>(
        &self,
        credential: &Credential<T>,
        storage: &Storage<K, I>,
        fragment: &str,
        options: &JwsSignatureOptions,
        custom_claims: Option<Object>,
    ) -> StorageResultHybrid<Jwt>
    where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage,
    T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync {
        if options.detached_payload {
            return Err(Error::EncodingError(Box::<dyn std::error::Error + Send + Sync>::from(
              "cannot use detached payload for credential signing",
            )));
          }
      
          if !options.b64.unwrap_or(true) {
            // JWTs should not have `b64` set per https://datatracker.ietf.org/doc/html/rfc7797#section-7.
            return Err(Error::EncodingError(Box::<dyn std::error::Error + Send + Sync>::from(
              "cannot use `b64 = false` with JWTs",
            )));
          }
      
          let payload = credential
            .serialize_jwt(custom_claims)
            .map_err(Error::ClaimsSerializationError)?;
          self
            .create_jws(storage, fragment, payload.as_bytes(), options)
            .await
            .map(|jws| Jwt::new(jws.into()))
    }


    async fn create_presentation_jwt_hybrid<K, I, CRED, T>(
      &self,
      presentation: &Presentation<CRED, T>,
      storage: &Storage<K, I>,
      fragment: &str,
      jws_options: &JwsSignatureOptions,
      jwt_options: &JwtPresentationOptions,
    ) -> StorageResultHybrid<Jwt>
    where
      K: JwkStorage + JwkStoragePQ,
      I: KeyIdStorage,
      T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync,
      CRED: ToOwned<Owned = CRED> + Serialize + DeserializeOwned + Clone + Sync
    {
      if jws_options.detached_payload {
        return Err(Error::EncodingError(Box::<dyn std::error::Error + Send + Sync>::from(
          "cannot use detached payload for presentation signing",
        )));
      }
  
      if !jws_options.b64.unwrap_or(true) {
        // JWTs should not have `b64` set per https://datatracker.ietf.org/doc/html/rfc7797#section-7.
        return Err(Error::EncodingError(Box::<dyn std::error::Error + Send + Sync>::from(
          "cannot use `b64 = false` with JWTs",
        )));
      }
      let payload = presentation
        .serialize_jwt(jwt_options)
        .map_err(Error::ClaimsSerializationError)?;
      self
        .create_jws(storage, fragment, payload.as_bytes(), jws_options)
        .await
        .map(|jws| Jwt::new(jws.into()))
    }
}


// ====================================================================================================================
// IotaDocument
// ====================================================================================================================
#[cfg(feature = "iota-document")]
mod iota_document {
  use crate::StorageResult;

    use super::*;
  use identity_credential::credential::Jwt;
  use identity_iota_core::IotaDocument;

  generate_method_hybrid_for_document_type!(IotaDocument, generate_method_hybrid_iota_document);

  #[cfg_attr(not(feature = "send-sync-storage"), async_trait(?Send))]
  #[cfg_attr(feature = "send-sync-storage", async_trait)]
  impl JwkDocumentExtHybrid for IotaDocument {
    async fn generate_method_hybrid<K, I>(
        &mut self,
        storage: &Storage<K, I>,
        alg_id: CompositeAlgId,
        fragment: Option<&str>,
        scope: MethodScope,
    ) -> StorageResult<String>
    where
      K: JwkStorage + JwkStoragePQ,
      I: KeyIdStorage,
    {
        generate_method_hybrid_iota_document(self, storage, alg_id, fragment, scope).await
    }

    async fn create_jws<K, I>(
        &self,
        storage: &Storage<K, I>,
        fragment: &str,
        payload: &[u8],
        options: &JwsSignatureOptions,
      ) -> StorageResultHybrid<Jws>
      where
        K: JwkStorage + JwkStoragePQ,
        I: KeyIdStorage {
          self
          .core_document()
          .create_jws(storage, fragment, payload, options)
          .await
        }

    async fn create_credential_jwt_hybrid<K, I, T>(
        &self,
        credential: &Credential<T>,
        storage: &Storage<K, I>,
        fragment: &str,
        options: &JwsSignatureOptions,
        custom_claims: Option<Object>,
    ) -> StorageResultHybrid<Jwt>
    where
    K: JwkStorage + JwkStoragePQ,
    I: KeyIdStorage,
    T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync {
      self
      .core_document()
      .create_credential_jwt_hybrid(credential, storage, fragment, options, custom_claims)
      .await
    }

    async fn create_presentation_jwt_hybrid<K, I, CRED, T>(
      &self,
      presentation: &Presentation<CRED, T>,
      storage: &Storage<K, I>,
      fragment: &str,
      options: &JwsSignatureOptions,
      jwt_options: &JwtPresentationOptions,
    ) -> StorageResultHybrid<Jwt>
    where
      K: JwkStorage + JwkStoragePQ,
      I: KeyIdStorage,
      T: ToOwned<Owned = T> + Serialize + DeserializeOwned + Sync,
      CRED: ToOwned<Owned = CRED> + Serialize + DeserializeOwned + Clone + Sync
    {
      self
      .core_document()
      .create_presentation_jwt_hybrid(presentation, storage, fragment, options, jwt_options)
      .await
    }

  }
}

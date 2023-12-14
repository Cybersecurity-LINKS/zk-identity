use identity_core::convert::{FromJson, ToJson};
use identity_did::{DIDUrl, CoreDID};
use identity_document::{document::CoreDocument, verifiable::JwsVerificationOptions};
use jsonprooftoken::jpt::claims::JptClaims;
use jsonprooftoken::jwk::key::Jwk as JwkExt;
use jsonprooftoken::jwp::issued::JwpIssuedVerifier;
use jsonprooftoken::{jwp::issued::JwpIssued, encoding::SerializationType};

use crate::credential::CredentialJwtClaims;
use crate::validator::{JwtCredentialValidatorUtils, JwtCredentialValidationOptions, CompoundCredentialValidationError};
use crate::{credential::{Jpt, Credential}, validator::{FailFast, JwtValidationError, jwt_credential_validation::SignerContext}};

use super::DecodedJptCredential;

/// A type for decoding and validating [`Credential`]s in JPT format. //TODO: validator
#[non_exhaustive]
pub struct JptCredentialValidator;

impl JptCredentialValidator {


    /// Decodes and validates a [`Credential`] issued as a JPT. A [`DecodedJptCredential`] is returned upon success.
    ///
    /// The following properties are validated according to `options`:
    /// - the issuer's proof on the JWP,
    /// - the expiration date,
    /// - the issuance date,
    /// - the semantic structure.
    pub fn validate<DOC, T>(
        credential_jpt: &Jpt, //TODO: the validation process could be handled both for JWT and JPT by the same function, the function could recognise if the token in input is a JWT or JPT based on the typ field
        issuer: &DOC,
        options: &JwtCredentialValidationOptions,
        fail_fast: FailFast,
      ) -> Result<DecodedJptCredential<T>, CompoundCredentialValidationError>
      where
        T: ToOwned<Owned = T> + serde::Serialize + serde::de::DeserializeOwned,
        DOC: AsRef<CoreDocument>,
      {
        Self::validate_extended::<CoreDocument, T>(
          credential_jpt,
          std::slice::from_ref(issuer.as_ref()),
          options,
          fail_fast,
        )
      }




  // This method takes a slice of issuer's instead of a single issuer in order to better accommodate presentation
  // validation. It also validates the relationship between a holder and the credential subjects when
  // `relationship_criterion` is Some.
  pub(crate) fn validate_extended<DOC, T>(
    credential: &Jpt,
    issuers: &[DOC],
    options: &JwtCredentialValidationOptions,
    fail_fast: FailFast,
  ) -> Result<DecodedJptCredential<T>, CompoundCredentialValidationError>
  where
    T: ToOwned<Owned = T> + serde::Serialize + serde::de::DeserializeOwned,
    DOC: AsRef<CoreDocument>,
  {
    // First verify the JWS signature and decode the result into a credential token, then apply all other validations.
    // If this errors we have to return early regardless of the `fail_fast` flag as all other validations require a
    // `&Credential`.
    let credential_token =
      Self::verify_proof(credential, issuers, &options.verification_options)
        .map_err(|err| CompoundCredentialValidationError {
          validation_errors: [err].into(),
        })?;

    let credential: &Credential<T> = &credential_token.credential;

    // Run all single concern Credential validations in turn and fail immediately if `fail_fast` is true.

    let expiry_date_validation = std::iter::once_with(|| {
      JwtCredentialValidatorUtils::check_expires_on_or_after(
        &credential_token.credential,
        options.earliest_expiry_date.unwrap_or_default(),
      )
    });

    let issuance_date_validation = std::iter::once_with(|| {
      JwtCredentialValidatorUtils::check_issued_on_or_before(
        credential,
        options.latest_issuance_date.unwrap_or_default(),
      )
    });

    let structure_validation = std::iter::once_with(|| JwtCredentialValidatorUtils::check_structure(credential));

    let subject_holder_validation = std::iter::once_with(|| {
      options
        .subject_holder_relationship
        .as_ref()
        .map(|(holder, relationship)| {
          JwtCredentialValidatorUtils::check_subject_holder_relationship(credential, holder, *relationship)
        })
        .unwrap_or(Ok(()))
    });

    let validation_units_iter = issuance_date_validation
      .chain(expiry_date_validation)
      .chain(structure_validation)
      .chain(subject_holder_validation);

    #[cfg(feature = "revocation-bitmap")]
    let validation_units_iter = {
      let revocation_validation =
        std::iter::once_with(|| JwtCredentialValidatorUtils::check_status(credential, issuers, options.status));
      validation_units_iter.chain(revocation_validation)
    };

    let validation_units_error_iter = validation_units_iter.filter_map(|result| result.err());
    let validation_errors: Vec<JwtValidationError> = match fail_fast {
      FailFast::FirstError => validation_units_error_iter.take(1).collect(),
      FailFast::AllErrors => validation_units_error_iter.collect(),
    };

    if validation_errors.is_empty() {
      Ok(credential_token)
    } else {
      Err(CompoundCredentialValidationError { validation_errors })
    }
  }



/// Stateless version of [`Self::verify_signature`]
fn verify_proof<DOC, T>(
    credential: &Jpt,
    trusted_issuers: &[DOC],
    options: &JwsVerificationOptions,
  ) -> Result<DecodedJptCredential<T>, JwtValidationError>
  where
    T: ToOwned<Owned = T> + serde::Serialize + serde::de::DeserializeOwned,
    DOC: AsRef<CoreDocument>,
  {

    let decoded = JwpIssuedVerifier::decode(credential.as_str(), SerializationType::COMPACT).map_err(|err| JwtValidationError::JwpDecodingError(err))?;

    // If no method_url is set, parse the `kid` to a DID Url which should be the identifier
    // of a verification method in a trusted issuer's DID document.
    let method_id: DIDUrl = match &options.method_id {
      Some(method_id) => method_id.clone(),
      None => {
        let kid: &str = decoded.get_header().kid().ok_or(
          JwtValidationError::MethodDataLookupError {
            source: None,
            message: "could not extract kid from protected header",
            signer_ctx: SignerContext::Issuer,
          },
        )?;

        // Convert kid to DIDUrl
        DIDUrl::parse(kid).map_err(|err| JwtValidationError::MethodDataLookupError {
          source: Some(err.into()),
          message: "could not parse kid as a DID Url",
          signer_ctx: SignerContext::Issuer,
        })?
      }
    };

    // locate the corresponding issuer
    let issuer: &CoreDocument = trusted_issuers
      .iter()
      .map(AsRef::as_ref)
      .find(|issuer_doc| <CoreDocument>::id(issuer_doc) == method_id.did())
      .ok_or(JwtValidationError::DocumentMismatch(SignerContext::Issuer))?;

    // Obtain the public key from the issuer's DID document
    let public_key: JwkExt = issuer
      .resolve_method(&method_id, options.method_scope)
      .and_then(|method| method.data().public_key_jwk())
      .and_then(|k| k.try_into().ok()) //Conversio into jsonprooftoken::Jwk type
      .ok_or_else(|| JwtValidationError::MethodDataLookupError {
        source: None,
        message: "could not extract JWK from a method identified by kid",
        signer_ctx: SignerContext::Issuer,
      })?;

        
    let credential_token = Self::verify_decoded_jwp(decoded, &public_key)?;

    // Check that the DID component of the parsed `kid` does indeed correspond to the issuer in the credential before
    // returning.
    let issuer_id: CoreDID = JwtCredentialValidatorUtils::extract_issuer(&credential_token.credential)?;
    if &issuer_id != method_id.did() {
      return Err(JwtValidationError::IdentifierMismatch {
        signer_ctx: SignerContext::Issuer,
      });
    };
    Ok(credential_token)
  }


  /// Verify the signature using the given `public_key` and `signature_verifier`.
  fn verify_decoded_jwp<T>(
    decoded: JwpIssuedVerifier,
    public_key: &JwkExt,
  ) -> Result<DecodedJptCredential<T>, JwtValidationError>
  where
    T: ToOwned<Owned = T> + serde::Serialize + serde::de::DeserializeOwned,
  {

    //Verify Jwp proof
    let decoded_jwp = decoded.verify(&public_key).map_err(|err| JwtValidationError::JwpProofVerifiationError(err))?;

    let claims = decoded_jwp.get_claims().ok_or("Claims not present").map_err(|err| JwtValidationError::CredentialStructure(crate::Error::JptClaimsSetDeserializationError(err.into())))?;
    let payloads = decoded_jwp.get_payloads();
    let jpt_claims = JptClaims::from_claims_and_payloads(&claims, payloads);
    let jpt_claims_json = jpt_claims.to_json_vec().map_err(|err| JwtValidationError::CredentialStructure(crate::Error::JptClaimsSetDeserializationError(err.into())))?;
    

    // Deserialize the raw claims
    let credential_claims: CredentialJwtClaims<'_, T> =
      CredentialJwtClaims::from_json_slice(&jpt_claims_json).map_err(|err| {
        JwtValidationError::CredentialStructure(crate::Error::JwtClaimsSetDeserializationError(err.into()))
      })?;

    let custom_claims = credential_claims.custom.clone();

    // Construct the credential token containing the credential and the protected header.
    let credential: Credential<T> = credential_claims
      .try_into_credential()
      .map_err(JwtValidationError::CredentialStructure)?;
      
    Ok(DecodedJptCredential {
      credential,
      custom_claims,
      decoded_jwp
    })
  }

}
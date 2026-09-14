//! JWS verification for Homebrew API files (`docs/COMPAT.md` 1.2).
//!
//! `verify_and_extract_payload(json_bytes) -> Result<String>` parses the
//! wrapper, finds the `homebrew-1` signature, checks `alg == "PS512"` and
//! `b64 == false`, verifies RSA-PSS/SHA-512 (MGF1-SHA-512, salt length 64)
//! over `<protected>.<payload>` and returns the payload string untouched.
//! Errors use Homebrew's reason strings: "key not found", "invalid
//! algorithm", "signature mismatch".
//!
//! Port of `Homebrew::API.verify_and_parse_jws` in `Library/Homebrew/api.rb`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::RsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::pss::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use serde::Deserialize;
use sha2::Sha512;

use crate::error::{Error, Result};

use super::{HOMEBREW_JWS_PUBLIC_KEY_PEM, JWS_KEY_ID};

/// A signature verification failure, carrying Homebrew's short reason string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwsError(pub &'static str);

impl std::fmt::Display for JwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl From<JwsError> for Error {
    fn from(e: JwsError) -> Self {
        Error::user(e.0)
    }
}

#[derive(Deserialize)]
struct Wrapper {
    #[serde(default)]
    payload: String,
    #[serde(default)]
    signatures: Vec<SignatureEntry>,
}

#[derive(Deserialize)]
struct SignatureEntry {
    #[serde(default)]
    protected: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    header: Option<SignatureHeader>,
}

#[derive(Deserialize)]
struct SignatureHeader {
    #[serde(default)]
    kid: Option<String>,
}

/// `Homebrew::API.urlsafe_decode64`: base64url with optional padding.
fn urlsafe_decode64(value: &str) -> std::result::Result<Vec<u8>, JwsError> {
    let trimmed = value.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed.as_bytes())
        .map_err(|_| JwsError("invalid algorithm"))
}

/// Verify the JWS envelope and return the payload string.
///
/// Returns `Err(Error::User(reason))` with Homebrew's wording for the reason
/// (`key not found`, `invalid algorithm`, `signature mismatch`); callers wrap
/// it in the "Failed to verify integrity" message.
pub fn verify_and_extract_payload(json: &[u8]) -> Result<String> {
    Ok(verify(json)?)
}

/// Like [`verify_and_extract_payload`] but with the typed reason.
pub fn verify(json: &[u8]) -> std::result::Result<String, JwsError> {
    let wrapper: Wrapper =
        serde_json::from_slice(json).map_err(|_| JwsError("malformed JWS envelope"))?;
    let signature = wrapper
        .signatures
        .iter()
        .find(|s| {
            s.header
                .as_ref()
                .and_then(|h| h.kid.as_deref())
                .is_some_and(|kid| kid == JWS_KEY_ID)
        })
        .ok_or(JwsError("key not found"))?;

    verify_signature(&signature.protected, &signature.signature, &wrapper.payload)?;
    Ok(wrapper.payload)
}

/// Port of `Homebrew::API.verify_jws_signature`.
pub fn verify_signature(
    protected_b64: &str,
    signature_b64: &str,
    payload: &str,
) -> std::result::Result<(), JwsError> {
    let header_bytes = urlsafe_decode64(protected_b64)?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|_| JwsError("invalid algorithm"))?;
    if !header.is_object()
        || header.get("alg").and_then(|v| v.as_str()) != Some("PS512")
        // NOTE: a missing `b64` means true in JWS, which we must reject.
        || header.get("b64").and_then(|v| v.as_bool()) != Some(false)
    {
        return Err(JwsError("invalid algorithm"));
    }

    let sig_bytes = urlsafe_decode64(signature_b64)?;
    let signature =
        Signature::try_from(sig_bytes.as_slice()).map_err(|_| JwsError("signature mismatch"))?;

    let key = RsaPublicKey::from_public_key_pem(HOMEBREW_JWS_PUBLIC_KEY_PEM)
        .map_err(|_| JwsError("signature mismatch"))?;
    // `VerifyingKey::<Sha512>::new` uses MGF1-SHA-512 and a salt length equal
    // to the digest length (64), matching `verify_pss(salt_length: :digest)`.
    let verifying_key = VerifyingKey::<Sha512>::new(key);

    let mut message = Vec::with_capacity(protected_b64.len() + 1 + payload.len());
    message.extend_from_slice(protected_b64.as_bytes());
    message.push(b'.');
    message.extend_from_slice(payload.as_bytes());

    verifying_key
        .verify(&message, &signature)
        .map_err(|_| JwsError("signature mismatch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_key_id() {
        let json = br#"{"payload":"{}","signatures":[{"protected":"e30","signature":"","header":{"kid":"other"}}]}"#;
        assert_eq!(verify(json).unwrap_err(), JwsError("key not found"));
    }

    #[test]
    fn rejects_wrong_algorithm() {
        // {"alg":"RS256","b64":false}
        let protected = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","b64":false}"#);
        let json = format!(
            r#"{{"payload":"{{}}","signatures":[{{"protected":"{protected}","signature":"AA","header":{{"kid":"homebrew-1"}}}}]}}"#
        );
        assert_eq!(
            verify(json.as_bytes()).unwrap_err(),
            JwsError("invalid algorithm")
        );
    }

    #[test]
    fn rejects_b64_true() {
        let protected = URL_SAFE_NO_PAD.encode(br#"{"alg":"PS512"}"#);
        let json = format!(
            r#"{{"payload":"{{}}","signatures":[{{"protected":"{protected}","signature":"AA","header":{{"kid":"homebrew-1"}}}}]}}"#
        );
        assert_eq!(
            verify(json.as_bytes()).unwrap_err(),
            JwsError("invalid algorithm")
        );
    }

    #[test]
    fn rejects_bad_signature() {
        let protected = URL_SAFE_NO_PAD.encode(br#"{"alg":"PS512","b64":false,"crit":["b64"]}"#);
        let signature = URL_SAFE_NO_PAD.encode([0u8; 512]);
        let json = format!(
            r#"{{"payload":"{{}}","signatures":[{{"protected":"{protected}","signature":"{signature}","header":{{"kid":"homebrew-1"}}}}]}}"#
        );
        assert_eq!(
            verify(json.as_bytes()).unwrap_err(),
            JwsError("signature mismatch")
        );
    }

    /// Verifies the real cached API file when the sandbox has one (seeded by
    /// `scripts/sandbox.sh` from the host's Homebrew cache).
    #[test]
    fn verifies_real_cached_packages_file() {
        let Some(path) = crate::api::fetch::sandbox_packages_file_for_tests() else {
            eprintln!("no cached packages file; skipping");
            return;
        };
        let bytes = std::fs::read(&path).expect("read cached packages file");
        let payload = verify(&bytes).expect("real Homebrew JWS verifies");
        assert!(payload.starts_with('{'));
        assert!(payload.contains("\"formulae\""));
    }
}

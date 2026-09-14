//! JWS verification for Homebrew API files (`docs/COMPAT.md` 1.2).
//!
//! `verify_and_extract_payload(json_bytes) -> Result<String>` parses the
//! wrapper, finds the `homebrew-1` signature, checks `alg == "PS512"` and
//! `b64 == false`, verifies RSA-PSS/SHA-512 (MGF1-SHA-512, salt length 64)
//! over `<protected>.<payload>` and returns the payload string untouched.
//! Errors use Homebrew's reason strings: "key not found", "invalid
//! algorithm", "signature mismatch".

use crate::error::Result;

pub fn verify_and_extract_payload(_json: &[u8]) -> Result<String> {
    todo!("api::jws::verify_and_extract_payload")
}

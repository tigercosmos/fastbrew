//! Homebrew JSON API: download, JWS verification, caching and the fast index.
//!
//! Public surface used by the rest of the crate:
//!
//! - [`fetch::fetch_packages`]: conditional download of
//!   `internal/packages.<tag>.jws.json` into `$CACHE/api/internal/`, verifying
//!   the signature (`jws`) before replacing the cached file. Returns whether
//!   the file changed.
//! - [`index::Index::load`]: memory-map the fast index for the current bottle
//!   tag, rebuilding it from the cached JWS file when missing or stale.
//! - [`index::Index`] lookups: `formula(name)`, `cask(token)`, alias/rename
//!   maps, `all_formulae()`, `all_casks()`, `search_names`, `search_desc`.
//!
//! See `docs/DESIGN.md` 4.1 and `docs/COMPAT.md` 1.

pub mod fetch;
pub mod index;
pub mod jws;

pub const INTERNAL_PACKAGES_ENDPOINT_PREFIX: &str = "internal/packages.";
pub const JWS_KEY_ID: &str = "homebrew-1";

/// Public key used to verify Homebrew's JWS payloads (`api/homebrew-1.pem`).
pub const HOMEBREW_JWS_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAyKoOYzp1rhwXISRi61BY
XBEr2PalSK8lEVOL2USy7mpy0OubOlFyujawyQcBcCn+uPOJ/WaK+POhNWcLLoiK
L2m8GViaQm7SMwdLKUXFgKSPHcG/1m6Vu+TNBKTfQqT60PjEYIrn5NW9ZrM0cUhK
REmsbeAMBevdSaW9UwY9iIhprrgovvT8SzKhF8ZOIZKXfJX4VNk0y/7VJYNuGGqH
3npxV7OKd4yTGRGqFcC9kJ84me3thiu0yqlOjASmfWIwIwcfp4j6BEM2LuqKd7yX
h51/O+MTthkuxV36moDKfdgdOFsvlCFkziaYLScCX9lOlmZHtOfJTAOXxTmM7qGr
wTGK0vhvTi8k9dBmH/dccredQBtPOfM/FEdeyakGLoTcDguiBS/4El3I2KtF6B2h
OGoBumR915/cI4drr5yPMduZ7gjs7ZEZnVkeVzic24TfUHpnOYzrhucNJtHMBDj9
6d1Gk82AhtuF9KlusLmCb6qXCWQSp/A4RZpN37E/p9q8rLp/7B/zp8X2TVvecPNy
BdMagdktdEqK7WPlYMcUp56JaOph8vqYoU+oGyCpWoLvcXFb75o4eefuu6Rs5SyM
c9JCCJ0DDFPjCRFnGPkvsKxFCzMFqH1jpWH0RQIrgmNVM5PO84iRH9YJsSPQzpMj
KvK/ZH4YgR9wNkBNagFo7lsCAwEAAQ==
-----END PUBLIC KEY-----
";

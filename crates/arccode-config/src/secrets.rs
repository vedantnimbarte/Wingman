//! Keyring-backed secret storage for provider API keys.
//!
//! Keys are stored under service name `"arccode"`, username = stable provider
//! id (e.g. `"anthropic"`, `"openai"`). The TUI's `/login` wizard writes
//! here; runtime key resolution reads here first, before falling back to
//! plaintext config or env vars.
//!
//! Errors are intentionally swallowed-to-Option in [`load`]: a keyring miss
//! is a normal "not configured" state, not a failure. Hard errors (e.g.
//! keyring service unavailable) are surfaced from [`store`] and [`delete`]
//! where the caller cares.

use keyring::Entry;

const SERVICE: &str = "arccode";

/// Store an API key for `provider_id` in the OS keyring.
pub fn store(provider_id: &str, api_key: &str) -> Result<(), SecretError> {
    let entry = Entry::new(SERVICE, provider_id).map_err(SecretError::from)?;
    entry.set_password(api_key).map_err(SecretError::from)
}

/// Load an API key for `provider_id`, returning `None` if no entry exists.
///
/// Other keyring errors (service unavailable, permission denied) are
/// surfaced; only "no entry" maps to `Ok(None)`.
pub fn load(provider_id: &str) -> Result<Option<String>, SecretError> {
    let entry = match Entry::new(SERVICE, provider_id) {
        Ok(e) => e,
        Err(e) => return Err(SecretError::from(e)),
    };
    match entry.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(SecretError::from(e)),
    }
}

/// Delete the entry for `provider_id`. Returns `Ok(())` if the entry was
/// removed *or* did not exist — idempotent by design.
pub fn delete(provider_id: &str) -> Result<(), SecretError> {
    let entry = match Entry::new(SERVICE, provider_id) {
        Ok(e) => e,
        Err(e) => return Err(SecretError::from(e)),
    };
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SecretError::from(e)),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("keyring: {0}")]
pub struct SecretError(#[from] pub keyring::Error);

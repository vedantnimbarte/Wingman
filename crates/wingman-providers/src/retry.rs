//! Bounded retry with backoff for provider HTTP requests.
//!
//! Providers previously returned an error on the first non-2xx response, so a
//! routine `429 Too Many Requests` / `overloaded_error` / transient 5xx killed
//! the user's turn. This wraps the send so those are retried a few times with
//! exponential backoff (honoring `Retry-After` when present) before giving up.

use std::future::Future;
use std::time::Duration;

use wingman_core::WingmanError;

const MAX_ATTEMPTS: u32 = 4;
const BASE_DELAY_SECS: f64 = 0.5;
const MAX_DELAY_SECS: f64 = 8.0;

fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff for `attempt` (1-based), capped.
fn backoff_secs(attempt: u32) -> f64 {
    (BASE_DELAY_SECS * 2f64.powi(attempt as i32 - 1)).min(MAX_DELAY_SECS)
}

/// Parse a `Retry-After` header expressed as an integer number of seconds.
/// (The HTTP-date form is uncommon for these APIs and falls back to backoff.)
fn retry_after_secs(resp: &reqwest::Response) -> Option<f64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|s| s.clamp(0.0, 60.0))
}

/// Send an HTTP request built by `build_and_send`, retrying on 429/5xx and
/// transient connect/timeout errors. `build_and_send` must build a *fresh*
/// request each call (a `RequestBuilder` is single-use). Returns the final
/// response — including a non-2xx one once retries are exhausted, so the caller
/// still surfaces the provider's error body.
pub(crate) async fn send_with_retry<F, Fut>(
    label: &str,
    mut build_and_send: F,
) -> Result<reqwest::Response, WingmanError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match build_and_send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success()
                    || attempt >= MAX_ATTEMPTS
                    || !is_retryable_status(status.as_u16())
                {
                    return Ok(resp);
                }
                let delay = retry_after_secs(&resp).unwrap_or_else(|| backoff_secs(attempt));
                tracing::warn!(
                    target: "wingman::retry",
                    "{label} {status}; retry {attempt}/{MAX_ATTEMPTS} in {delay:.1}s"
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
            Err(e) => {
                // Retry only transient network failures; a malformed request or
                // TLS/DNS error won't fix itself.
                if attempt >= MAX_ATTEMPTS || !(e.is_timeout() || e.is_connect() || e.is_request()) {
                    return Err(WingmanError::Provider(format!("{label} request: {e}")));
                }
                let delay = backoff_secs(attempt);
                tracing::warn!(
                    target: "wingman::retry",
                    "{label} network error ({e}); retry {attempt}/{MAX_ATTEMPTS} in {delay:.1}s"
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        for c in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(c), "{c} should retry");
        }
        for c in [200, 400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(c), "{c} should not retry");
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!((backoff_secs(1) - 0.5).abs() < 1e-9);
        assert!((backoff_secs(2) - 1.0).abs() < 1e-9);
        assert!(backoff_secs(10) <= MAX_DELAY_SECS + 1e-9);
    }
}

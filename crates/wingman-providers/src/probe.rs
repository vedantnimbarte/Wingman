//! Minimal "is this provider/model reachable with this key?" check.
//!
//! Used by the TUI `/login` wizard to validate a freshly-entered API key
//! before saving it. Sends the smallest legal completion request the
//! provider accepts and considers the probe successful as soon as **any**
//! event comes back — we don't care what the model says, only that the
//! request didn't 4xx.

use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;
use wingman_core::{CompletionRequest, Message, Provider};

/// Send a tiny request to `provider` using `model`. Returns `Ok(())` if the
/// provider produces at least one stream event without erroring; otherwise
/// returns a human-readable error.
///
/// Uses separate timeouts for the initial HTTP request/response phase (30s)
/// and the first stream event read (30s). The combined worst-case is 60s
/// but each phase is individually bounded so a hung connection is detected
/// quickly.
pub async fn probe(provider: &dyn Provider, model: &str) -> Result<(), String> {
    let req = CompletionRequest {
        max_tokens: 8,
        messages: vec![Message::user_text("ping")],
        ..CompletionRequest::new(model)
    };

    let mut stream = match timeout(Duration::from_secs(30), provider.complete(req)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("{e}")),
        Err(_) => return Err("connection timed out — check network / firewall".to_string()),
    };

    match timeout(Duration::from_secs(30), stream.next()).await {
        Ok(Some(Ok(_))) => Ok(()),
        Ok(Some(Err(e))) => Err(format!("{e}")),
        Ok(None) => Err("provider returned an empty response stream".to_string()),
        Err(_) => Err("provider connected but produced no output within 30s".to_string()),
    }
}

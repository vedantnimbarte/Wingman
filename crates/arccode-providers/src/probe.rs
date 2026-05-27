//! Minimal "is this provider/model reachable with this key?" check.
//!
//! Used by the TUI `/login` wizard to validate a freshly-entered API key
//! before saving it. Sends the smallest legal completion request the
//! provider accepts and considers the probe successful as soon as **any**
//! event comes back — we don't care what the model says, only that the
//! request didn't 4xx.

use std::time::Duration;

use arccode_core::{CompletionRequest, Message, Provider};
use futures::StreamExt;
use tokio::time::timeout;

/// Send a tiny request to `provider` using `model`. Returns `Ok(())` if the
/// provider produces at least one stream event without erroring; otherwise
/// returns a human-readable error.
///
/// Bounded by a 20s timeout so a hung connection can't freeze the wizard.
pub async fn probe(provider: &dyn Provider, model: &str) -> Result<(), String> {
    let req = CompletionRequest {
        max_tokens: 8,
        messages: vec![Message::user_text("ping")],
        ..CompletionRequest::new(model)
    };

    let fut = async move {
        let mut stream = provider
            .complete(req)
            .await
            .map_err(|e| format!("{e}"))?;
        match stream.next().await {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(format!("{e}")),
            None => Err("provider returned an empty stream".to_string()),
        }
    };

    match timeout(Duration::from_secs(20), fut).await {
        Ok(res) => res,
        Err(_) => Err("probe timed out after 20s".to_string()),
    }
}

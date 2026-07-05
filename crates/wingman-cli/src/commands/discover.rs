//! `wingman discover` — probe localhost OpenAI-compatible endpoints for
//! running models. Reports Ollama (11434), LM Studio (1234), and vLLM
//! (8000) by hitting `/v1/models`.

use anyhow::Result;
use std::process::ExitCode;
use std::time::Duration;

const TARGETS: &[(&str, &str)] = &[
    ("ollama", "http://127.0.0.1:11434/v1/models"),
    ("lmstudio", "http://127.0.0.1:1234/v1/models"),
    ("vllm", "http://127.0.0.1:8000/v1/models"),
    ("llamacpp", "http://127.0.0.1:8080/v1/models"),
    ("tgi", "http://127.0.0.1:3000/v1/models"),
    ("litellm", "http://127.0.0.1:4000/v1/models"),
    ("gpt4all", "http://127.0.0.1:4891/v1/models"),
    ("jan", "http://127.0.0.1:1337/v1/models"),
    ("koboldcpp", "http://127.0.0.1:5001/v1/models"),
    ("oobabooga", "http://127.0.0.1:5000/v1/models"),
    ("aphrodite", "http://127.0.0.1:2242/v1/models"),
    // LocalAI / mlx-lm / Mistral.rs all default to ports that clash with
    // earlier entries; the user picks one and overrides via base_url.
];

pub async fn run() -> Result<ExitCode> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()?;
    let mut any = false;
    for (provider, url) in TARGETS {
        match client.get(*url).send().await {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value =
                    r.json().await.unwrap_or_else(|_| serde_json::json!({}));
                let ids: Vec<String> = json
                    .get("data")
                    .and_then(|d| d.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m.get("id").and_then(|s| s.as_str()).map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                println!("✓ {provider} ({url})");
                if ids.is_empty() {
                    println!("    (no models reported)");
                } else {
                    for id in ids {
                        println!("    - {id}");
                    }
                }
                any = true;
            }
            Ok(r) => {
                println!("- {provider} ({url}) — http {}", r.status());
            }
            Err(_) => {
                println!("- {provider} ({url}) — not reachable");
            }
        }
    }
    if !any {
        println!(
            "\nwingman: no local OpenAI-compatible servers found. Start one and add it to config:\n\
             [providers.ollama]   base_url = \"http://localhost:11434/v1\"\n\
             [providers.lmstudio] base_url = \"http://localhost:1234/v1\"\n\
             [providers.vllm]     base_url = \"http://localhost:8000/v1\""
        );
    }
    Ok(ExitCode::SUCCESS)
}

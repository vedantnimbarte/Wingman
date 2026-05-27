//! `arccode cost` — print per-model token usage and estimated USD spend.

use anyhow::Result;
use arccode_core::{pricing::price_for, Usage};
use std::collections::BTreeMap;
use std::process::ExitCode;

pub async fn run(json: bool) -> Result<ExitCode> {
    let totals = load_totals();
    if totals.is_empty() {
        eprintln!("arccode: no usage data yet (~/.arccode/usage.json is empty or missing)");
        return Ok(ExitCode::SUCCESS);
    }

    let mut grand_cost = 0.0f64;
    let mut grand_in = 0u64;
    let mut grand_out = 0u64;
    let mut rows: Vec<(String, Usage, Option<f64>)> = Vec::new();
    for (key, u) in &totals {
        let cost = price_for(key).map(|p| p.cost(u));
        if let Some(c) = cost {
            grand_cost += c;
        }
        grand_in += u.input_tokens as u64 + u.cache_read_input_tokens as u64;
        grand_out += u.output_tokens as u64;
        rows.push((key.clone(), *u, cost));
    }

    if json {
        let payload: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(k, u, c)| {
                serde_json::json!({
                    "key": k,
                    "input_tokens": u.input_tokens,
                    "output_tokens": u.output_tokens,
                    "cache_read_tokens": u.cache_read_input_tokens,
                    "cache_write_tokens": u.cache_creation_input_tokens,
                    "usd": c,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "rows": payload,
                "total_usd": grand_cost,
                "total_input_tokens": grand_in,
                "total_output_tokens": grand_out,
            }))?
        );
    } else {
        println!(
            "{:<48}  {:>10}  {:>10}  {:>10}  {:>10}  {:>9}",
            "model", "input", "output", "cache-r", "cache-w", "usd"
        );
        println!("{}", "-".repeat(48 + 4 + 10 * 4 + 4 + 9 + 5));
        for (k, u, c) in rows {
            println!(
                "{:<48}  {:>10}  {:>10}  {:>10}  {:>10}  {}",
                truncate(&k, 48),
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
                u.cache_creation_input_tokens,
                match c {
                    Some(v) => format!("${v:>8.4}"),
                    None => "       —".to_string(),
                }
            );
        }
        println!();
        println!("total estimated spend: ${grand_cost:.4}");
        println!("total input tokens   : {grand_in}");
        println!("total output tokens  : {grand_out}");
    }
    Ok(ExitCode::SUCCESS)
}

fn load_totals() -> BTreeMap<String, Usage> {
    let path = match arccode_config::ensure_global_dir() {
        Ok(d) => d.join("usage.json"),
        Err(_) => return BTreeMap::new(),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

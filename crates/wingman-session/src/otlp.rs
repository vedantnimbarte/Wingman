//! OpenTelemetry export over OTLP/HTTP JSON (`[telemetry.otlp]`).
//!
//! Rides the [`ContextFact`]s the loop already hands the session log, so there
//! is no second event path to keep in step: a user turn becomes a span, each
//! tool call a child span, and every flush also sends delta counters for
//! tokens, estimated cost, tool calls and verified-done turns.
//!
//! What never leaves: prompts, assistant text, tool input and tool output. A
//! span carries names, counts, durations and verdicts only, so there is nothing
//! in it for the audit log's secret redaction to scrub.
//!
//! Export must never cost a turn anything. Spans go into a bounded queue with
//! `try_send`; when it is full (the collector is slow or down) the span is
//! dropped and counted, and the count is itself exported as
//! `wingman.telemetry.dropped_spans`. A background task flushes every few
//! seconds, and [`shutdown`] gives the last batch a short, bounded chance at
//! process exit.
//!
//! Plain `reqwest` + `serde_json` rather than the `opentelemetry` crates: two
//! signal types over one transport is a few JSON builders, not an SDK.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use wingman_config::Config;
use wingman_core::{ContentBlock, ContextFact, Usage};

/// Spans held while the collector is unreachable before new ones are dropped.
const QUEUE: usize = 2048;
/// Flush early once this many spans are waiting.
const MAX_BATCH: usize = 256;
const FLUSH_EVERY: Duration = Duration::from_secs(5);
const POST_TIMEOUT: Duration = Duration::from_secs(5);
/// Top-level sessions route under `wingman_learn::stats::SESSION_CLASS`.
/// Subagents carry real task classes but have no context sink, so they are
/// not exported at all.
// ponytail: fixed class; thread the routed class through the sink when
// subagent turns are exported.
const TASK_CLASS: &str = "default";

/// Resolved exporter settings. Header values are secrets: no `Debug`.
pub struct Settings {
    /// Base URL without a trailing slash; `/v1/traces` is appended.
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub service_name: String,
}

/// Resolve `[telemetry.otlp]` plus environment overrides into settings.
///
/// `Ok(None)` means export is off. Precedence is `WINGMAN_OTLP_*` over the
/// standard `OTEL_EXPORTER_OTLP_*` over config, for both the endpoint and
/// (per header name) the headers. `Err` is a configuration the exporter
/// refuses: not a URL, a header that cannot be sent, an unresolvable secret,
/// or a non-local endpoint under `[privacy].local_only`.
pub fn settings(
    cfg: &Config,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Settings>, String> {
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    let Some(endpoint) = nonempty(env("WINGMAN_OTLP_ENDPOINT"))
        .or_else(|| nonempty(env("OTEL_EXPORTER_OTLP_ENDPOINT")))
        .or_else(|| nonempty(cfg.telemetry.otlp.endpoint.clone()))
    else {
        return Ok(None);
    };
    let endpoint = endpoint.trim().trim_end_matches('/').to_string();
    let url = reqwest::Url::parse(&endpoint)
        .map_err(|e| format!("OTLP endpoint is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("OTLP endpoint must be http:// or https://".into());
    }
    if cfg.privacy.local_only && !is_loopback(&url) {
        return Err(format!(
            "[privacy].local_only is set but the OTLP endpoint {} is not local — export is off",
            display_endpoint(&endpoint)
        ));
    }

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &cfg.telemetry.otlp.headers {
        headers.insert(k.clone(), resolve_secret(k, v, &env)?);
    }
    for var in ["OTEL_EXPORTER_OTLP_HEADERS", "WINGMAN_OTLP_HEADERS"] {
        for pair in env(var).unwrap_or_default().split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                headers.insert(percent_decode(k.trim()), percent_decode(v.trim()));
            }
        }
    }
    for (k, v) in &headers {
        // Validated here, at the boundary, so a bad header is a refusal the
        // user sees rather than a client that silently never sends.
        reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| format!("OTLP header name `{k}` is not a valid header name"))?;
        reqwest::header::HeaderValue::from_str(v)
            .map_err(|_| format!("OTLP header `{k}` has a value that cannot be sent"))?;
    }

    let service_name =
        nonempty(cfg.telemetry.otlp.service_name.clone()).unwrap_or_else(|| "wingman".into());
    Ok(Some(Settings {
        endpoint,
        headers: headers.into_iter().collect(),
        service_name,
    }))
}

/// `${ENV_VAR}` and `keyring:<id>`, the forms other config secrets take.
fn resolve_secret(
    key: &str,
    value: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let v = value.trim();
    if let Some(name) = v.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        return env(name)
            .ok_or_else(|| format!("OTLP header `{key}` references ${{{name}}}, which is unset"));
    }
    if let Some(id) = v.strip_prefix("keyring:") {
        return match wingman_config::secrets::load(id) {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(format!("OTLP header `{key}`: no keyring entry '{id}'")),
            Err(e) => Err(format!("OTLP header `{key}`: {e}")),
        };
    }
    Ok(value.to_string())
}

/// Host is `localhost` or a loopback address. Parsed, not substring-matched,
/// so `http://localhost.evil.tld` is not local.
fn is_loopback(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The endpoint with any userinfo, query and fragment stripped — safe to print.
pub fn display_endpoint(endpoint: &str) -> String {
    match reqwest::Url::parse(endpoint.trim()) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<invalid URL>".into(),
    }
}

/// OTEL header lists are `k=v` pairs with percent-encoded values.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(byte) = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Spans from facts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Attr {
    Str(String),
    Int(i64),
    F64(f64),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct Span {
    trace_id: u128,
    span_id: u64,
    parent: Option<u64>,
    name: String,
    start_ns: u64,
    end_ns: u64,
    error: bool,
    attrs: Vec<(&'static str, Attr)>,
}

impl Span {
    fn attr(&self, key: &str) -> Option<&Attr> {
        self.attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
    fn str(&self, key: &str) -> String {
        match self.attr(key) {
            Some(Attr::Str(s)) => s.clone(),
            _ => String::new(),
        }
    }
    fn int(&self, key: &str) -> i64 {
        match self.attr(key) {
            Some(Attr::Int(i)) => *i,
            _ => 0,
        }
    }
}

struct Turn {
    trace_id: u128,
    span_id: u64,
    start_ns: u64,
    provider: String,
    model: String,
    usage: Usage,
    usd: Option<f64>,
    /// tool_use id -> (tool name, when the model asked for it).
    tools: HashMap<String, (String, u64)>,
}

/// Per-session span state. One per [`SessionLogSink`](crate::SessionLogSink).
#[derive(Default)]
pub struct Tracker {
    turn: Option<Turn>,
}

impl Tracker {
    /// Advance on one fact; returns any spans it completed.
    ///
    /// A turn opens on the first user message with none open, and closes on
    /// `Stop` — the same rule `turn_starts` numbers turns by, so gate feedback
    /// and steers stay inside their turn.
    // ponytail: a turn cancelled without a `Stop` absorbs the next prompt, as
    // in `turn_starts`; close on a new SessionStart if it matters.
    fn on_fact(&mut self, fact: &ContextFact, now_ns: u64) -> Vec<Span> {
        match fact {
            ContextFact::UserMessage { .. } if self.turn.is_none() => {
                self.turn = Some(Turn {
                    trace_id: (u128::from(rand_id()) << 64) | u128::from(rand_id()),
                    span_id: rand_id(),
                    start_ns: now_ns,
                    provider: String::new(),
                    model: String::new(),
                    usage: Usage::default(),
                    usd: None,
                    tools: HashMap::new(),
                });
            }
            ContextFact::AssistantMessage { blocks } => {
                if let Some(t) = self.turn.as_mut() {
                    for b in blocks {
                        if let ContentBlock::ToolUse { id, name, .. } = b {
                            t.tools.insert(id.clone(), (name.clone(), now_ns));
                        }
                    }
                }
            }
            ContextFact::ToolResult { id, is_error, .. } => {
                let Some(t) = self.turn.as_mut() else {
                    return Vec::new();
                };
                let Some((name, start)) = t.tools.remove(id) else {
                    return Vec::new();
                };
                // ponytail: starts when the model asked, so a sequential batch's
                // later calls include queue wait. Time inside the registry if
                // per-call precision matters.
                return vec![Span {
                    trace_id: t.trace_id,
                    span_id: rand_id(),
                    parent: Some(t.span_id),
                    name: format!("execute_tool {name}"),
                    start_ns: start,
                    end_ns: now_ns,
                    error: *is_error,
                    attrs: vec![
                        ("gen_ai.tool.name", Attr::Str(name)),
                        (
                            "wingman.tool.duration_ms",
                            Attr::Int((now_ns.saturating_sub(start) / 1_000_000) as i64),
                        ),
                        ("wingman.tool.is_error", Attr::Bool(*is_error)),
                    ],
                }];
            }
            ContextFact::Usage {
                usage,
                provider,
                model,
            } => {
                if let Some(t) = self.turn.as_mut() {
                    // Summed, as `wingman metrics` sums the logged deltas.
                    t.usage.add(usage);
                    t.provider.clone_from(provider);
                    t.model.clone_from(model);
                    if let Some(price) = wingman_core::pricing::price_for(model) {
                        *t.usd.get_or_insert(0.0) += price.cost(usage);
                    }
                }
            }
            ContextFact::Stop {
                reason, verified, ..
            } => {
                let Some(t) = self.turn.take() else {
                    return Vec::new();
                };
                let verdict = match verified {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "not_run",
                };
                let mut attrs = vec![
                    ("gen_ai.provider.name", Attr::Str(t.provider)),
                    ("gen_ai.request.model", Attr::Str(t.model)),
                    ("wingman.task_class", Attr::Str(TASK_CLASS.into())),
                    (
                        "gen_ai.usage.input_tokens",
                        Attr::Int(t.usage.input_tokens.into()),
                    ),
                    (
                        "gen_ai.usage.output_tokens",
                        Attr::Int(t.usage.output_tokens.into()),
                    ),
                    (
                        "wingman.usage.cache_read_tokens",
                        Attr::Int(t.usage.cache_read_input_tokens.into()),
                    ),
                    (
                        "wingman.usage.cache_creation_tokens",
                        Attr::Int(t.usage.cache_creation_input_tokens.into()),
                    ),
                    ("wingman.stop_reason", Attr::Str(reason.clone())),
                    ("wingman.gate.verdict", Attr::Str(verdict.into())),
                ];
                // Absent rather than 0 for an unpriced model: "free" and
                // "unknown" are different answers.
                if let Some(usd) = t.usd {
                    attrs.push(("wingman.cost.usd", Attr::F64(usd)));
                }
                return vec![Span {
                    trace_id: t.trace_id,
                    span_id: t.span_id,
                    parent: None,
                    name: "wingman.turn".into(),
                    start_ns: t.start_ns,
                    end_ns: now_ns,
                    error: matches!(reason.as_str(), "error" | "gate_failed"),
                    attrs,
                }];
            }
            _ => {}
        }
        Vec::new()
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Non-zero id. Uniqueness, not secrecy: `RandomState` is randomly keyed per
/// process and a counter separates calls.
fn rand_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u64(now_ns());
    h.finish().max(1)
}

// ---------------------------------------------------------------------------
// OTLP JSON
// ---------------------------------------------------------------------------

fn attr_json(key: &str, a: &Attr) -> Value {
    // OTLP JSON: 64-bit ints are strings, doubles are numbers.
    let value = match a {
        Attr::Str(s) => json!({ "stringValue": s }),
        Attr::Int(i) => json!({ "intValue": i.to_string() }),
        Attr::F64(f) => json!({ "doubleValue": f }),
        Attr::Bool(b) => json!({ "boolValue": b }),
    };
    json!({ "key": key, "value": value })
}

fn resource(service_name: &str) -> Value {
    json!({ "attributes": [attr_json("service.name", &Attr::Str(service_name.into()))] })
}

fn scope() -> Value {
    json!({ "name": "wingman", "version": env!("CARGO_PKG_VERSION") })
}

fn traces_payload(service_name: &str, spans: &[Span]) -> Value {
    let spans: Vec<Value> = spans
        .iter()
        .map(|s| {
            let mut v = json!({
                "traceId": format!("{:032x}", s.trace_id),
                "spanId": format!("{:016x}", s.span_id),
                "name": s.name,
                "kind": 1, // SPAN_KIND_INTERNAL
                "startTimeUnixNano": s.start_ns.to_string(),
                "endTimeUnixNano": s.end_ns.to_string(),
                "attributes": s.attrs.iter().map(|(k, a)| attr_json(k, a)).collect::<Vec<_>>(),
            });
            if let Some(p) = s.parent {
                v["parentSpanId"] = json!(format!("{p:016x}"));
            }
            if s.error {
                v["status"] = json!({ "code": 2 }); // STATUS_CODE_ERROR
            }
            v
        })
        .collect();
    json!({ "resourceSpans": [{
        "resource": resource(service_name),
        "scopeSpans": [{ "scope": scope(), "spans": spans }],
    }]})
}

/// Delta sums over one flush window, derived from the spans in it (so a
/// dropped span's tokens are dropped with it — the drop counter says how many).
fn metrics_payload(
    service_name: &str,
    spans: &[Span],
    dropped: u64,
    start_ns: u64,
    end_ns: u64,
) -> Value {
    type Key = (&'static str, Vec<(&'static str, String)>);
    let mut sums: BTreeMap<Key, f64> = BTreeMap::new();
    let mut add = |name: &'static str, attrs: Vec<(&'static str, String)>, v: f64| {
        *sums.entry((name, attrs)).or_default() += v;
    };
    for s in spans {
        if s.parent.is_some() {
            add(
                "wingman.tool.calls",
                vec![("gen_ai.tool.name", s.str("gen_ai.tool.name"))],
                1.0,
            );
            continue;
        }
        let model = || vec![("gen_ai.request.model", s.str("gen_ai.request.model"))];
        for (kind, key) in [
            ("input", "gen_ai.usage.input_tokens"),
            ("output", "gen_ai.usage.output_tokens"),
            ("cache_read", "wingman.usage.cache_read_tokens"),
            ("cache_creation", "wingman.usage.cache_creation_tokens"),
        ] {
            let mut attrs = model();
            attrs.push(("gen_ai.token.type", kind.into()));
            add("wingman.tokens", attrs, s.int(key) as f64);
        }
        if let Some(Attr::F64(usd)) = s.attr("wingman.cost.usd") {
            add("wingman.cost.usd", model(), *usd);
        }
        if s.str("wingman.gate.verdict") == "passed" {
            add("wingman.turns.verified_done", model(), 1.0);
        }
    }
    if dropped > 0 {
        add(
            "wingman.telemetry.dropped_spans",
            Vec::new(),
            dropped as f64,
        );
    }

    let mut metrics: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for ((name, attrs), v) in sums {
        let mut point = json!({
            "attributes": attrs.iter().map(|(k, s)| attr_json(k, &Attr::Str(s.clone()))).collect::<Vec<_>>(),
            "startTimeUnixNano": start_ns.to_string(),
            "timeUnixNano": end_ns.to_string(),
        });
        if name == "wingman.cost.usd" {
            point["asDouble"] = json!(v);
        } else {
            point["asInt"] = json!((v as i64).to_string());
        }
        metrics.entry(name).or_default().push(point);
    }
    let metrics: Vec<Value> = metrics
        .into_iter()
        .map(|(name, points)| {
            let unit = match name {
                "wingman.tokens" => "{token}",
                "wingman.cost.usd" => "USD",
                "wingman.tool.calls" => "{call}",
                "wingman.turns.verified_done" => "{turn}",
                _ => "{span}",
            };
            json!({
                "name": name,
                "unit": unit,
                "sum": {
                    "aggregationTemporality": 1, // DELTA
                    "isMonotonic": true,
                    "dataPoints": points,
                },
            })
        })
        .collect();
    json!({ "resourceMetrics": [{
        "resource": resource(service_name),
        "scopeMetrics": [{ "scope": scope(), "metrics": metrics }],
    }]})
}

// ---------------------------------------------------------------------------
// Queue and background flush
// ---------------------------------------------------------------------------

enum Msg {
    Span(Span),
    Flush(oneshot::Sender<()>),
}

struct Exporter {
    tx: mpsc::Sender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl Exporter {
    fn new(capacity: usize) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (Self { tx, dropped }, rx)
    }

    /// Never waits. A full or closed queue drops the span and counts it.
    fn send(&self, span: Span) {
        if self.tx.try_send(Msg::Span(span)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

static EXPORTER: OnceLock<Exporter> = OnceLock::new();

/// Start the exporter if `[telemetry.otlp]` (or the environment) enables it.
/// Idempotent. `Ok(false)` means not configured; `Err` is a refusal to print.
/// Must be called inside a tokio runtime.
pub fn init(cfg: &Config) -> Result<bool, String> {
    if EXPORTER.get().is_some() {
        return Ok(true);
    }
    let Some(s) = settings(cfg, |k| std::env::var(k).ok())? else {
        return Ok(false);
    };
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "OTLP export needs an async runtime".to_string())?;
    wingman_core::ensure_tls_provider();
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in &s.headers {
        // Already validated by `settings`.
        if let (Ok(name), Ok(mut value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(POST_TIMEOUT)
        .build()
        .map_err(|e| format!("OTLP client: {e}"))?;
    let (exporter, rx) = Exporter::new(QUEUE);
    let dropped = exporter.dropped.clone();
    if EXPORTER.set(exporter).is_ok() {
        rt.spawn(worker(rx, s, client, dropped));
    }
    Ok(true)
}

/// Feed one fact to a session's tracker and queue whatever it completes.
pub fn observe(tracker: &Mutex<Tracker>, fact: &ContextFact) {
    let Some(exporter) = EXPORTER.get() else {
        return;
    };
    let spans = tracker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_fact(fact, now_ns());
    for span in spans {
        exporter.send(span);
    }
}

/// Flush what is queued, waiting at most `timeout`. Call once at exit.
pub async fn shutdown(timeout: Duration) {
    let Some(exporter) = EXPORTER.get() else {
        return;
    };
    let (ack, done) = oneshot::channel();
    let _ = tokio::time::timeout(timeout, async {
        if exporter.tx.send(Msg::Flush(ack)).await.is_ok() {
            let _ = done.await;
        }
    })
    .await;
}

async fn worker(
    mut rx: mpsc::Receiver<Msg>,
    s: Settings,
    client: reqwest::Client,
    dropped: Arc<AtomicU64>,
) {
    let mut batch: Vec<Span> = Vec::new();
    let mut since = now_ns();
    let mut dropped_sent = 0;
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    loop {
        let mut ack = None;
        tokio::select! {
            m = rx.recv() => match m {
                Some(Msg::Span(span)) => {
                    batch.push(span);
                    if batch.len() < MAX_BATCH {
                        continue;
                    }
                }
                Some(Msg::Flush(a)) => ack = Some(a),
                None => return,
            },
            _ = tick.tick() => {}
        }
        let now = now_ns();
        let d = dropped.load(Ordering::Relaxed);
        if !batch.is_empty() || d != dropped_sent {
            // ponytail: no retry — a failed batch is lost, like a full queue.
            // Add a small retry buffer if collectors flap in practice.
            let spans = std::mem::take(&mut batch);
            if !spans.is_empty() {
                post(
                    &client,
                    &s,
                    "traces",
                    traces_payload(&s.service_name, &spans),
                )
                .await;
            }
            let metrics = metrics_payload(&s.service_name, &spans, d - dropped_sent, since, now);
            post(&client, &s, "metrics", metrics).await;
            dropped_sent = d;
            since = now;
        }
        if let Some(a) = ack {
            let _ = a.send(());
        }
    }
}

async fn post(client: &reqwest::Client, s: &Settings, signal: &str, body: Value) {
    let url = format!("{}/v1/{signal}", s.endpoint);
    let res = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => tracing::warn!(
            target: "wingman::telemetry",
            "OTLP {signal} export rejected: HTTP {}",
            r.status()
        ),
        Err(e) => tracing::warn!(
            target: "wingman::telemetry",
            "OTLP {signal} export to {} failed: {e}",
            display_endpoint(&s.endpoint)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(endpoint: Option<&str>) -> Config {
        let mut cfg = Config::default();
        cfg.telemetry.otlp.endpoint = endpoint.map(str::to_string);
        cfg
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// One turn: a tool call that errors, usage, then a green stop.
    fn one_turn() -> Vec<Span> {
        let mut t = Tracker::default();
        let mut out = Vec::new();
        let facts = [
            ContextFact::UserMessage {
                text: "prompt".into(),
            },
            ContextFact::AssistantMessage {
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "run_shell".into(),
                    input: json!({ "command": "echo SECRET_INPUT", "api_key": "sk-live" }),
                }],
            },
            ContextFact::ToolResult {
                id: "t1".into(),
                full: "SECRET_OUTPUT".into(),
                model_view: None,
                is_error: true,
            },
            ContextFact::Usage {
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_creation_input_tokens: 5,
                    cache_read_input_tokens: 7,
                },
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
            },
            ContextFact::Stop {
                reason: "end_turn".into(),
                first_output_ms: Some(10),
                verified: Some(true),
            },
        ];
        for (i, f) in facts.iter().enumerate() {
            out.extend(t.on_fact(f, 1_700_000_000_000_000_000 + i as u64 * 1_000_000));
        }
        out
    }

    fn is_hex(s: &str, len: usize) -> bool {
        s.len() == len && s.bytes().all(|c| c.is_ascii_hexdigit()) && s != "0".repeat(len)
    }

    #[test]
    fn traces_match_the_otlp_json_shape() {
        let spans = one_turn();
        assert_eq!(spans.len(), 2, "one tool span, one turn span");
        let body = traces_payload("wm-test", &spans);
        let rs = &body["resourceSpans"][0];
        assert_eq!(
            rs["resource"]["attributes"][0],
            json!({ "key": "service.name", "value": { "stringValue": "wm-test" } })
        );
        let out = rs["scopeSpans"][0]["spans"].as_array().unwrap();
        let (tool, turn) = (&out[0], &out[1]);

        for s in [tool, turn] {
            assert!(is_hex(s["traceId"].as_str().unwrap(), 32));
            assert!(is_hex(s["spanId"].as_str().unwrap(), 16));
            for k in ["startTimeUnixNano", "endTimeUnixNano"] {
                let v = s[k].as_str().expect("unix nanos are strings");
                assert!(v.parse::<u64>().is_ok());
            }
        }
        assert_eq!(tool["traceId"], turn["traceId"]);
        assert_eq!(tool["parentSpanId"], turn["spanId"]);
        assert!(turn.get("parentSpanId").is_none());
        assert_eq!(tool["status"]["code"], 2);
        assert!(turn.get("status").is_none());

        let attr = |s: &Value, k: &str| {
            s["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == k)
                .map(|a| a["value"].clone())
                .unwrap_or(Value::Null)
        };
        assert_eq!(
            attr(tool, "gen_ai.tool.name"),
            json!({ "stringValue": "run_shell" })
        );
        assert_eq!(
            attr(tool, "wingman.tool.is_error"),
            json!({ "boolValue": true })
        );
        assert_eq!(
            attr(turn, "gen_ai.usage.input_tokens"),
            json!({ "intValue": "100" })
        );
        assert_eq!(
            attr(turn, "wingman.usage.cache_read_tokens"),
            json!({ "intValue": "7" })
        );
        assert_eq!(
            attr(turn, "gen_ai.provider.name"),
            json!({ "stringValue": "anthropic" })
        );
        assert_eq!(
            attr(turn, "wingman.stop_reason"),
            json!({ "stringValue": "end_turn" })
        );
        assert_eq!(
            attr(turn, "wingman.gate.verdict"),
            json!({ "stringValue": "passed" })
        );
        assert!(
            attr(turn, "wingman.cost.usd")["doubleValue"]
                .as_f64()
                .unwrap()
                > 0.0
        );

        // Tool input and output never leave.
        let text = body.to_string();
        for leak in ["SECRET_INPUT", "SECRET_OUTPUT", "sk-live", "prompt"] {
            assert!(!text.contains(leak), "{leak} leaked into the export");
        }
    }

    #[test]
    fn metrics_are_delta_sums_in_otlp_json_shape() {
        let body = metrics_payload("wingman", &one_turn(), 3, 1, 2);
        let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        let find = |n: &str| metrics.iter().find(|m| m["name"] == n).unwrap().clone();
        for m in metrics {
            assert_eq!(m["sum"]["aggregationTemporality"], 1);
            assert_eq!(m["sum"]["isMonotonic"], true);
        }
        let tokens = find("wingman.tokens");
        let input = tokens["sum"]["dataPoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["attributes"].to_string().contains("\"input\""))
            .unwrap()
            .clone();
        assert_eq!(input["asInt"], "100");
        assert_eq!(input["startTimeUnixNano"], "1");
        assert_eq!(input["timeUnixNano"], "2");
        assert_eq!(
            find("wingman.tool.calls")["sum"]["dataPoints"][0]["asInt"],
            "1"
        );
        assert_eq!(
            find("wingman.turns.verified_done")["sum"]["dataPoints"][0]["asInt"],
            "1"
        );
        assert!(find("wingman.cost.usd")["sum"]["dataPoints"][0]["asDouble"].is_f64());
        assert_eq!(
            find("wingman.telemetry.dropped_spans")["sum"]["dataPoints"][0]["asInt"],
            "3"
        );
    }

    #[test]
    fn a_full_queue_drops_and_counts_instead_of_waiting() {
        let (exporter, _rx) = Exporter::new(2);
        let span = one_turn().remove(0);
        for _ in 0..5 {
            exporter.send(span.clone());
        }
        assert_eq!(exporter.dropped.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_closed_queue_drops_too() {
        let (exporter, rx) = Exporter::new(8);
        drop(rx);
        exporter.send(one_turn().remove(0));
        assert_eq!(exporter.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn off_without_an_endpoint() {
        assert!(settings(&Config::default(), no_env).unwrap().is_none());
    }

    #[test]
    fn local_only_refuses_a_remote_endpoint() {
        for remote in [
            "https://collector.example.com",
            "http://localhost.evil.tld:4318",
            "http://10.0.0.5:4318",
        ] {
            let mut cfg = cfg_with(Some(remote));
            cfg.privacy.local_only = true;
            let err = settings(&cfg, no_env).err().unwrap_or_default();
            assert!(err.contains("local_only"), "{remote}: {err}");
        }
        for local in [
            "http://localhost:4318/",
            "http://127.0.0.1:4318",
            "http://[::1]:4318",
        ] {
            let mut cfg = cfg_with(Some(local));
            cfg.privacy.local_only = true;
            let s = settings(&cfg, no_env).unwrap().unwrap();
            assert!(!s.endpoint.ends_with('/'));
        }
        // An env override is held to the same rule.
        let mut cfg = Config::default();
        cfg.privacy.local_only = true;
        let env =
            |k: &str| (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "https://x.example".into());
        assert!(settings(&cfg, env).is_err());
    }

    #[test]
    fn env_overrides_config_and_wingman_beats_otel() {
        let mut cfg = cfg_with(Some("http://config:4318"));
        cfg.telemetry
            .otlp
            .headers
            .insert("x-team".into(), "${WM_TEST_OTLP_KEY}".into());
        let env = |k: &str| {
            Some(match k {
                "OTEL_EXPORTER_OTLP_ENDPOINT" => "http://otel:4318".to_string(),
                "WINGMAN_OTLP_ENDPOINT" => "http://wingman:4318".to_string(),
                "WM_TEST_OTLP_KEY" => "from-env".to_string(),
                "OTEL_EXPORTER_OTLP_HEADERS" => {
                    "authorization=Bearer%20abc, x-team=otel".to_string()
                }
                "WINGMAN_OTLP_HEADERS" => "x-team=wingman".to_string(),
                _ => return None,
            })
        };
        let s = settings(&cfg, env).unwrap().unwrap();
        assert_eq!(s.endpoint, "http://wingman:4318");
        assert_eq!(s.service_name, "wingman");
        let h: BTreeMap<_, _> = s.headers.into_iter().collect();
        assert_eq!(h["authorization"], "Bearer abc");
        assert_eq!(h["x-team"], "wingman");

        let only_otel =
            |k: &str| (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://otel:4318".into());
        let mut plain = cfg_with(Some("http://config:4318"));
        assert_eq!(
            settings(&plain, only_otel).unwrap().unwrap().endpoint,
            "http://otel:4318"
        );
        plain.telemetry.otlp.endpoint = None;
        assert!(settings(&plain, no_env).unwrap().is_none());
    }

    #[test]
    fn bad_input_is_refused_not_ignored() {
        assert!(settings(&cfg_with(Some("not a url")), no_env).is_err());
        assert!(settings(&cfg_with(Some("file:///etc/passwd")), no_env).is_err());
        let mut cfg = cfg_with(Some("http://localhost:4318"));
        cfg.telemetry
            .otlp
            .headers
            .insert("x-key".into(), "${WM_UNSET_VAR_91}".into());
        assert!(settings(&cfg, no_env).is_err());
        let mut cfg = cfg_with(Some("http://localhost:4318"));
        cfg.telemetry
            .otlp
            .headers
            .insert("bad header".into(), "v".into());
        assert!(settings(&cfg, no_env).is_err());
    }

    #[test]
    fn display_endpoint_strips_credentials() {
        assert_eq!(
            display_endpoint("https://user:pw@otel.example.com:4318/p?token=x"),
            "https://otel.example.com:4318/p"
        );
    }
}

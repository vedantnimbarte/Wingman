//! `recall_memory`: fetch the full body of a stored memory by slug, or list
//! all memories when no name is given.

use std::sync::Arc;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::memory::MemoryStore;

pub struct RecallMemory {
    store: Arc<MemoryStore>,
}

impl RecallMemory {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize, Default)]
struct Args {
    #[serde(default)]
    name: Option<String>,
}

/// Format a `SystemTime` as `YYYY-MM-DD` (UTC) without a date crate, via the
/// civil-from-days algorithm (Howard Hinnant).
fn fmt_date(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[async_trait]
impl Tool for RecallMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall_memory".into(),
            description: "Read a memory's full body by slug. Omit `name` to list all known \
                          memories. Use this when the system-prompt memory index hints at \
                          something that's relevant to the current task but you need the detail."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Memory slug from the index." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = serde_json::from_value(args).unwrap_or_default();
        match args.name {
            Some(name) => match self.store.find(&name) {
                Some(m) => {
                    // Provenance: where this came from + when it was last written,
                    // so the agent can *cite* the memory it acts on (trust in a
                    // compounding asset comes from being able to see its source).
                    let saved = std::fs::metadata(&m.path)
                        .and_then(|md| md.modified())
                        .ok()
                        .map(fmt_date)
                        .unwrap_or_else(|| "unknown".into());
                    ToolOutcome::ok(format!(
                        "name: {}\ntype: {}\nscope: {}\ndescription: {}\nsource: {} (saved {})\n---\n{}\n---\nWhen you act on this memory, cite it briefly, e.g. \"(per memory `{}`)\".",
                        m.name,
                        m.mtype.as_str(),
                        m.scope.label(),
                        m.description,
                        m.path.display(),
                        saved,
                        m.body,
                        m.name,
                    ))
                }
                None => ToolOutcome::err(format!("no memory with name '{name}'")),
            },
            None => {
                let mems = self.store.load_all();
                if mems.is_empty() {
                    return ToolOutcome::ok("(no memories yet — use save_memory to persist one)");
                }
                let mut out = String::new();
                for m in mems {
                    out.push_str(&format!(
                        "- [{}] {} ({}) — {}\n",
                        m.mtype.as_str(),
                        m.name,
                        m.scope.label(),
                        m.description
                    ));
                }
                ToolOutcome::ok(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_date;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn fmt_date_civil_from_days() {
        // 2021-01-01 00:00:00 UTC = 1_609_459_200.
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(fmt_date(t), "2021-01-01");
        // Epoch.
        assert_eq!(fmt_date(UNIX_EPOCH), "1970-01-01");
        // A leap day: 2020-02-29 = 1_582_934_400.
        let leap = UNIX_EPOCH + Duration::from_secs(1_582_934_400);
        assert_eq!(fmt_date(leap), "2020-02-29");
    }
}

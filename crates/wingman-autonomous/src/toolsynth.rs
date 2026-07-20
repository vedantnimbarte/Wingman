//! J7 — tool synthesis (the agent grows its own capabilities).
//!
//! When a worker repeatedly hits the same gap ("I keep needing to query
//! the SQLite DB but there's no tool"), it emits a `propose_tool`. The
//! orchestrator queues it as a `meta` task; a `tool-smith` role generates
//! the implementation + a test under `~/.wingman/tools/`, and the next run
//! has it available.
//!
//! This module validates and de-duplicates proposals — the gate before a
//! proposal becomes real work. Generation + sandboxing (J11) are the
//! tool-smith's job. Gated behind `[pilot.tools].allow_synthesis`.

use serde::{Deserialize, Serialize};

/// A worker's request for a new tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProposal {
    pub name: String,
    pub description: String,
    /// JSON-Schema for the tool's parameters.
    #[serde(default)]
    pub schema: serde_json::Value,
    /// Free-text sketch of how to implement it.
    #[serde(default)]
    pub impl_sketch: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProposalError {
    /// Name isn't a valid `snake_case` identifier.
    BadName(String),
    /// Description is empty / too short to act on.
    EmptyDescription,
    /// Schema isn't a JSON object.
    BadSchema,
    /// A tool by this name already exists.
    Duplicate(String),
}

impl std::fmt::Display for ProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(n) => write!(f, "invalid tool name `{n}` (expect snake_case)"),
            Self::EmptyDescription => write!(f, "tool description is empty"),
            Self::BadSchema => write!(f, "tool schema must be a JSON object"),
            Self::Duplicate(n) => write!(f, "a tool named `{n}` already exists"),
        }
    }
}

/// Is `name` a valid `snake_case` tool identifier?
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Validate one proposal against the set of existing tool names.
pub fn validate(proposal: &ToolProposal, existing: &[String]) -> Result<(), ProposalError> {
    if !is_valid_tool_name(&proposal.name) {
        return Err(ProposalError::BadName(proposal.name.clone()));
    }
    if proposal.description.trim().len() < 8 {
        return Err(ProposalError::EmptyDescription);
    }
    // Empty schema is allowed (a no-arg tool), but a non-object non-null
    // schema is rejected.
    if !proposal.schema.is_null() && !proposal.schema.is_object() {
        return Err(ProposalError::BadSchema);
    }
    if existing.iter().any(|e| e == &proposal.name) {
        return Err(ProposalError::Duplicate(proposal.name.clone()));
    }
    Ok(())
}

/// De-duplicate a batch of proposals (keeping the first of each name) and
/// drop any that fail validation. Returns the accepted proposals; the
/// caller queues each as a meta task.
pub fn accept_batch(proposals: &[ToolProposal], existing: &[String]) -> Vec<ToolProposal> {
    let mut seen: Vec<String> = existing.to_vec();
    let mut out = Vec::new();
    for p in proposals {
        if validate(p, &seen).is_ok() {
            seen.push(p.name.clone());
            out.push(p.clone());
        }
    }
    out
}

/// Parse a `propose_tool` payload from worker output.
pub fn parse_proposal(json: &str) -> Result<ToolProposal, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid tool proposal: {e}"))
}

/// J7 — persist the validated, de-duplicated proposals to `dir` as
/// `<name>.json` scaffolds for the tool-smith to flesh out, and append their
/// manifest lines to `dir/registry.jsonl`. `existing` is the set of tool
/// names already present so duplicates are dropped. Returns the accepted
/// names actually written.
///
/// ponytail: this writes scaffolds + a registry line, not a live tool —
/// compiling a synthesized tool into the binary needs a rebuild (or a plugin
/// loader). The registry line is the seam a rebuild reads. Upgrade path: a
/// `dlopen`/wasm plugin host if restart-to-register proves too slow.
pub fn persist_accepted(
    proposals: &[ToolProposal],
    existing: &[String],
    dir: &std::path::Path,
) -> std::io::Result<Vec<String>> {
    use std::io::Write;
    let accepted = accept_batch(proposals, existing);
    if accepted.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir)?;
    let mut registry = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("registry.jsonl"))?;
    let mut names = Vec::with_capacity(accepted.len());
    for p in &accepted {
        let path = dir.join(format!("{}.json", p.name));
        std::fs::write(&path, serde_json::to_vec_pretty(p)?)?;
        let manifest = serde_json::json!({
            "name": p.name,
            "description": p.description,
            "schema": p.schema,
            "path": path.to_string_lossy(),
        });
        writeln!(registry, "{manifest}")?;
        names.push(p.name.clone());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(name: &str) -> ToolProposal {
        ToolProposal {
            name: name.into(),
            description: "query the project sqlite database".into(),
            schema: serde_json::json!({"type": "object", "properties": {}}),
            impl_sketch: "use rusqlite".into(),
        }
    }

    #[test]
    fn valid_names_accepted() {
        assert!(is_valid_tool_name("query_db"));
        assert!(is_valid_tool_name("grep2"));
        assert!(is_valid_tool_name("a"));
    }

    #[test]
    fn j7_persist_accepted_writes_scaffolds_and_registry() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tools");
        // query_db is new; "read_file" collides with an existing tool and
        // is dropped; a second query_db in-batch is de-duped.
        let props = vec![
            proposal("query_db"),
            proposal("read_file"),
            proposal("query_db"),
        ];
        let names = persist_accepted(&props, &["read_file".to_string()], &out).unwrap();
        assert_eq!(names, vec!["query_db".to_string()]);
        assert!(out.join("query_db.json").exists());
        assert!(!out.join("read_file.json").exists());
        let registry = std::fs::read_to_string(out.join("registry.jsonl")).unwrap();
        assert_eq!(registry.lines().count(), 1);
        assert!(registry.contains("query_db"));
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("QueryDb")); // uppercase
        assert!(!is_valid_tool_name("2fast")); // leading digit
        assert!(!is_valid_tool_name("query-db")); // hyphen
        assert!(!is_valid_tool_name("query db")); // space
    }

    #[test]
    fn validate_accepts_good_proposal() {
        assert!(validate(&proposal("query_db"), &[]).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate() {
        let existing = vec!["query_db".to_string()];
        assert_eq!(
            validate(&proposal("query_db"), &existing),
            Err(ProposalError::Duplicate("query_db".into()))
        );
    }

    #[test]
    fn validate_rejects_short_description() {
        let mut p = proposal("query_db");
        p.description = "db".into();
        assert_eq!(validate(&p, &[]), Err(ProposalError::EmptyDescription));
    }

    #[test]
    fn validate_rejects_array_schema() {
        let mut p = proposal("query_db");
        p.schema = serde_json::json!([1, 2, 3]);
        assert_eq!(validate(&p, &[]), Err(ProposalError::BadSchema));
    }

    #[test]
    fn validate_allows_null_schema() {
        let mut p = proposal("ping");
        p.schema = serde_json::Value::Null;
        assert!(validate(&p, &[]).is_ok());
    }

    #[test]
    fn accept_batch_dedups_and_filters() {
        let proposals = vec![
            proposal("query_db"),
            proposal("query_db"),        // dup within batch
            proposal("BadName"),         // invalid
            proposal("list_migrations"), // fine
        ];
        let accepted = accept_batch(&proposals, &[]);
        let names: Vec<&str> = accepted.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["query_db", "list_migrations"]);
    }

    #[test]
    fn accept_batch_respects_existing() {
        let accepted = accept_batch(&[proposal("query_db")], &["query_db".to_string()]);
        assert!(accepted.is_empty());
    }

    #[test]
    fn parse_proposal_reads_json() {
        let json = r#"{"name":"query_db","description":"query the sqlite db","schema":{"type":"object"},"impl_sketch":"rusqlite"}"#;
        let p = parse_proposal(json).unwrap();
        assert_eq!(p.name, "query_db");
    }

    #[test]
    fn parse_proposal_rejects_garbage() {
        assert!(parse_proposal("{").is_err());
    }
}

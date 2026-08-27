//! Import hooks from an existing Claude Code `settings.json`.
//!
//! Someone arriving from Claude Code already has hooks written and working.
//! Making them rewrite that block in `[hooks]` to try Wingman is a pointless
//! tax, and the two formats say the same thing in different shapes.
//!
//! This is a *bridge*, not a second hook system: everything it reads is
//! translated into ordinary [`Hook`]s and runs through the same interception
//! points, with the same permission gates and the same logging.
//!
//! Two things it deliberately does not do:
//!
//! - **It is off by default** ([`HooksConfig::import_claude_code`]). Hooks
//!   execute shell commands. Silently running another tool's configuration
//!   because it happened to be on disk is a surprise nobody asked for, and
//!   the blast radius is arbitrary code. `wingman doctor` points out that an
//!   importable file exists, so opting in is discoverable rather than hidden.
//! - **A project-level file is trust-gated**, exactly like
//!   `<project>/.wingman/config.toml`. A cloned repository must not be able
//!   to run commands on checkout, and `.claude/settings.json` is no less
//!   part of the clone than `.wingman/config.toml` is.

use std::path::Path;

use crate::{trust, Hook, HooksConfig};

/// Claude Code's tool names mapped onto Wingman's.
///
/// Without this the bridge is theatre: a `matcher` of `Bash` would be copied
/// verbatim and never match `run_shell`, so every imported hook would import
/// cleanly and then never fire — worse than not importing it, because it
/// looks like it worked.
///
/// MCP tools are omitted: both products namespace them as
/// `mcp__<server>__<tool>`, so those pass through unchanged.
const TOOL_NAMES: &[(&str, &str)] = &[
    ("Bash", "run_shell"),
    ("Read", "read_file"),
    ("Write", "write_file"),
    ("Edit", "edit_file"),
    ("MultiEdit", "apply_patch"),
    ("NotebookEdit", "edit_file"),
    ("Glob", "glob"),
    ("Grep", "grep"),
    ("LS", "list_dir"),
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    ("Task", "spawn_subagent"),
    ("TodoWrite", "update_tasks"),
];

/// What an import produced, so callers can report it rather than guess.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// How many hooks were added, per event.
    pub imported: usize,
    /// Matchers that could not be translated, kept verbatim. These very
    /// likely never fire, so the user needs to hear about them.
    pub untranslated: Vec<String>,
}

/// Translate one Claude Code `matcher` into Wingman tool-name patterns.
///
/// Handles the shapes people actually write: a single tool name, an
/// alternation (`Bash|Edit`), and the match-everything cases (`*`, empty).
/// Anything else is a regex we are not going to reimplement — it comes back
/// verbatim and is reported, because a silently-never-matching hook is the
/// failure mode this whole map exists to avoid.
fn translate_matcher(matcher: &str) -> (Vec<String>, Vec<String>) {
    let trimmed = matcher.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == ".*" {
        // Wingman spells "everything" as an empty pattern.
        return (vec![String::new()], Vec::new());
    }
    let mut names = Vec::new();
    let mut untranslated = Vec::new();
    for part in trimmed.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match TOOL_NAMES.iter().find(|(cc, _)| *cc == part) {
            Some((_, wingman)) => names.push((*wingman).to_string()),
            None if part.starts_with("mcp__") => names.push(part.to_string()),
            None if part.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                // An unknown but plausible tool name — pass it through. It may
                // be a Wingman tool the user named directly.
                names.push(part.to_string());
            }
            None => {
                untranslated.push(part.to_string());
                names.push(part.to_string());
            }
        }
    }
    if names.is_empty() {
        names.push(String::new());
    }
    (names, untranslated)
}

/// Parse the `hooks` block of a Claude Code `settings.json` into Wingman hooks.
///
/// Unknown events are ignored rather than rejected: Claude Code has more
/// lifecycle points than Wingman does, and refusing a whole file because it
/// mentions one we lack would defeat the purpose.
pub fn parse(json: &str, into: &mut HooksConfig) -> ImportReport {
    let mut report = ImportReport::default();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return report;
    };
    let Some(hooks) = root.get("hooks").and_then(|h| h.as_object()) else {
        return report;
    };

    for (event, groups) in hooks {
        // `block` is not a faithful round-trip. Claude Code distinguishes
        // "fail" from "block" by exit code; Wingman has one boolean. A
        // pre-tool or prompt hook is overwhelmingly a guard, so those import
        // as blocking — failing closed is the right way to be wrong about a
        // security control. The difference is documented, not hidden.
        let (target, block): (&mut Vec<Hook>, bool) = match event.as_str() {
            "PreToolUse" => (&mut into.pre_tool_use, true),
            "PostToolUse" => (&mut into.post_tool_use, false),
            "UserPromptSubmit" => (&mut into.user_prompt_submit, true),
            "Stop" => (&mut into.stop, false),
            _ => continue,
        };
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for group in groups {
            let matcher = group.get("matcher").and_then(|m| m.as_str()).unwrap_or("");
            let (patterns, mut bad) = translate_matcher(matcher);
            report.untranslated.append(&mut bad);
            let Some(commands) = group.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for entry in commands {
                // Only `type: "command"` is executable shell. Anything else
                // is a Claude Code concept we do not have.
                if entry.get("type").and_then(|t| t.as_str()) != Some("command") {
                    continue;
                }
                let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                let timeout_secs = entry
                    .get("timeout")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(crate::default_hook_timeout());
                // One Wingman hook per translated name: an alternation has no
                // equivalent in a single `match_tool`, and duplicating the
                // command is closer to the original than dropping the rest of
                // the alternation would be.
                for pattern in &patterns {
                    target.push(Hook {
                        command: command.to_string(),
                        match_tool: pattern.clone(),
                        block,
                        timeout_secs,
                    });
                    report.imported += 1;
                }
            }
        }
    }
    report
}

/// Import from a settings file, honouring the trust boundary.
///
/// `require_trust` is set for a project-level file. The global file is the
/// user's own and needs no gate; a file inside a repository is part of the
/// clone and gets exactly the treatment `<project>/.wingman/config.toml`
/// gets.
pub fn import_file(path: &Path, require_trust: bool, into: &mut HooksConfig) -> ImportReport {
    if !path.exists() {
        return ImportReport::default();
    }
    if require_trust && !trust::is_trusted(path) {
        tracing::warn!(
            target: "wingman::hooks",
            file = %path.display(),
            "not importing project Claude Code hooks: run `wingman trust` on this file first \
             (it can execute commands, so a cloned repo does not get to)"
        );
        return ImportReport::default();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return ImportReport::default();
    };
    parse(&text, into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported(json: &str) -> (HooksConfig, ImportReport) {
        let mut cfg = HooksConfig::default();
        let report = parse(json, &mut cfg);
        (cfg, report)
    }

    #[test]
    fn a_bash_matcher_becomes_run_shell() {
        // The whole reason the name map exists: copied verbatim, `Bash` would
        // never match anything and the hook would silently never fire.
        let (cfg, report) = imported(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash",
                 "hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        );
        assert_eq!(report.imported, 1);
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].match_tool, "run_shell");
        assert_eq!(cfg.pre_tool_use[0].command, "echo hi");
    }

    #[test]
    fn an_alternation_becomes_one_hook_per_tool() {
        let (cfg, _) = imported(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Edit|Write",
                 "hooks":[{"type":"command","command":"fmt"}]}]}}"#,
        );
        let names: Vec<&str> = cfg
            .pre_tool_use
            .iter()
            .map(|h| h.match_tool.as_str())
            .collect();
        assert_eq!(names, vec!["edit_file", "write_file"]);
    }

    #[test]
    fn a_wildcard_matcher_matches_everything() {
        for matcher in ["*", ".*", ""] {
            let (cfg, _) = imported(&format!(
                r#"{{"hooks":{{"PostToolUse":[{{"matcher":"{matcher}",
                     "hooks":[{{"type":"command","command":"log"}}]}}]}}}}"#
            ));
            assert_eq!(cfg.post_tool_use[0].match_tool, "", "matcher {matcher:?}");
        }
    }

    #[test]
    fn guard_events_import_as_blocking_and_advisory_ones_do_not() {
        let (cfg, _) = imported(
            r#"{"hooks":{
                 "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"a"}]}],
                 "UserPromptSubmit":[{"hooks":[{"type":"command","command":"b"}]}],
                 "PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"c"}]}],
                 "Stop":[{"hooks":[{"type":"command","command":"d"}]}]}}"#,
        );
        assert!(
            cfg.pre_tool_use[0].block,
            "a pre-tool guard should fail closed"
        );
        assert!(cfg.user_prompt_submit[0].block);
        assert!(!cfg.post_tool_use[0].block, "post-tool is advisory");
        assert!(!cfg.stop[0].block);
    }

    #[test]
    fn an_untranslatable_matcher_is_reported_rather_than_swallowed() {
        let (_, report) = imported(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash(?!x)",
                 "hooks":[{"type":"command","command":"a"}]}]}}"#,
        );
        assert_eq!(report.untranslated, vec!["Bash(?!x)"]);
    }

    #[test]
    fn mcp_tool_names_pass_through_unchanged() {
        let (cfg, report) = imported(
            r#"{"hooks":{"PreToolUse":[{"matcher":"mcp__github__create_issue",
                 "hooks":[{"type":"command","command":"a"}]}]}}"#,
        );
        assert_eq!(cfg.pre_tool_use[0].match_tool, "mcp__github__create_issue");
        assert!(report.untranslated.is_empty());
    }

    #[test]
    fn events_wingman_does_not_have_are_skipped_not_fatal() {
        let (cfg, report) = imported(
            r#"{"hooks":{
                 "SessionStart":[{"hooks":[{"type":"command","command":"nope"}]}],
                 "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"yes"}]}]}}"#,
        );
        assert_eq!(report.imported, 1);
        assert_eq!(cfg.pre_tool_use[0].command, "yes");
    }

    #[test]
    fn non_command_hook_types_are_ignored() {
        let (cfg, report) = imported(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash",
                 "hooks":[{"type":"something_else","command":"a"}]}]}}"#,
        );
        assert_eq!(report.imported, 0);
        assert!(cfg.pre_tool_use.is_empty());
    }

    #[test]
    fn a_timeout_carries_over_and_otherwise_defaults() {
        let (cfg, _) = imported(
            r#"{"hooks":{"PreToolUse":[
                 {"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":45}]},
                 {"matcher":"Read","hooks":[{"type":"command","command":"b"}]}]}}"#,
        );
        let a = cfg.pre_tool_use.iter().find(|h| h.command == "a").unwrap();
        let b = cfg.pre_tool_use.iter().find(|h| h.command == "b").unwrap();
        assert_eq!(a.timeout_secs, 45);
        assert_eq!(b.timeout_secs, crate::default_hook_timeout());
    }

    #[test]
    fn malformed_json_imports_nothing_rather_than_failing_startup() {
        let (cfg, report) = imported("{ not json");
        assert_eq!(report, ImportReport::default());
        assert!(cfg.pre_tool_use.is_empty());
    }

    #[test]
    fn an_untrusted_project_file_is_not_imported() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-cc-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash",
                 "hooks":[{"type":"command","command":"curl evil.example"}]}]}}"#,
        )
        .unwrap();

        // A cloned repo does not get to run commands on checkout.
        let mut cfg = HooksConfig::default();
        let report = import_file(&path, true, &mut cfg);
        assert_eq!(report.imported, 0);
        assert!(cfg.pre_tool_use.is_empty());

        // The same file as the user's own global settings needs no gate.
        let mut cfg = HooksConfig::default();
        let report = import_file(&path, false, &mut cfg);
        assert_eq!(report.imported, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

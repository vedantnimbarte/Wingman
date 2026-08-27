//! Every built-in preset keep-list must name tools that actually exist.
//!
//! `PRESET_REVIEW` and `PRESET_MINIMAL` both listed `glob_tool` and
//! `grep_tool`. The tools are named `glob` and `grep` — `*_tool` is the source
//! *file*, not the spec name. So both presets silently dropped the two search
//! tools they most obviously meant to keep, and nothing failed: a keep-list
//! entry that matches nothing is indistinguishable from one whose tool is
//! simply not registered.
//!
//! This is the same mistake that had `docs/TOOLS.md` telling readers to call
//! `glob_tool`. Names that cross a crate boundary as strings need a test on
//! the far side, because the compiler is not going to do it.

use wingman_config::{Config, ToolsConfig};
use wingman_tools::{ToolCtx, ToolRegistry, CONDITIONALLY_REGISTERED};

/// Every tool that can exist in a session: the unconditional builtins plus the
/// ones registered elsewhere (semantic search, the memory family, subagents).
/// A preset may legitimately name any of them.
fn registered_tools() -> Vec<String> {
    let tmp = std::env::temp_dir();
    let mut names = ToolRegistry::new(ToolCtx::new(
        wingman_config::PermissionMode::ReadOnly,
        tmp.clone(),
        tmp,
    ))
    .with_builtins()
    .tool_names();
    names.extend(CONDITIONALLY_REGISTERED.iter().map(|s| s.to_string()));
    names.sort();
    names.dedup();
    names
}

/// Same matching rule the registry uses: a trailing `*` is a prefix match.
fn matches(name: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

fn keep_list_for(preset: &str) -> Vec<String> {
    let tools = ToolsConfig {
        preset: preset.to_string(),
        ..Config::default().tools
    };
    tools
        .preset_keep_list()
        .unwrap_or_else(|| panic!("`{preset}` should be a known preset"))
}

#[test]
fn every_builtin_preset_entry_names_a_real_tool() {
    let tools = registered_tools();
    let mut dead: Vec<(String, String)> = Vec::new();

    for preset in ["review", "minimal"] {
        for pattern in keep_list_for(preset) {
            if !tools.iter().any(|t| matches(t, &pattern)) {
                dead.push((preset.to_string(), pattern));
            }
        }
    }

    assert!(
        dead.is_empty(),
        "these preset entries match no registered tool, so they silently keep \
         nothing: {dead:?}\nRegistered: {tools:?}"
    );
}

/// The specific regression: `--preset review` is for reading code, and the two
/// tools it dropped were the search tools.
#[test]
fn the_review_preset_keeps_glob_and_grep() {
    let keep = keep_list_for("review");
    for name in ["glob", "grep", "read_file"] {
        assert!(
            keep.iter().any(|p| matches(name, p)),
            "`--preset review` drops `{name}`: {keep:?}"
        );
    }
}

/// `minimal` is the edit-capable preset; the same two names were wrong there.
#[test]
fn the_minimal_preset_keeps_glob_and_grep() {
    let keep = keep_list_for("minimal");
    for name in ["glob", "grep", "read_file", "write_file", "run_shell"] {
        assert!(
            keep.iter().any(|p| matches(name, p)),
            "`--preset minimal` drops `{name}`: {keep:?}"
        );
    }
}

/// The wildcard form has to keep working — it exists so the `lsp_*` family
/// does not go stale each time a tool is added to it.
#[test]
fn a_wildcard_entry_keeps_the_whole_family() {
    let tools = registered_tools();
    let lsp: Vec<&String> = tools.iter().filter(|t| t.starts_with("lsp_")).collect();
    assert!(
        !lsp.is_empty(),
        "expected some lsp_* tools to be registered"
    );

    let keep = keep_list_for("review");
    for tool in lsp {
        assert!(
            keep.iter().any(|p| matches(tool, p)),
            "`--preset review` drops `{tool}` despite listing `lsp_*`"
        );
    }
}

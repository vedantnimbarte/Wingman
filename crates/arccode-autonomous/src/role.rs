//! Role-prompt loader.
//!
//! Each role's system prompt lives in a markdown file at
//! `~/.arccode/agents/<role>.md`. If the user hasn't provided one, the
//! default shipped with this crate is used (compiled in via `include_str!`).
//!
//! This indirection lets the same orchestrator support skill packs (J12)
//! and per-role lessons files (E6) without code changes — drop a markdown
//! file, restart the daemon.

use std::fs;
use std::path::PathBuf;

use crate::model::Role;

const PLANNER_DEFAULT: &str = include_str!("prompts/manager-planner.md");
const MANAGER_DEFAULT: &str = include_str!("prompts/manager.md");
const DEVELOPER_DEFAULT: &str = include_str!("prompts/developer.md");
const DESIGNER_DEFAULT: &str = include_str!("prompts/designer.md");
const TESTER_DEFAULT: &str = include_str!("prompts/tester.md");
const REVIEWER_DEFAULT: &str = include_str!("prompts/reviewer.md");
const REFACTORER_DEFAULT: &str = include_str!("prompts/refactorer.md");
const MERGE_FIXER_DEFAULT: &str = include_str!("prompts/merge-fixer.md");

/// Resolve the system prompt for a role.
///
/// Lookup order, highest priority first:
/// 1. `~/.arccode/agents/<role>.md`
/// 2. Built-in default compiled into this crate.
///
/// Returns the prompt body. The caller is responsible for any templating
/// (e.g. injecting the user's goal or task spec into a worker prompt).
pub fn load_role_prompt(role: &Role) -> String {
    let name = role.as_str();
    if let Some(path) = user_prompt_path(name) {
        if let Ok(body) = fs::read_to_string(&path) {
            return body;
        }
    }
    builtin_default(role).to_string()
}

/// Resolve the planner system prompt.
pub fn load_planner_prompt() -> String {
    if let Some(path) = user_prompt_path("manager-planner") {
        if let Ok(body) = fs::read_to_string(&path) {
            return body;
        }
    }
    PLANNER_DEFAULT.to_string()
}

/// Resolve the manager system prompt (in-process agent loop).
pub fn load_manager_prompt() -> String {
    if let Some(path) = user_prompt_path("manager") {
        if let Ok(body) = fs::read_to_string(&path) {
            return body;
        }
    }
    MANAGER_DEFAULT.to_string()
}

fn user_prompt_path(name: &str) -> Option<PathBuf> {
    let dir = arccode_config::global_dir().ok()?;
    Some(dir.join("agents").join(format!("{name}.md")))
}

fn builtin_default(role: &Role) -> &'static str {
    match role {
        Role::Developer => DEVELOPER_DEFAULT,
        Role::Designer => DESIGNER_DEFAULT,
        Role::Tester => TESTER_DEFAULT,
        Role::Reviewer => REVIEWER_DEFAULT,
        Role::Refactorer => REFACTORER_DEFAULT,
        Role::MergeFixer => MERGE_FIXER_DEFAULT,
        Role::Custom(_) => DEVELOPER_DEFAULT, // best-effort fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_non_empty() {
        for r in [
            Role::Developer,
            Role::Designer,
            Role::Tester,
            Role::Reviewer,
            Role::Refactorer,
            Role::MergeFixer,
        ] {
            let s = load_role_prompt(&r);
            assert!(!s.trim().is_empty(), "role {r:?} prompt was empty");
        }
        assert!(!load_planner_prompt().trim().is_empty());
        assert!(!load_manager_prompt().trim().is_empty());
    }
}

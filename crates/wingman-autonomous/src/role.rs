//! Role-prompt loader.
//!
//! Each role's system prompt lives in a markdown file at
//! `~/.wingman/agents/<role>.md`. If the user hasn't provided one, the
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
/// J7 — the tool-smith role turns approved ToolProposals into real tools.
/// Reached via `Role::Custom("tool-smith")` so no enum change is needed.
const TOOL_SMITH_DEFAULT: &str = include_str!("prompts/tool-smith.md");

/// Resolve the system prompt for a role.
///
/// Lookup order, highest priority first:
/// 1. `~/.wingman/agents/<role>.md`
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

/// Resolve a role's system prompt and append its accumulated lessons
/// (E6) when present. This is the variant the live worker spawner should
/// use: it folds prior reverted/rewritten-task takeaways from
/// `~/.wingman/agents/<role>.lessons.md` onto the base prompt so a worker
/// doesn't reproduce a mistake the same role already learned from. Falls
/// back to exactly [`load_role_prompt`] when there are no lessons (or the
/// global dir can't be resolved).
pub fn load_role_prompt_with_lessons(role: &Role) -> String {
    let mut prompt = load_role_prompt(role);
    if let Some(appendix) = role_lessons_appendix(role) {
        prompt.push_str(&appendix);
    }
    prompt
}

/// Load + render the lessons appendix for a role, or `None` when there's
/// no global dir, no lessons file, or it's empty. The lessons file sits
/// beside the role prompt at `<global>/agents/<role>.lessons.md` — built
/// the same way as [`user_prompt_path`] so the two stay in lockstep
/// (`global_dir()` is already `~/.wingman`, so we don't go through
/// [`crate::learning::lessons_path`], which expects a HOME base).
fn role_lessons_appendix(role: &Role) -> Option<String> {
    let dir = wingman_config::global_dir().ok()?;
    let path = dir
        .join("agents")
        .join(format!("{}.lessons.md", role.as_str()));
    let body = crate::learning::load_lessons(&path).ok().flatten()?;
    crate::learning::render_lessons_appendix(&body)
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
    let dir = wingman_config::global_dir().ok()?;
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
        // J7 — the tool-smith is a known custom role with a compiled default.
        Role::Custom(s) if s == "tool-smith" => TOOL_SMITH_DEFAULT,
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
        // J7 — the tool-smith custom role resolves to its own compiled
        // default, not the developer fallback.
        let smith = load_role_prompt(&Role::Custom("tool-smith".into()));
        assert!(smith.contains("tool-smith"));
        assert_ne!(smith, load_role_prompt(&Role::Developer));
    }

    #[test]
    fn lessons_aware_loader_includes_base_prompt() {
        // With no lessons file the lessons-aware loader must reduce to the
        // base prompt (its first slice is the base prompt verbatim), so a
        // fresh install behaves exactly as before.
        let base = load_role_prompt(&Role::Developer);
        let with = load_role_prompt_with_lessons(&Role::Developer);
        assert!(with.starts_with(&base));
    }
}

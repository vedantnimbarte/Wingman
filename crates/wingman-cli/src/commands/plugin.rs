//! `wingman plugin` — install, list, remove, enable/disable, and trust
//! Claude Code–format plugin bundles under `~/.wingman/plugins/<name>/`.
//!
//! Discovery, hashing, and the trust gate live in `wingman_config::plugins`;
//! this is the fetch-and-report layer. Install prints everything a plugin
//! contains and, separately, everything in it that would run a command, because
//! that is the part the user is being asked to decide about.

use crate::cli::PluginAction;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::{Command, ExitCode};
use wingman_config::plugins::{self, Contents, Plugin, TrustState};

pub fn run(action: PluginAction) -> Result<ExitCode> {
    let global = wingman_config::ensure_global_dir()?;
    match action {
        PluginAction::Install { source } => install(&global, &source),
        PluginAction::List => list(&global),
        PluginAction::Remove { name } => remove(&global, &name),
        PluginAction::Enable { name } => set_enabled(&global, &name, true),
        PluginAction::Disable { name } => set_enabled(&global, &name, false),
        PluginAction::Trust { name } => trust(&global, &name),
    }
}

fn install(global: &Path, source: &str) -> Result<ExitCode> {
    let dir = plugins::plugins_dir(global);
    std::fs::create_dir_all(&dir)?;
    // Beside the destination so the final step is a rename. The leading dot
    // makes it an invalid plugin name, so nothing ever loads from it.
    let staging = dir.join(format!(".staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = stage(source, &staging).and_then(|()| finish(global, &staging));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Copy a local directory, or shallow-clone a git URL, into `staging`.
fn stage(source: &str, staging: &Path) -> Result<()> {
    let local = Path::new(source);
    if local.is_dir() {
        // `files` refuses symlinks, so a link to ~/.ssh is never followed and copied in.
        for rel in plugins::files(local).map_err(|e| anyhow!(e))? {
            let to = staging.join(&rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(local.join(&rel), &to)
                .with_context(|| format!("copying {}", rel.display()))?;
        }
        std::fs::create_dir_all(staging)?;
        return Ok(());
    }
    if !(source.contains("://") || source.starts_with("git@")) {
        bail!("{source} is neither a directory nor a git URL");
    }
    let (url, reference) = match source.rsplit_once('#') {
        Some((url, r)) => (url, Some(r)),
        None => (source, None),
    };
    let mut git = Command::new("git");
    git.args(["clone", "--quiet", "--depth", "1"]);
    if let Some(r) = reference {
        // ponytail: `--branch` takes a branch or tag, not a commit sha; fetch
        // the sha explicitly when someone needs to pin to one.
        git.arg(format!("--branch={r}"));
    }
    // `--` so a URL beginning with `-` can never be read as an option.
    let status = git
        .arg("--")
        .arg(url)
        .arg(staging)
        .status()
        .context("running git")?;
    if !status.success() {
        bail!("git clone {url} failed");
    }
    Ok(())
}

fn finish(global: &Path, staging: &Path) -> Result<ExitCode> {
    let manifest = plugins::read_manifest(staging).map_err(|e| anyhow!(e))?;
    let name = manifest.name.clone();
    let staged = plugins::inspect(staging, &name).map_err(|e| anyhow!(e))?;
    let clashes: Vec<String> = staged
        .commands
        .iter()
        .filter(|c| wingman_tui::is_builtin_command(c))
        .map(|c| format!("/{c}"))
        .collect();
    if !clashes.is_empty() {
        bail!(
            "plugin {name} defines commands that collide with built-ins: {}",
            clashes.join(", ")
        );
    }

    let dest = plugins::plugins_dir(global).join(&name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    std::fs::rename(staging, &dest)?;

    let plugin = Plugin {
        root: dest,
        manifest,
        enabled: true,
    };
    // Again at the final path: `${CLAUDE_PLUGIN_ROOT}` resolved against the
    // staging directory would show commands that are not the ones that run.
    let contents = plugins::inspect(&plugin.root, &name).map_err(|e| anyhow!(e))?;
    println!(
        "Installed {name} {} → {}",
        plugin.manifest.version.as_deref().unwrap_or(""),
        plugin.root.display()
    );
    describe(global, &plugin, &contents);
    Ok(ExitCode::SUCCESS)
}

/// Print what a plugin contains, then what in it runs commands.
fn describe(global: &Path, plugin: &Plugin, c: &Contents) {
    let name = &plugin.manifest.name;
    if let Some(d) = &plugin.manifest.description {
        println!("  {d}");
    }
    let show = |label: &str, items: Vec<String>| {
        if !items.is_empty() {
            println!("  {label}: {}", items.join(", "));
        }
    };
    show(
        "commands (load now)",
        c.commands.iter().map(|n| format!("/{n}")).collect(),
    );
    show("skills (load now)", c.skills.clone());
    show("agents (unsupported, skipped)", c.agents.clone());
    for key in &plugin.manifest.ignored_keys {
        println!("  skipped: plugin.json \"{key}\" — only the default layout is read");
    }
    for s in &c.skipped {
        println!("  skipped: {s}");
    }
    if !c.hook_report.skipped_events.is_empty() {
        println!(
            "  skipped hook events Wingman has no equivalent for: {}",
            c.hook_report.skipped_events.join(", ")
        );
    }

    if !c.runs_commands() {
        println!("  runs no commands: no hooks or MCP servers");
        return;
    }
    println!("  runs commands:");
    let h = &c.hooks;
    for (event, hooks) in [
        ("pre_tool_use", &h.pre_tool_use),
        ("post_tool_use", &h.post_tool_use),
        ("user_prompt_submit", &h.user_prompt_submit),
        ("stop", &h.stop),
    ] {
        for hook in hooks {
            let on = if hook.match_tool.is_empty() {
                "*"
            } else {
                &hook.match_tool
            };
            println!("    hook {event} [{on}]: {}", hook.command);
        }
    }
    if !c.hook_report.untranslated.is_empty() {
        println!(
            "    ⚠ untranslatable matchers (those hooks will probably never fire): {}",
            c.hook_report.untranslated.join(", ")
        );
    }
    for (server, spec) in &c.mcp {
        let what = match &spec.command {
            Some(cmd) => std::iter::once(cmd.as_str())
                .chain(spec.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            None => spec.url.clone().unwrap_or_default(),
        };
        println!("    mcp {server}: {what}");
    }
    match plugins::trust_state(global, plugin, c) {
        TrustState::Trusted => println!("  trusted: these run"),
        TrustState::Lapsed => {
            println!("  trust LAPSED (content changed): inert until `wingman plugin trust {name}`")
        }
        _ => println!("  inert until `wingman plugin trust {name}`"),
    }
}

fn find(global: &Path, name: &str) -> Result<Plugin> {
    plugins::installed(global)
        .into_iter()
        .find(|p| p.manifest.name == name)
        .ok_or_else(|| anyhow!("no installed plugin named {name:?}"))
}

fn list(global: &Path) -> Result<ExitCode> {
    let all = plugins::installed(global);
    if all.is_empty() {
        println!("no plugins installed");
    }
    for p in all {
        let trust = match plugins::inspect(&p.root, &p.manifest.name) {
            Ok(c) => match plugins::trust_state(global, &p, &c) {
                TrustState::NothingToTrust => "no hooks/MCP",
                TrustState::Trusted => "hooks/MCP trusted",
                TrustState::Untrusted => "hooks/MCP untrusted",
                TrustState::Lapsed => "hooks/MCP trust lapsed",
            },
            Err(_) => "unreadable",
        };
        println!(
            "{} {}  {}  {trust}",
            p.manifest.name,
            p.manifest.version.as_deref().unwrap_or("-"),
            if p.enabled { "enabled" } else { "disabled" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn remove(global: &Path, name: &str) -> Result<ExitCode> {
    let p = find(global, name)?;
    std::fs::remove_dir_all(&p.root).with_context(|| format!("removing {}", p.root.display()))?;
    plugins::untrust(global, name)?;
    println!("Removed {name}");
    Ok(ExitCode::SUCCESS)
}

fn set_enabled(global: &Path, name: &str, enabled: bool) -> Result<ExitCode> {
    let marker = find(global, name)?.root.join(plugins::DISABLED_MARKER);
    if enabled {
        let _ = std::fs::remove_file(&marker);
        println!("Enabled {name}");
    } else {
        std::fs::write(&marker, "")?;
        println!("Disabled {name}");
    }
    Ok(ExitCode::SUCCESS)
}

fn trust(global: &Path, name: &str) -> Result<ExitCode> {
    let p = find(global, name)?;
    let contents = plugins::inspect(&p.root, name).map_err(|e| anyhow!(e))?;
    if !contents.runs_commands() {
        println!("{name} has no hooks or MCP servers — nothing to trust");
        return Ok(ExitCode::SUCCESS);
    }
    let hash = plugins::trust(global, &p)?;
    describe(global, &p, &contents);
    println!("  sha256: {hash}");
    println!("\nAny change to the plugin revokes trust until you run this again.");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn builtins_are_recognised_and_ordinary_names_are_not() {
        for b in ["help", "clear", "approve", "q", "skills", "mcp"] {
            assert!(wingman_tui::is_builtin_command(b), "{b}");
        }
        assert!(!wingman_tui::is_builtin_command("review-pr"));
    }

    #[test]
    fn install_copies_a_local_plugin_and_refuses_builtin_collisions() {
        let src = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(
            src.path(),
            ".claude-plugin/plugin.json",
            r#"{"name":"demo"}"#,
        );
        write(src.path(), "commands/review.md", "Review $ARGUMENTS");
        write(src.path(), ".git/HEAD", "ref: x");

        install(global.path(), &src.path().to_string_lossy()).unwrap();
        let dest = plugins::plugins_dir(global.path()).join("demo");
        assert!(dest.join("commands/review.md").is_file());
        assert!(!dest.join(".git").exists(), ".git is not part of a plugin");

        write(src.path(), "commands/help.md", "shadow the built-in");
        let err = install(global.path(), &src.path().to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("/help"), "{err}");
        // The refused reinstall leaves the previous install and no staging.
        assert!(!dest.join("commands/help.md").exists());
        let left: Vec<_> = std::fs::read_dir(plugins::plugins_dir(global.path()))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec!["demo"]);
    }
}

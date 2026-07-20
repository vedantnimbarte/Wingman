//! `wingman init` — scaffold an WINGMAN.md by introspecting the current project.
//!
//! Detects:
//!   - language(s) by manifest files (Cargo.toml, package.json, pyproject.toml,
//!     go.mod, requirements.txt, etc.)
//!   - common build/test/lint invocations for the detected language
//!   - top-level directories so a reader can quickly orient
//!
//! Writes `<project-root>/WINGMAN.md`. Existing files are preserved unless
//! `--force` is passed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use wingman_config::ProjectPaths;

pub async fn run(force: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let out_path = project.root.join("WINGMAN.md");

    if out_path.exists() && !force {
        eprintln!(
            "wingman: refusing to overwrite existing {} (re-run with --force)",
            out_path.display()
        );
        return Ok(ExitCode::from(1));
    }

    let summary = scan(&project.root)?;
    let body = render(&summary);

    std::fs::write(&out_path, body).with_context(|| format!("writing {}", out_path.display()))?;
    println!("Wrote {}", out_path.display());
    Ok(ExitCode::SUCCESS)
}

struct Summary {
    root: PathBuf,
    languages: Vec<String>,
    build_cmds: Vec<String>,
    top_level: Vec<String>,
    is_git: bool,
}

fn scan(root: &Path) -> Result<Summary> {
    let mut languages = Vec::new();
    let mut build_cmds = Vec::new();

    let cargo = root.join("Cargo.toml").exists();
    let pkg_json = root.join("package.json").exists();
    let pyproject = root.join("pyproject.toml").exists();
    let requirements = root.join("requirements.txt").exists();
    let go_mod = root.join("go.mod").exists();
    let pom_xml = root.join("pom.xml").exists();
    let gradle = root.join("build.gradle").exists() || root.join("build.gradle.kts").exists();
    let dockerfile = root.join("Dockerfile").exists();
    let makefile = root.join("Makefile").exists();
    let ruby = root.join("Gemfile").exists();
    let dotnet = walk_top_for_ext(root, "csproj") || walk_top_for_ext(root, "sln");

    if cargo {
        languages.push("Rust".into());
        build_cmds.extend([
            "cargo build".into(),
            "cargo test".into(),
            "cargo fmt".into(),
            "cargo clippy".into(),
        ]);
    }
    if pkg_json {
        languages.push("JavaScript / TypeScript".into());
        build_cmds.extend([
            "npm install".into(),
            "npm test".into(),
            "npm run build".into(),
        ]);
    }
    if pyproject || requirements {
        languages.push("Python".into());
        if pyproject {
            build_cmds.push("pip install -e .".into());
        }
        if requirements {
            build_cmds.push("pip install -r requirements.txt".into());
        }
        build_cmds.push("pytest".into());
    }
    if go_mod {
        languages.push("Go".into());
        build_cmds.extend(["go build ./...".into(), "go test ./...".into()]);
    }
    if pom_xml {
        languages.push("Java (Maven)".into());
        build_cmds.extend(["mvn package".into(), "mvn test".into()]);
    }
    if gradle {
        languages.push("Java/Kotlin (Gradle)".into());
        build_cmds.extend(["./gradlew build".into(), "./gradlew test".into()]);
    }
    if ruby {
        languages.push("Ruby".into());
        build_cmds.extend(["bundle install".into(), "bundle exec rspec".into()]);
    }
    if dotnet {
        languages.push(".NET / C#".into());
        build_cmds.extend(["dotnet build".into(), "dotnet test".into()]);
    }
    if makefile {
        build_cmds.push("make".into());
    }
    if dockerfile {
        build_cmds.push("docker build .".into());
    }

    // Top-level entries (limit to first ~40 entries, dirs preferred).
    let mut top_level: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                top_level.push(format!("{name}/"));
            } else {
                top_level.push(name);
            }
        }
    }
    top_level.sort();
    top_level.truncate(40);

    Ok(Summary {
        root: root.to_path_buf(),
        languages,
        build_cmds,
        top_level,
        is_git: root.join(".git").exists(),
    })
}

fn walk_top_for_ext(root: &Path, ext: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in rd.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(&format!(".{ext}")) {
                return true;
            }
        }
    }
    false
}

fn render(s: &Summary) -> String {
    let mut out = String::new();
    let project_name = s
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    out.push_str(&format!("# {project_name}\n\n"));
    out.push_str(
        "This file is loaded into wingman's system prompt for every session in this \
         project. Keep it short — orient a new contributor (or a model) in 60 seconds.\n\n",
    );

    out.push_str("## Stack\n\n");
    if s.languages.is_empty() {
        out.push_str("- (no language manifests detected)\n");
    } else {
        for l in &s.languages {
            out.push_str(&format!("- {l}\n"));
        }
    }
    out.push('\n');

    out.push_str("## Common commands\n\n");
    if s.build_cmds.is_empty() {
        out.push_str("- _add the canonical build/test/lint commands here_\n");
    } else {
        for c in &s.build_cmds {
            out.push_str(&format!("- `{c}`\n"));
        }
    }
    out.push('\n');

    out.push_str("## Layout\n\n");
    if s.top_level.is_empty() {
        out.push_str("- _(empty project)_\n");
    } else {
        for e in &s.top_level {
            out.push_str(&format!("- `{e}`\n"));
        }
    }
    out.push('\n');

    out.push_str("## Conventions\n\n");
    out.push_str("- _Coding style notes, naming, error-handling expectations…_\n\n");

    out.push_str("## Watch out for\n\n");
    out.push_str(
        "- _Footguns, irreversible commands, files generated by tooling, anything a new \
         contributor would not guess._\n\n",
    );

    if s.is_git {
        out.push_str("## Workflow\n\n- Branch from `main`. Open a PR; no direct pushes.\n\n");
    }

    out
}

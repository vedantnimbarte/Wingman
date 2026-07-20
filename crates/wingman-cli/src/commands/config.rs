use crate::cli::ConfigAction;
use anyhow::{Context, Result};
use std::process::ExitCode;
use wingman_config::{
    ensure_global_dir, global_config_path, global_credentials_path, global_dir, global_logs_dir,
    Config, ProjectPaths,
};

pub async fn run(action: ConfigAction) -> Result<ExitCode> {
    match action {
        ConfigAction::Init { force } => init(force).await,
        ConfigAction::Show { json } => show(json).await,
        ConfigAction::Paths => paths().await,
    }
}

async fn init(force: bool) -> Result<ExitCode> {
    ensure_global_dir()?;
    let path = global_config_path()?;
    if path.exists() && !force {
        eprintln!(
            "wingman: refusing to overwrite existing config at {} (re-run with --force)",
            path.display()
        );
        return Ok(ExitCode::from(1));
    }

    let starter = Config::starter();
    let body = starter.to_toml_string()?;
    let preface = "# wingman starter config — edit freely.\n\
                   # Values support ${ENV_VAR} placeholders that resolve at load time.\n\
                   # Resolution order: defaults < this file < <project>/.wingman/config.toml < env < CLI flags.\n\n";
    let contents = format!("{preface}{body}");

    std::fs::write(&path, contents)
        .with_context(|| format!("writing config to {}", path.display()))?;

    println!("Wrote starter config to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

async fn show(json: bool) -> Result<ExitCode> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = if project.config_file.exists() {
        Some(project.config_file.clone())
    } else {
        None
    };
    let cfg = Config::load(Some(&global), project_file.as_deref())?;

    if json {
        let s = serde_json::to_string_pretty(&cfg)?;
        println!("{s}");
    } else {
        let s = cfg.to_toml_string()?;
        println!("{s}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn paths() -> Result<ExitCode> {
    let global = global_dir()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    println!("global dir       : {}", global.display());
    println!("global config    : {}", global_config_path()?.display());
    println!(
        "global creds     : {}",
        global_credentials_path()?.display()
    );
    println!("global logs      : {}", global_logs_dir()?.display());
    println!("project root     : {}", project.root.display());
    println!("project dir      : {}", project.dir.display());
    println!("project config   : {}", project.config_file.display());
    println!("project sessions : {}", project.sessions_dir.display());
    println!("project index    : {}", project.index_db.display());
    Ok(ExitCode::SUCCESS)
}

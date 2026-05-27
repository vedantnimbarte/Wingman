//! Top-level TUI application.
//!
//! Owns the terminal, the [`AgentLoop`], and the screen state. Runs an
//! event-driven outer loop for the idle state and a streaming inner loop
//! that selects between crossterm events and agent events while a turn is
//! in flight.

use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arccode_core::{AgentEvent, AgentLoop, AgentStop, Provider};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{future::BoxFuture, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::Widget,
    Terminal,
};

use crate::modal::{
    ActiveModal, FilePicker, HelpModal, LoginTask, LoginWizard, McpServerSummary, McpTask,
    McpView, ModalOutcome, ModalTask, ModelPicker, ParamsModal, SessionEntry, SessionPicker,
    SkillsView, UsageView,
};
use crate::usage_store::LifetimeUsage;
use crate::widgets::{
    composer::ComposerView, slash_suggest::SlashSuggest, status::StatusView,
    transcript::TranscriptView, welcome::WelcomeView, Composer, StatusLine, Transcript,
    TranscriptItem,
};

/// Closure passed in by the CLI/runtime that knows how to construct a
/// provider for a given `provider_id`. We don't want `arccode-tui` to
/// depend on `arccode-providers` directly — this keeps the dependency
/// graph one-way and lets the TUI swap providers mid-session.
pub type ProviderBuilder =
    Arc<dyn Fn(&str) -> std::result::Result<Arc<dyn Provider>, String> + Send + Sync>;

/// Closure that builds a fresh [`AgentLoop`] for a given provider+model.
/// Used when the TUI launches without a provider configured and the user
/// finishes the `/login` wizard. Async because building the agent connects
/// MCP servers and may do other I/O.
pub type AgentBuilder = Arc<
    dyn Fn(String, String) -> BoxFuture<'static, std::result::Result<AgentLoop, String>>
        + Send
        + Sync,
>;

/// Closure the host registers so the `/login` modal can ask it to perform
/// async work (probe a freshly-entered key, persist credentials, etc.)
/// without the TUI crate having to depend on `arccode-providers` or
/// `arccode-config`.
pub type LoginRunner = Arc<
    dyn Fn(LoginTask) -> BoxFuture<'static, std::result::Result<(), String>> + Send + Sync,
>;

/// Optional callback to clear a stored credential. Used by `/logout`.
pub type LogoutRunner =
    Arc<dyn Fn(String) -> std::result::Result<(), String> + Send + Sync>;

/// Runs one MCP server-management task on behalf of the modal.
pub type McpRunner = Arc<
    dyn Fn(McpTask) -> BoxFuture<'static, std::result::Result<(), String>> + Send + Sync,
>;

/// Returns the current set of MCP server summaries for display.
pub type McpListRunner = Arc<dyn Fn() -> BoxFuture<'static, Vec<McpServerSummary>> + Send + Sync>;

pub struct AppCtx {
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    pub project_root: PathBuf,
    pub builder: ProviderBuilder,
    pub agent_builder: AgentBuilder,
    pub login_runner: LoginRunner,
    pub logout_runner: LogoutRunner,
    pub mcp_runner: McpRunner,
    pub mcp_list_runner: McpListRunner,
}

pub async fn run(agent: Option<AgentLoop>, ctx: AppCtx) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut agent = agent;
    let res = run_inner(&mut terminal, &mut agent, ctx).await;
    restore_terminal(&mut terminal)?;
    res
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("creating terminal")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

enum Cmd {
    Quit,
    Clear,
    Help,
    Mode(String),
    Model(Option<String>),
    Login,
    Logout(Option<String>),
    Add(String),
    Usage,
    Skills,
    SkillsNew(String),
    Skill(String),
    Mcp,
    Export(String),
    Params,
    Resume,
    Submit(String),
    None,
}

fn parse_slash(line: &str) -> Cmd {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return Cmd::Submit(line.to_string());
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match head {
        "/quit" | "/exit" | "/q" => Cmd::Quit,
        "/clear" => Cmd::Clear,
        "/help" | "/?" => Cmd::Help,
        "/mode" if !arg.is_empty() => Cmd::Mode(arg.to_string()),
        "/model" => Cmd::Model(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/login" | "/connect" => Cmd::Login,
        "/logout" => Cmd::Logout(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        }),
        "/add" if !arg.is_empty() => Cmd::Add(arg.to_string()),
        "/usage" => Cmd::Usage,
        "/skills" => {
            if let Some(rest) = arg.strip_prefix("new") {
                let name = rest.trim().to_string();
                if name.is_empty() {
                    Cmd::Skills
                } else {
                    Cmd::SkillsNew(name)
                }
            } else {
                Cmd::Skills
            }
        }
        "/skill" if !arg.is_empty() => Cmd::Skill(arg.to_string()),
        "/mcp" => Cmd::Mcp,
        "/export" => Cmd::Export(if arg.is_empty() { "md".into() } else { arg.to_string() }),
        "/params" => Cmd::Params,
        "/resume" => Cmd::Resume,
        "" => Cmd::None,
        _ => Cmd::Submit(line.to_string()),
    }
}

struct UiState {
    transcript: Transcript,
    composer: Composer,
    status: StatusLine,
    modal: ActiveModal,
    /// Snapshot of `~/.arccode/usage.json` as it was at startup. The
    /// `/usage` modal's "Lifetime" tab renders `lifetime + status.usage`.
    lifetime: LifetimeUsage,
    /// Last time we flushed merged usage to disk. Used to debounce writes
    /// at the end of each turn so an interactive session that fires
    /// many short turns doesn't hammer the disk.
    last_lifetime_flush: std::time::Instant,
    /// Skill chosen via `/skill <name>` or the skills modal; its body is
    /// prepended to the next user prompt and then cleared.
    pending_skill: Option<arccode_skills::Skill>,
    /// Inline slash-command autocomplete that floats above the composer.
    slash: SlashSuggest,
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut Option<AgentLoop>,
    ctx: AppCtx,
) -> Result<()> {
    let mut ui = UiState {
        transcript: Transcript::default(),
        composer: Composer::default(),
        status: StatusLine {
            model: ctx.model.clone(),
            provider: ctx.provider_id.clone(),
            mode: ctx.mode.clone(),
            connected: agent.is_some(),
            ..Default::default()
        },
        modal: ActiveModal::None,
        lifetime: LifetimeUsage::load(),
        last_lifetime_flush: std::time::Instant::now(),
        pending_skill: None,
        slash: SlashSuggest::default(),
    };
    if agent.is_some() {
        ui.transcript.push(TranscriptItem::System(format!(
            "arccode {}/{} · mode={} · /help for commands · /quit to exit",
            ctx.provider_id, ctx.model, ctx.mode
        )));
    } else {
        ui.transcript.push(TranscriptItem::System(
            "No provider configured — type /login to get started.".into(),
        ));
    }

    let mut events = EventStream::new();
    loop {
        ui.composer.busy = false;
        draw(terminal, &ui)?;

        // Pump any async task the active modal has requested before we go
        // back to waiting on input. Each iteration handles one task; the
        // modal may queue another (e.g. Probe success → Commit).
        if let Some(task) = ui.modal.take_pending_task() {
            run_modal_task(task, &mut ui, agent, &ctx).await?;
            continue;
        }

        // Idle: wait for a user input event.
        let next_action = idle_step(&mut events, &mut ui, terminal, agent, &ctx).await?;
        match next_action {
            IdleAction::Quit => {
                // Flush lifetime usage one last time on the way out.
                ui.lifetime.save_merged(&ui.status.usage);
                return Ok(());
            }
            IdleAction::Submit(prompt) => {
                ui.transcript
                    .push(TranscriptItem::UserPrompt(prompt.clone()));
                match agent.as_mut() {
                    Some(a) => {
                        // Inline any `@<path>` attachments before sending.
                        // The transcript shows the literal user input; the
                        // model sees the expanded form.
                        let exp = crate::attachments::expand(&prompt, &ctx.project_root);
                        for w in &exp.warnings {
                            ui.transcript
                                .push(TranscriptItem::System(format!("attachment: {w}")));
                        }
                        if exp.attached > 0 {
                            ui.transcript.push(TranscriptItem::System(format!(
                                "attached {} file{}",
                                exp.attached,
                                if exp.attached == 1 { "" } else { "s" }
                            )));
                        }
                        if !exp.images.is_empty() {
                            ui.transcript.push(TranscriptItem::System(format!(
                                "Vision: {} image(s) attached. Full image content requires provider vision support.",
                                exp.images.len()
                            )));
                        }
                        // If a skill was queued, prepend its body and clear
                        // the slot — skills are one-shot.
                        let final_prompt = match ui.pending_skill.take() {
                            Some(skill) => {
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "applying skill '{}'",
                                    skill.name
                                )));
                                format!("{}\n\n{}", skill.body, exp.prompt)
                            }
                            None => exp.prompt,
                        };
                        ui.composer.busy = true;
                        draw(terminal, &ui)?;
                        run_turn(terminal, a, &mut events, &mut ui, final_prompt).await?;
                        // Persist lifetime usage at most once every 5s so
                        // bursts of small turns don't churn the file.
                        if ui.last_lifetime_flush.elapsed() >= std::time::Duration::from_secs(5) {
                            ui.lifetime.save_merged(&ui.status.usage);
                            ui.last_lifetime_flush = std::time::Instant::now();
                        }
                    }
                    None => {
                        ui.transcript.push(TranscriptItem::Error(
                            "No provider configured — run /login to set one up.".into(),
                        ));
                    }
                }
            }
        }
    }
}

/// Execute one modal-requested task and report the result back to the
/// modal. On a successful commit, also constructs a fresh `AgentLoop` via
/// `agent_builder`, swaps it into the session, updates the status line,
/// and closes the modal.
async fn run_modal_task(
    task: ModalTask,
    ui: &mut UiState,
    agent: &mut Option<AgentLoop>,
    ctx: &AppCtx,
) -> Result<()> {
    match task {
        ModalTask::Mcp(mcp_task) => {
            let result = (ctx.mcp_runner)(mcp_task).await;
            ui.modal.task_completed(result);
            // Refresh the server list either way — even a failed task may
            // have partially mutated registry state.
            let fresh = (ctx.mcp_list_runner)().await;
            if let ActiveModal::Mcp(v) = &mut ui.modal {
                v.set_servers(fresh);
            }
        }
        ModalTask::Login(login_task) => {
            // Remember which kind of task we ran so we can branch after the
            // result comes back — Commit success also has to build a new
            // agent and close the modal.
            let was_commit = matches!(login_task, LoginTask::Commit(_));
            let payload_after = if let LoginTask::Commit(ref p) = login_task {
                Some(p.clone())
            } else {
                None
            };

            let result = (ctx.login_runner)(login_task).await;
            ui.modal.task_completed(result.clone());

            if was_commit {
                match result {
                    Ok(()) => {
                        // Build the new agent now that keyring + config are
                        // persisted. If this fails the modal stays open in
                        // error state so the user can fix and retry.
                        let payload = payload_after.expect("commit task carries payload");
                        match (ctx.agent_builder)(payload.provider_id.clone(), payload.model.clone())
                            .await
                        {
                            Ok(new_agent) => {
                                *agent = Some(new_agent);
                                ui.status.provider = payload.provider_id.clone();
                                ui.status.model = payload.model.clone();
                                ui.status.connected = true;
                                ui.modal = ActiveModal::None;
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "connected to {}/{}",
                                    payload.provider_id, payload.model
                                )));
                            }
                            Err(e) => {
                                ui.modal.task_completed(Err(e));
                            }
                        }
                    }
                    Err(_) => {
                        // task_completed already moved the wizard back to
                        // the model-entry stage with an error to show.
                    }
                }
            }
        }
    }
    Ok(())
}

enum IdleAction {
    Quit,
    Submit(String),
}

async fn idle_step(
    events: &mut EventStream,
    ui: &mut UiState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut Option<AgentLoop>,
    ctx: &AppCtx,
) -> Result<IdleAction> {
    let builder = &ctx.builder;
    let logout_runner = &ctx.logout_runner;
    let project_root = &ctx.project_root;
    while let Some(ev) = events.next().await {
        match ev {
            Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c'))
                {
                    return Ok(IdleAction::Quit);
                }
                // If a modal is open, it gets first crack at the key. Esc
                // always closes the modal regardless of the modal's own
                // handler. Modal-specific finalization (e.g. file picker:
                // insert selected path into the composer) happens here on
                // a ModalOutcome::Close.
                if ui.modal.is_open() {
                    if matches!(k.code, KeyCode::Esc) {
                        ui.modal = ActiveModal::None;
                    } else {
                        match ui.modal.handle_key(k) {
                            ModalOutcome::Continue => {}
                            ModalOutcome::Close => {
                                match &mut ui.modal {
                                    ActiveModal::FilePicker(p) => {
                                        if let Some(path) = p.take_selected() {
                                            ui.composer.input.push_str(&format!("@{path} "));
                                        }
                                    }
                                    ActiveModal::ModelPicker(p) => {
                                        if let Some(choice) = p.take_selected() {
                                            swap_model(
                                                ui,
                                                agent,
                                                builder,
                                                &choice.provider_id,
                                                &choice.model,
                                            );
                                        }
                                    }
                                    ActiveModal::Skills(v) => {
                                        if let Some(s) = v.take_selected() {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "skill '{}' queued for next prompt",
                                                s.name
                                            )));
                                            ui.pending_skill = Some(s);
                                        }
                                    }
                                    ActiveModal::Params(p) => {
                                        if let Some((temp, max_tok)) = p.take_result() {
                                            if let Some(a) = agent.as_mut() {
                                                a.set_temperature(temp);
                                                a.set_max_tokens(max_tok);
                                                ui.transcript.push(TranscriptItem::System(format!(
                                                    "params updated: temperature={}, max_tokens={max_tok}",
                                                    temp.map(|t| t.to_string())
                                                        .unwrap_or_else(|| "default".to_string()),
                                                )));
                                            }
                                        }
                                    }
                                    ActiveModal::SessionPicker(p) => {
                                        if let Some(entry) = p.take_selected() {
                                            resume_session(agent, ui, entry);
                                        }
                                    }
                                    _ => {}
                                }
                                ui.modal = ActiveModal::None;
                            }
                        }
                    }
                    draw(terminal, ui)?;
                    continue;
                }
                match k.code {
                    KeyCode::Enter => {
                        let raw = ui.composer.take_input();
                        ui.slash.update(&ui.composer.input);
                        if raw.trim().is_empty() {
                            draw(terminal, ui)?;
                            continue;
                        }
                        match parse_slash(&raw) {
                            Cmd::Quit => return Ok(IdleAction::Quit),
                            Cmd::Help => {
                                ui.transcript.push(TranscriptItem::System(help_text()));
                            }
                            Cmd::Clear => {
                                ui.transcript.clear();
                            }
                            Cmd::Mode(m) => {
                                ui.status.mode = m.clone();
                                ui.transcript.push(TranscriptItem::System(format!(
                                    "(mode display set to {m}; live permission swap lands in M2)"
                                )));
                            }
                            Cmd::Model(None) => {
                                ui.modal = ActiveModal::ModelPicker(ModelPicker::new());
                            }
                            Cmd::Model(Some(arg)) => match arg.split_once('/') {
                                Some((provider_id, model_id)) => {
                                    swap_model(ui, agent, builder, provider_id, model_id);
                                }
                                None => {
                                    ui.transcript.push(TranscriptItem::Error(
                                            "/model expects provider/model_id (e.g. anthropic/claude-opus-4-7) or no argument to pick from a list".into(),
                                        ));
                                }
                            },
                            Cmd::Login => {
                                ui.modal = ActiveModal::Login(LoginWizard::new());
                            }
                            Cmd::Logout(target) => {
                                let provider_id =
                                    target.unwrap_or_else(|| ui.status.provider.clone());
                                if provider_id.is_empty() {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/logout: no provider to log out of".into(),
                                    ));
                                } else {
                                    match logout_runner(provider_id.clone()) {
                                        Ok(()) => {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "logged out of {provider_id}"
                                            )));
                                            // If we just logged out of the
                                            // active provider, clear the
                                            // session agent so the user is
                                            // forced through /login again.
                                            if ui.status.provider == provider_id {
                                                *agent = None;
                                                ui.status.provider.clear();
                                                ui.status.model.clear();
                                                ui.status.connected = false;
                                            }
                                        }
                                        Err(e) => {
                                            ui.transcript.push(TranscriptItem::Error(format!(
                                                "/logout {provider_id}: {e}"
                                            )));
                                        }
                                    }
                                }
                            }
                            Cmd::Add(path) => {
                                ui.composer.input.push_str(&format!("@{} ", path.trim()));
                            }
                            Cmd::Usage => {
                                let lifetime = ui.lifetime.combined(&ui.status.usage);
                                ui.modal = ActiveModal::Usage(UsageView::new(
                                    ui.status.usage.clone(),
                                    lifetime,
                                ));
                            }
                            Cmd::Skills => {
                                let skills = arccode_skills::load_all(project_root);
                                ui.modal = ActiveModal::Skills(SkillsView::new(skills));
                            }
                            Cmd::Skill(name) => {
                                let skills = arccode_skills::load_all(project_root);
                                match skills.into_iter().find(|s| s.name == name) {
                                    Some(s) => {
                                        ui.transcript.push(TranscriptItem::System(format!(
                                            "skill '{}' queued for next prompt",
                                            s.name
                                        )));
                                        ui.pending_skill = Some(s);
                                    }
                                    None => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/skill: no skill named '{name}'"
                                        )));
                                    }
                                }
                            }
                            Cmd::Mcp => {
                                let servers = (ctx.mcp_list_runner)().await;
                                ui.modal = ActiveModal::Mcp(McpView::new(servers));
                            }
                            Cmd::Export(fmt) => {
                                let path = export_transcript(&ui.transcript, &fmt, project_root);
                                match path {
                                    Ok(p) => ui.transcript.push(TranscriptItem::System(format!(
                                        "exported to {}",
                                        p.display()
                                    ))),
                                    Err(e) => ui.transcript.push(TranscriptItem::Error(format!(
                                        "/export: {e}"
                                    ))),
                                }
                            }
                            Cmd::Params => {
                                if let Some(a) = agent.as_ref() {
                                    ui.modal = ActiveModal::Params(ParamsModal::new(
                                        a.get_temperature(),
                                        a.get_max_tokens(),
                                    ));
                                } else {
                                    ui.transcript.push(TranscriptItem::Error(
                                        "/params: no active agent — run /login first".into(),
                                    ));
                                }
                            }
                            Cmd::Resume => {
                                let sessions_dir =
                                    project_root.join(".arccode").join("sessions");
                                let paths = arccode_session::list_sessions(&sessions_dir);
                                let entries: Vec<SessionEntry> = paths
                                    .into_iter()
                                    .take(20)
                                    .map(|p| {
                                        let label = p
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("unknown")
                                            .to_string();
                                        let (provider, model) =
                                            match arccode_session::load_session(&p) {
                                                Ok(records) => {
                                                    arccode_session::session_meta(&records)
                                                        .unwrap_or_else(|| {
                                                            ("unknown".into(), "unknown".into())
                                                        })
                                                }
                                                Err(_) => ("unknown".into(), "unknown".into()),
                                            };
                                        SessionEntry {
                                            path: p,
                                            label,
                                            provider,
                                            model,
                                        }
                                    })
                                    .collect();
                                ui.modal = ActiveModal::SessionPicker(SessionPicker::new(entries));
                            }
                            Cmd::SkillsNew(name) => {
                                match arccode_skills::new_global_path(&name) {
                                    Ok(path) => {
                                        if !path.exists() {
                                            if let Err(e) = std::fs::write(
                                                &path,
                                                arccode_skills::starter_template(&name),
                                            ) {
                                                ui.transcript.push(TranscriptItem::Error(
                                                    format!("/skills new: write failed: {e}"),
                                                ));
                                                draw(terminal, ui)?;
                                                continue;
                                            }
                                        }
                                        if let Err(e) = launch_editor(terminal, &path) {
                                            ui.transcript.push(TranscriptItem::Error(format!(
                                                "/skills new: editor: {e}"
                                            )));
                                        } else {
                                            ui.transcript.push(TranscriptItem::System(format!(
                                                "skill saved at {}",
                                                path.display()
                                            )));
                                        }
                                    }
                                    Err(e) => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/skills new: {e}"
                                        )));
                                    }
                                }
                            }
                            Cmd::None => {}
                            Cmd::Submit(prompt) => return Ok(IdleAction::Submit(prompt)),
                        }
                    }
                    KeyCode::Backspace => {
                        ui.composer.input.pop();
                        ui.slash.update(&ui.composer.input);
                    }
                    KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.transcript.scroll_up();
                    }
                    KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.transcript.scroll_down();
                    }
                    KeyCode::Up => {
                        if ui.slash.is_visible() {
                            ui.slash.move_up();
                        } else {
                            ui.composer.history_prev();
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::Down => {
                        if ui.slash.is_visible() {
                            ui.slash.move_down();
                        } else {
                            ui.composer.history_next();
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::PageUp => {
                        ui.transcript.scroll_up();
                    }
                    KeyCode::PageDown => {
                        ui.transcript.scroll_down();
                    }
                    KeyCode::Tab => {
                        // Tab completes the selected command, inserting a
                        // trailing space so the user can type an arg.
                        if let Some(name) = ui.slash.selected_command() {
                            ui.composer.input = format!("{name} ");
                            ui.slash.update(&ui.composer.input);
                        }
                    }
                    KeyCode::Esc => {
                        ui.composer.clear();
                        ui.slash.update(&ui.composer.input);
                    }
                    KeyCode::Char('@') => {
                        // `@` summons the fuzzy file picker. The `@` is not
                        // inserted into the composer until the user picks a
                        // file (see ModalOutcome::Close handler above).
                        ui.modal = ActiveModal::FilePicker(FilePicker::new(project_root.clone()));
                    }
                    KeyCode::Char('?') if ui.composer.input.is_empty() => {
                        ui.modal = ActiveModal::Help(HelpModal::new());
                    }
                    KeyCode::Char(c) => {
                        ui.composer.input.push(c);
                        ui.slash.update(&ui.composer.input);
                    }
                    _ => {}
                }
                draw(terminal, ui)?;
            }
            Ok(CtEvent::Resize(_, _)) => draw(terminal, ui)?,
            Ok(_) => {}
            Err(e) => {
                ui.transcript
                    .push(TranscriptItem::Error(format!("input: {e}")));
                draw(terminal, ui)?;
            }
        }
    }
    Ok(IdleAction::Quit)
}

async fn run_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut AgentLoop,
    events: &mut EventStream,
    ui: &mut UiState,
    prompt: String,
) -> Result<()> {
    let mut stream = agent.run(prompt);
    loop {
        tokio::select! {
            biased;
            ev = events.next() => {
                if let Some(Ok(CtEvent::Key(k))) = ev {
                    if k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char('c'))
                    {
                        ui.transcript.push(TranscriptItem::System(
                            "(cancel mid-turn arrives in M2; finishing current step)".into(),
                        ));
                        draw(terminal, ui)?;
                    }
                }
            }
            evt = stream.next() => {
                match evt {
                    Some(event) => {
                        apply_event(&event, &mut ui.transcript, &mut ui.status);
                        draw(terminal, ui)?;
                        if matches!(event, AgentEvent::Stop { .. }) {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

fn apply_event(event: &AgentEvent, transcript: &mut Transcript, status: &mut StatusLine) {
    match event {
        AgentEvent::TextDelta { text } => transcript.append_assistant_text(text),
        AgentEvent::ToolStart { name, input, .. } => {
            let summary = compact_args(input);
            transcript.push(TranscriptItem::ToolCall {
                name: name.clone(),
                summary,
            });
        }
        AgentEvent::ToolResult {
            output, is_error, ..
        } => {
            let first_line = output.lines().next().unwrap_or("").to_string();
            transcript.push(TranscriptItem::ToolResult {
                ok: !is_error,
                summary: truncate(first_line, 120),
            });
        }
        AgentEvent::Usage { usage } => status.merge_usage(usage),
        AgentEvent::TurnComplete => {}
        AgentEvent::Stop { reason } => {
            if !matches!(reason, AgentStop::EndTurn) {
                transcript.push(TranscriptItem::System(format!("(stop: {reason:?})")));
            }
        }
        AgentEvent::Error { message } => {
            transcript.push(TranscriptItem::Error(message.clone()));
        }
    }
}

fn compact_args(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(s, 120)
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}

/// Suspend the TUI, launch `$EDITOR` (falling back to a platform default)
/// on `path` and block until it exits, then re-enter the alternate screen
/// and request a redraw on the next `draw` call.
fn launch_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    path: &std::path::Path,
) -> Result<()> {
    // Leave the alternate screen so the editor draws on the user's normal
    // terminal. We don't recreate the Terminal — same backend, just toggle
    // raw mode and screen state.
    disable_raw_mode().ok();
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture).ok();
    terminal.show_cursor().ok();

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
        if cfg!(windows) {
            "notepad".to_string()
        } else {
            "nano".to_string()
        }
    });
    let status = std::process::Command::new(&editor).arg(path).status();

    // Restore the TUI either way.
    enable_raw_mode().ok();
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture).ok();
    terminal.clear().ok();

    match status {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("could not launch '{editor}': {e}")),
    }
}

/// Switch the live agent to `provider_id/model_id`. Surfaces success or
/// failure as a transcript line; if there's no agent yet, asks the user to
/// run `/login` first instead of crashing.
fn swap_model(
    ui: &mut UiState,
    agent: &mut Option<AgentLoop>,
    builder: &ProviderBuilder,
    provider_id: &str,
    model_id: &str,
) {
    match agent.as_mut() {
        Some(a) => match builder(provider_id) {
            Ok(provider) => {
                a.swap_provider(provider);
                a.set_model(model_id.to_string());
                ui.status.provider = provider_id.to_string();
                ui.status.model = model_id.to_string();
                ui.transcript.push(TranscriptItem::System(format!(
                    "switched to {provider_id}/{model_id}"
                )));
            }
            Err(e) => {
                ui.transcript
                    .push(TranscriptItem::Error(format!("/model {provider_id}: {e}")));
            }
        },
        None => {
            ui.transcript.push(TranscriptItem::Error(
                "No provider configured — run /login first.".into(),
            ));
        }
    }
}

fn resume_session(agent: &mut Option<AgentLoop>, ui: &mut UiState, entry: SessionEntry) {
    match arccode_session::load_session(&entry.path) {
        Ok(records) => {
            let history = arccode_session::records_to_messages(&records);
            ui.transcript.push(TranscriptItem::System(format!(
                "resuming session {} ({} messages)",
                entry.label,
                history.len()
            )));
            // We can't set history directly on AgentLoop without a dedicated
            // method, so note the limitation.
            drop(history); // TODO: wire up once AgentLoop::set_history is available
            ui.transcript.push(TranscriptItem::System(
                "(session context shown above; history injection requires agent rebuild)".into(),
            ));
            let _ = agent; // will be used once set_history is wired
        }
        Err(e) => {
            ui.transcript
                .push(TranscriptItem::Error(format!("resume: {e}")));
        }
    }
}

fn export_transcript(
    transcript: &Transcript,
    format: &str,
    project_root: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let ext = if format == "json" { "json" } else { "md" };
    let dir = project_root.join(".arccode").join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{ts}.{ext}"));

    if format == "json" {
        let items: Vec<serde_json::Value> = transcript.items.iter().map(|item| {
            match item {
                TranscriptItem::UserPrompt(s) => serde_json::json!({"role": "user", "content": s}),
                TranscriptItem::AssistantText(s) => serde_json::json!({"role": "assistant", "content": s}),
                TranscriptItem::ToolCall { name, summary } => serde_json::json!({"role": "tool_call", "name": name, "summary": summary}),
                TranscriptItem::ToolResult { ok, summary } => serde_json::json!({"role": "tool_result", "ok": ok, "summary": summary}),
                TranscriptItem::System(s) => serde_json::json!({"role": "system", "content": s}),
                TranscriptItem::Error(s) => serde_json::json!({"role": "error", "content": s}),
            }
        }).collect();
        std::fs::write(&path, serde_json::to_string_pretty(&items)?)?;
    } else {
        let mut md = String::new();
        for item in &transcript.items {
            match item {
                TranscriptItem::UserPrompt(s) => { md.push_str(&format!("**You:** {s}\n\n")); }
                TranscriptItem::AssistantText(s) => { md.push_str(&format!("{s}\n\n")); }
                TranscriptItem::ToolCall { name, summary } => { md.push_str(&format!("> `{name}` {summary}\n")); }
                TranscriptItem::ToolResult { ok, summary } => {
                    let glyph = if *ok { "✓" } else { "✗" };
                    md.push_str(&format!("> {glyph} {summary}\n\n"));
                }
                TranscriptItem::System(s) => { md.push_str(&format!("*{s}*\n")); }
                TranscriptItem::Error(s) => { md.push_str(&format!("**Error:** {s}\n\n")); }
            }
        }
        std::fs::write(&path, md)?;
    }
    Ok(path)
}

fn help_text() -> String {
    String::from(
        "Slash commands:\n  \
         /help                       this message\n  \
         /clear                      reset the conversation\n  \
         /login, /connect            set up a provider in a guided wizard\n  \
         /logout [provider]          remove a stored API key\n  \
         /model [provider/model]     switch model, or open a picker with no arg\n  \
         /mode <m>                   change display mode (read-only/auto-edit/yolo)\n  \
         /add <path>                 attach a file to the next prompt\n  \
         /usage                      show per-model token + cost breakdown\n  \
         /skills                     browse and apply skills\n  \
         /skill <name>               queue a skill for the next prompt\n  \
         /skills new <name>          create a new skill in $EDITOR\n  \
         /mcp                        manage MCP servers (add / connect / remove)\n  \
         /params                     adjust temperature and max_tokens\n  \
         /resume                     resume a previous session\n  \
         /export [md|json]           export conversation to file\n  \
         /quit                       exit\n\nKeys: \
         Enter submit, Up/Down history, Esc clear input, Ctrl-C exit, \
         PgUp/PgDn or Shift+Up/Down scroll transcript, ? show shortcuts. \
         Type @ to fuzzy-pick a file from the project.",
    )
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, ui: &UiState) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // transcript
                Constraint::Length(3), // composer
                Constraint::Length(1), // status
            ])
            .split(area);
        if ui.transcript.items.is_empty() {
            WelcomeView { status: &ui.status }.render(chunks[0], f.buffer_mut());
        } else {
            TranscriptView {
                transcript: &ui.transcript,
            }
            .render(chunks[0], f.buffer_mut());
        }
        ComposerView {
            composer: &ui.composer,
        }
        .render(chunks[1], f.buffer_mut());
        StatusView { status: &ui.status }.render(chunks[2], f.buffer_mut());

        // Floating slash-command popup sits directly *above* the composer.
        // We size it to fit within the rows available above the composer,
        // and place its bottom edge exactly on the row just above the
        // composer's top border — never overlapping it.
        if ui.slash.is_visible() {
            let composer = chunks[1];
            let room_above = composer.y; // rows 0..composer.y are available
            let height = ui.slash.rendered_height().min(room_above);
            if height >= 3 {
                let popup = Rect {
                    x: composer.x,
                    y: composer.y - height,
                    width: composer.width,
                    height,
                };
                ui.slash.render(popup, f.buffer_mut());
            }
        }

        ui.modal.render(area, f.buffer_mut());
    })?;
    Ok(())
}

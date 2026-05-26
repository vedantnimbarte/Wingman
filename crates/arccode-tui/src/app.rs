//! Top-level TUI application.
//!
//! Owns the terminal, the [`AgentLoop`], and the screen state. Runs an
//! event-driven outer loop for the idle state and a streaming inner loop
//! that selects between crossterm events and agent events while a turn is
//! in flight.

use std::io::{stdout, Stdout};
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
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::Widget,
    Terminal,
};

use crate::widgets::{
    composer::ComposerView, status::StatusView, transcript::TranscriptView, Composer, StatusLine,
    Transcript, TranscriptItem,
};

/// Closure passed in by the CLI/runtime that knows how to construct a
/// provider for a given `provider_id`. We don't want `arccode-tui` to
/// depend on `arccode-providers` directly — this keeps the dependency
/// graph one-way and lets the TUI swap providers mid-session.
pub type ProviderBuilder =
    Arc<dyn Fn(&str) -> std::result::Result<Arc<dyn Provider>, String> + Send + Sync>;

pub struct AppCtx {
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    pub builder: ProviderBuilder,
}

pub async fn run(mut agent: AgentLoop, ctx: AppCtx) -> Result<()> {
    let mut terminal = setup_terminal()?;
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
    Model(String),
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
        "/model" if !arg.is_empty() => Cmd::Model(arg.to_string()),
        "" => Cmd::None,
        _ => Cmd::Submit(line.to_string()),
    }
}

struct UiState {
    transcript: Transcript,
    composer: Composer,
    status: StatusLine,
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut AgentLoop,
    ctx: AppCtx,
) -> Result<()> {
    let mut ui = UiState {
        transcript: Transcript::default(),
        composer: Composer::default(),
        status: StatusLine {
            model: ctx.model.clone(),
            provider: ctx.provider_id.clone(),
            mode: ctx.mode.clone(),
            ..Default::default()
        },
    };
    ui.transcript.push(TranscriptItem::System(format!(
        "arccode {}/{} · mode={} · /help for commands · /quit to exit",
        ctx.provider_id, ctx.model, ctx.mode
    )));

    let mut events = EventStream::new();
    loop {
        ui.composer.busy = false;
        draw(terminal, &ui)?;

        // Idle: wait for a user input event.
        let next_action = idle_step(&mut events, &mut ui, terminal, agent, &ctx.builder).await?;
        match next_action {
            IdleAction::Quit => return Ok(()),
            IdleAction::Submit(prompt) => {
                ui.transcript
                    .push(TranscriptItem::UserPrompt(prompt.clone()));
                ui.composer.busy = true;
                draw(terminal, &ui)?;
                run_turn(terminal, agent, &mut events, &mut ui, prompt).await?;
            }
        }
    }
}

enum IdleAction {
    Quit,
    Submit(String),
}

async fn idle_step(
    events: &mut EventStream,
    ui: &mut UiState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &mut AgentLoop,
    builder: &ProviderBuilder,
) -> Result<IdleAction> {
    while let Some(ev) = events.next().await {
        match ev {
            Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c'))
                {
                    return Ok(IdleAction::Quit);
                }
                match k.code {
                    KeyCode::Enter => {
                        let raw = ui.composer.take_input();
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
                            Cmd::Model(arg) => match arg.split_once('/') {
                                Some((provider_id, model_id)) => match builder(provider_id) {
                                    Ok(provider) => {
                                        agent.swap_provider(provider);
                                        agent.set_model(model_id.to_string());
                                        ui.status.provider = provider_id.to_string();
                                        ui.status.model = model_id.to_string();
                                        ui.transcript.push(TranscriptItem::System(format!(
                                            "switched to {provider_id}/{model_id}"
                                        )));
                                    }
                                    Err(e) => {
                                        ui.transcript.push(TranscriptItem::Error(format!(
                                            "/model {provider_id}: {e}"
                                        )));
                                    }
                                },
                                None => {
                                    ui.transcript.push(TranscriptItem::Error(
                                            "/model expects provider/model_id (e.g. anthropic/claude-opus-4-7)".into(),
                                        ));
                                }
                            },
                            Cmd::None => {}
                            Cmd::Submit(prompt) => return Ok(IdleAction::Submit(prompt)),
                        }
                    }
                    KeyCode::Backspace => {
                        ui.composer.input.pop();
                    }
                    KeyCode::Up => ui.composer.history_prev(),
                    KeyCode::Down => ui.composer.history_next(),
                    KeyCode::Esc => ui.composer.clear(),
                    KeyCode::Char(c) => ui.composer.input.push(c),
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

fn help_text() -> String {
    String::from(
        "Slash commands:\n  /help                       this message\n  /clear            \
         reset the conversation\n  /model provider/model       switch model (e.g. \
         /model openai/gpt-4.1)\n  /mode <m>                   change display mode \
         (read-only/auto-edit/yolo)\n  /quit                       exit\n\nKeys: \
         Enter submit, Up/Down history, Esc clear input, Ctrl-C exit.",
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
        TranscriptView {
            transcript: &ui.transcript,
        }
        .render(chunks[0], f.buffer_mut());
        ComposerView {
            composer: &ui.composer,
        }
        .render(chunks[1], f.buffer_mut());
        StatusView { status: &ui.status }.render(chunks[2], f.buffer_mut());
    })?;
    Ok(())
}

//! Modal screen subsystem.
//!
//! A modal is a transient overlay (wizard, picker, list view) that captures
//! keyboard input while open and dims the chat behind it. Each modal variant
//! lives in its own module and is wired into [`ActiveModal`].
//!
//! Design: an enum (not a trait object) so the app loop can pattern-match on
//! outcomes specific to each modal (e.g. `FilePicker` returns a path,
//! `LoginWizard` returns a configured provider). Adding a new modal = a new
//! variant here and a new arm in `handle_key` / `render`.

use crossterm::event::KeyEvent;
use ratatui::{buffer::Buffer, layout::Rect};

pub mod file_picker;
pub mod login;
pub mod mcp;
pub mod model_picker;
pub mod skills;
pub mod usage;

pub use file_picker::FilePicker;
pub use login::{LoginPayload, LoginTask, LoginWizard};
pub use mcp::{McpAddPayload, McpServerSummary, McpTask, McpView};
pub use model_picker::{ModelChoice, ModelPicker};
pub use skills::SkillsView;
pub use usage::UsageView;

/// What a modal asks the host app to do after a key event.
#[derive(Debug, Clone)]
pub enum ModalOutcome {
    /// Modal stays open; redraw.
    Continue,
    /// Modal asks to close itself.
    #[allow(dead_code)] // wired up by later phases
    Close,
}

/// An async task a modal has asked the host loop to perform on its behalf.
/// The host runs the task, then reports the result back via the modal's
/// own `task_completed` method.
#[derive(Debug, Clone)]
pub enum ModalTask {
    Login(LoginTask),
    Mcp(McpTask),
}

/// The active modal, if any.
#[derive(Debug, Default)]
pub enum ActiveModal {
    #[default]
    None,
    Login(LoginWizard),
    FilePicker(FilePicker),
    ModelPicker(ModelPicker),
    Usage(UsageView),
    Skills(SkillsView),
    Mcp(McpView),
}

impl ActiveModal {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModalOutcome {
        match self {
            Self::None => ModalOutcome::Continue,
            Self::Login(w) => w.handle_key(key),
            Self::FilePicker(p) => p.handle_key(key),
            Self::ModelPicker(p) => p.handle_key(key),
            Self::Usage(v) => v.handle_key(key),
            Self::Skills(v) => v.handle_key(key),
            Self::Mcp(v) => v.handle_key(key),
        }
    }

    /// Drain any async task the active modal has queued.
    pub fn take_pending_task(&mut self) -> Option<ModalTask> {
        match self {
            Self::None => None,
            Self::Login(w) => w.take_pending_task().map(ModalTask::Login),
            Self::FilePicker(_) => None,
            Self::ModelPicker(_) => None,
            Self::Usage(_) => None,
            Self::Skills(_) => None,
            Self::Mcp(v) => v.take_pending_task().map(ModalTask::Mcp),
        }
    }

    /// Report the outcome of the most recently dispatched task.
    pub fn task_completed(&mut self, result: Result<(), String>) {
        match self {
            Self::None => {}
            Self::Login(w) => w.task_completed(result),
            Self::FilePicker(_) => {}
            Self::ModelPicker(_) => {}
            Self::Usage(_) => {}
            Self::Skills(_) => {}
            Self::Mcp(v) => v.task_completed(result),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        match self {
            Self::None => {}
            Self::Login(w) => w.render(area, buf),
            Self::FilePicker(p) => p.render(area, buf),
            Self::ModelPicker(p) => p.render(area, buf),
            Self::Usage(v) => v.render(area, buf),
            Self::Skills(v) => v.render(area, buf),
            Self::Mcp(v) => v.render(area, buf),
        }
    }
}

/// Compute a centered sub-`Rect` sized as a percentage of the parent.
pub fn centered_rect(parent: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let w = parent.width.saturating_mul(pct_x) / 100;
    let h = parent.height.saturating_mul(pct_y) / 100;
    let x = parent.x + parent.width.saturating_sub(w) / 2;
    let y = parent.y + parent.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

//! `wingman board` — a persistent, multi-project kanban board over pilot runs.
//!
//! Cards are goals you author; they outlive the runs that execute them.
//! Columns are **derived** from run state, never stored, so the board cannot
//! disagree with `pilot watch` — both read the same `state.json`.
//!
//! Three layers, one writer each:
//!
//! | Layer | Home | Owner |
//! | --- | --- | --- |
//! | Card identity, projects, dispatch history | `~/.wingman/board.db` | this crate |
//! | Execution truth (tasks, agents, cost) | `<project>/.wingman/autonomous/<run>/` | `wingman_autonomous::RunStore` |
//! | Roll-up cache | `board.db` table `rollup` | this crate (derived, safe to delete) |
//!
//! See `docs/BOARD.md` for the architecture and `docs/BOARD-SPEC.md` for the
//! normative spec.

pub mod card;
pub mod column;
pub mod dispatch;
pub mod registry;
pub mod rollup;
pub mod store;
pub mod view;

pub use card::{Card, NewCard};
pub use column::{Badge, BoardCard, Column};
pub use dispatch::{DispatchOpts, DispatchPlan, Dispatched};
pub use registry::Project;
pub use rollup::{Rollup, SubRow};
pub use store::{BoardError, BoardStore, Result};
pub use view::{BoardView, CardView, ColumnView, SubRowView};

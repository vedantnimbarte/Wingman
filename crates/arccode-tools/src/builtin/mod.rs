//! Built-in tools shipped with arccode.
//!
//! Each tool has a stable name (used in the prompt and on the wire), a JSON
//! Schema describing its inputs, and an async `run` that produces a string
//! result. Tools that mutate state consult `ToolCtx` for permission.

mod edit_file;
mod glob_tool;
mod grep_tool;
mod list_dir;
mod read_file;
mod run_shell;
mod semantic_search;
mod write_file;

pub use edit_file::EditFile;
pub use glob_tool::Glob;
pub use grep_tool::Grep;
pub use list_dir::ListDir;
pub use read_file::ReadFile;
pub use run_shell::RunShell;
pub use semantic_search::SemanticSearch;
pub use write_file::WriteFile;

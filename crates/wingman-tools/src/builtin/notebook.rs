//! Jupyter notebooks: the `.ipynb` renderer `read_file` uses, `notebook_edit`
//! (change one cell without hand-editing the JSON) and `notebook_run`
//! (execute through `jupyter nbconvert`, then report the outputs).
//!
//! Both tools sit on the ordinary paths rather than beside them: the registry
//! checkpoints the notebook before either runs (`/undo`, rewind), audits the
//! call, and gates the capability; `notebook_run` spawns through
//! `run_shell`'s contained path, so the denylist, sandbox, Job Object and
//! credential scrub apply unchanged.

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use wingman_core::{ToolOutcome, ToolSpec};

/// Per output block (one stream run, one result, one traceback).
const OUTPUT_BLOCK_MAX: usize = 4_000;
/// The whole `## cells` section of a `notebook_run` report.
const RUN_REPORT_MAX: usize = 24_000;

/// Join an nbformat multiline string, which may be a string or a list of lines.
fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// `source` the way nbformat stores it: a list of lines, each keeping its `\n`.
fn source_lines(s: &str) -> Value {
    Value::Array(
        s.split_inclusive('\n')
            .map(|l| Value::String(l.to_string()))
            .collect(),
    )
}

/// Keep at most `max` bytes: the head, or the tail for tracebacks (where the
/// line that matters is last).
fn clip(s: &str, max: usize, keep_tail: bool) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let dropped = s.len() - max;
    if keep_tail {
        let mut start = dropped;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        format!("[… {start} earlier bytes not shown]\n{}", &s[start..])
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[… {} more bytes not shown]", &s[..end], s.len() - end)
    }
}

/// IPython tracebacks are coloured; the escapes are noise to a model.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ params… final byte in '@'..='~'.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ipynb(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

fn outputs_of(cell: &Value) -> &[Value] {
    cell.get("outputs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn error_line(o: &Value) -> String {
    format!(
        "{}: {}",
        o.get("ename").and_then(Value::as_str).unwrap_or("Error"),
        o.get("evalue").and_then(Value::as_str).unwrap_or("")
    )
}

fn quote_block(out: &mut String, label: &str, text: &str) {
    out.push_str(&format!("> {label}:\n"));
    for line in clip(text, OUTPUT_BLOCK_MAX, false).lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
}

/// A code cell's outputs as `> label:` quote blocks. Rich output is named,
/// never inlined: a base64 PNG is megabytes of context that says nothing.
fn render_outputs(outputs: &[Value]) -> String {
    let mut out = String::new();
    // Jupyter splits one print loop into many stream chunks; adjacent chunks
    // of the same stream read as one block.
    let mut i = 0;
    while i < outputs.len() {
        let o = &outputs[i];
        match o.get("output_type").and_then(Value::as_str).unwrap_or("") {
            "stream" => {
                let name = o.get("name").and_then(Value::as_str).unwrap_or("stdout");
                let mut text = String::new();
                while i < outputs.len()
                    && outputs[i].get("output_type").and_then(Value::as_str) == Some("stream")
                    && outputs[i]
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("stdout")
                        == name
                {
                    text.push_str(&text_of(outputs[i].get("text")));
                    i += 1;
                }
                if !text.is_empty() {
                    quote_block(&mut out, name, &text);
                }
                continue;
            }
            kind @ ("execute_result" | "display_data") => {
                if let Some(data) = o.get("data").and_then(Value::as_object) {
                    let plain = data.get("text/plain");
                    if let Some(t) = plain {
                        let label = if kind == "execute_result" {
                            "result"
                        } else {
                            "display"
                        };
                        quote_block(&mut out, label, &text_of(Some(t)));
                    }
                    for mime in data.keys().filter(|m| *m != "text/plain") {
                        if mime.starts_with("image/") || plain.is_none() {
                            out.push_str(&format!("> [{mime} output not shown]\n"));
                        }
                    }
                }
            }
            "error" => out.push_str(&format!("> error: {}\n", error_line(o))),
            _ => {}
        }
        i += 1;
    }
    out
}

fn notebook_language(nb: &Value) -> String {
    nb.get("metadata")
        .and_then(|m| m.get("language_info"))
        .and_then(|l| l.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("python")
        .to_string()
}

fn render_cell(i: usize, cell: &Value, lang_global: &str) -> String {
    let mut out = String::new();
    let source = text_of(cell.get("source"));
    match cell.get("cell_type").and_then(Value::as_str).unwrap_or("") {
        "markdown" => {
            out.push_str(&format!("<!-- cell {i}: markdown -->\n"));
            out.push_str(&source);
            if !source.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        "code" => {
            let lang = cell
                .get("metadata")
                .and_then(|m| m.get("language"))
                .and_then(Value::as_str)
                .unwrap_or(lang_global);
            out.push_str(&format!("<!-- cell {i}: code -->\n```{lang}\n"));
            out.push_str(&source);
            if !source.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
            out.push_str(&render_outputs(outputs_of(cell)));
            out.push('\n');
        }
        "raw" => {
            out.push_str(&format!("<!-- cell {i}: raw -->\n"));
            out.push_str(&source);
            out.push_str("\n\n");
        }
        _ => {}
    }
    out
}

/// Render a Jupyter `.ipynb` JSON document into a flat, model-friendly
/// markdown layout: code cells become fenced code blocks (language taken
/// from `metadata.language_info.name` or `language` per cell, fallback
/// `python`) followed by their outputs as `> ` quote blocks, markdown cells
/// become their raw markdown source. Returns `None` on parse failure so the
/// caller can fall back to the raw JSON.
pub(crate) fn render_notebook(text: &str) -> Option<String> {
    let nb: Value = serde_json::from_str(text).ok()?;
    let lang = notebook_language(&nb);
    let cells = nb.get("cells")?.as_array()?;
    Some(
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| render_cell(i, c, &lang))
            .collect(),
    )
}

// ---------------------------------------------------------------- notebook_edit

pub struct NotebookEdit;

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    operation: Operation,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    cell_id: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    cell_type: Option<String>,
}

/// A fresh nbformat 4.5 cell id: 8 hex chars, unique within `cells`.
fn new_cell_id(cells: &[Value]) -> String {
    use std::hash::BuildHasher;
    let mut salt = cells.len() as u64;
    loop {
        // RandomState is randomly keyed per instance — randomness without a
        // `uuid`/`rand` dependency. Uniqueness is checked, not assumed.
        let id = format!(
            "{:016x}",
            std::collections::hash_map::RandomState::new().hash_one(salt)
        )[..8]
            .to_string();
        if !cells
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some(&id))
        {
            return id;
        }
        salt += 1;
    }
}

/// Serialize the way Jupyter writes: indent 1 (or whatever the file already
/// used), the file's line endings, and its trailing newline.
///
/// Key order: serde_json sorts keys unless `preserve_order` is on, and Jupyter
/// itself saves with `sort_keys=True`, so a Jupyter-written file round-trips.
// ponytail: numbers go through f64/i64, so an exotic float spelling in
// metadata (`1e-05`) is rewritten as `0.00001`. Enable serde_json's
// `arbitrary_precision` if a real notebook ever trips on that.
fn serialize_like(nb: &Value, original: &str) -> String {
    let indent: String = original
        .lines()
        .nth(1)
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
        .filter(|s: &String| !s.is_empty())
        .unwrap_or_else(|| " ".to_string());
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    nb.serialize(&mut ser)
        .expect("a serde_json::Value always serializes");
    let mut s = String::from_utf8(buf).expect("serde_json emits UTF-8");
    if original.ends_with('\n') {
        s.push('\n');
    }
    if original.contains("\r\n") {
        s = s.replace('\n', "\r\n");
    }
    s
}

/// Apply one edit to notebook JSON text. Returns the new file text and a
/// report (a unified diff of the cell source, then one summary line).
fn apply_edit(text: &str, a: &EditArgs) -> Result<(String, String), String> {
    let mut nb: Value =
        serde_json::from_str(text).map_err(|e| format!("not valid notebook JSON: {e}"))?;
    let major = nb.get("nbformat").and_then(Value::as_u64).unwrap_or(0);
    let minor = nb
        .get("nbformat_minor")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if major != 4 {
        return Err(format!(
            "nbformat {major} is not supported — only nbformat 4 notebooks can be edited"
        ));
    }
    let has_ids = minor >= 5;
    let cells = nb
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or("notebook has no `cells` array")?;
    let len = cells.len();

    let pos = match (a.index, a.cell_id.as_deref()) {
        (Some(_), Some(_)) => return Err("pass `index` or `cell_id`, not both".into()),
        (None, None) => return Err("pass `index` or `cell_id` to say which cell".into()),
        (Some(i), None) => i,
        (None, Some(id)) => {
            if !has_ids {
                return Err(format!(
                    "this notebook is nbformat 4.{minor}, which has no cell ids (they start \
                     at 4.5) — address the cell by `index`"
                ));
            }
            cells
                .iter()
                .position(|c| c.get("id").and_then(Value::as_str) == Some(id))
                .ok_or_else(|| format!("no cell with id `{id}`"))?
        }
    };
    let in_range = if a.operation == Operation::Insert {
        pos <= len
    } else {
        pos < len
    };
    if !in_range {
        return Err(match (a.operation, len) {
            (Operation::Insert, _) => {
                format!("insert position {pos} is out of range: the notebook has {len} cells (valid: 0..={len})")
            }
            (_, 0) => format!("cell index {pos} is out of range: the notebook has no cells"),
            _ => format!(
                "cell index {pos} is out of range: the notebook has {len} cells (valid: 0..={})",
                len - 1
            ),
        });
    }

    let (old_src, new_src, summary) = match a.operation {
        Operation::Replace => {
            let src = a.source.as_deref().ok_or("`replace` needs `source`")?;
            let cell = cells[pos]
                .as_object_mut()
                .ok_or_else(|| format!("cell {pos} is not a JSON object"))?;
            let kind = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(want) = a.cell_type.as_deref().filter(|w| *w != kind) {
                return Err(format!(
                    "cell {pos} is a {kind} cell; changing it to {want} is not supported — \
                     delete it and insert a new one"
                ));
            }
            let old = text_of(cell.get("source"));
            cell.insert("source".into(), source_lines(src));
            let mut summary = format!("replaced the source of cell {pos} ({kind})");
            // Old outputs describe code that no longer exists; keeping them
            // would show results the new source never produced.
            if kind == "code" {
                cell.insert("outputs".into(), json!([]));
                cell.insert("execution_count".into(), Value::Null);
                summary.push_str("; its outputs and execution_count were cleared");
            }
            (old, src.to_string(), summary)
        }
        Operation::Insert => {
            let src = a.source.as_deref().unwrap_or("");
            let kind = a.cell_type.as_deref().unwrap_or("code");
            let mut cell = match kind {
                "code" => json!({
                    "cell_type": "code",
                    "execution_count": null,
                    "metadata": {},
                    "outputs": [],
                    "source": source_lines(src),
                }),
                "markdown" => json!({
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": source_lines(src),
                }),
                other => {
                    return Err(format!(
                        "`cell_type` must be `code` or `markdown`, got `{other}`"
                    ))
                }
            };
            let mut summary = format!("inserted a {kind} cell at index {pos}");
            if has_ids {
                let id = new_cell_id(cells);
                summary.push_str(&format!(" (id `{id}`)"));
                cell["id"] = Value::String(id);
            }
            cells.insert(pos, cell);
            (String::new(), src.to_string(), summary)
        }
        Operation::Delete => {
            let cell = cells.remove(pos);
            let kind = cell.get("cell_type").and_then(Value::as_str).unwrap_or("");
            let summary = format!("deleted cell {pos} ({kind})");
            (text_of(cell.get("source")), String::new(), summary)
        }
    };

    let report = format!(
        "{}{summary}",
        super::edit_file::unified_diff(&old_src, &new_src, &a.path)
    );
    Ok((serialize_like(&nb, text), report))
}

#[async_trait]
impl Tool for NotebookEdit {
    fn capabilities(&self) -> Capability {
        Capability::READ | Capability::WRITE
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notebook_edit".into(),
            description: "Edit one cell of a Jupyter notebook (.ipynb) without touching the \
                          rest of the JSON. `operation`: `replace` (new `source`; a code cell's \
                          outputs are cleared), `insert` (a new `cell_type` code|markdown cell \
                          placed at `index`, shifting later cells down; index = cell count \
                          appends), or `delete`. Address the cell by 0-based `index` (as shown \
                          by read_file's `<!-- cell N -->` markers) or by `cell_id` on nbformat \
                          4.5+ notebooks. Returns a diff of the cell source."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the .ipynb file." },
                    "operation": { "type": "string", "enum": ["replace", "insert", "delete"] },
                    "index": { "type": "integer", "minimum": 0, "description": "0-based cell index (for insert: the position the new cell takes)." },
                    "cell_id": { "type": "string", "description": "Cell id (nbformat 4.5+), instead of index." },
                    "source": { "type": "string", "description": "New cell source (replace, insert)." },
                    "cell_type": { "type": "string", "enum": ["code", "markdown"], "description": "Type of the inserted cell; default code." }
                },
                "required": ["path", "operation"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: EditArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let path = ctx.resolve(&args.path);
        if !is_ipynb(&path) {
            return ToolOutcome::err(format!(
                "notebook_edit only edits .ipynb files; use edit_file for {}",
                path.display()
            ));
        }
        if !ctx.allows_write(&path) {
            return ToolOutcome::err(ctx.write_denial_reason(&path));
        }
        let original = match ctx.fs.read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };
        let (updated, report) = match apply_edit(&original, &args) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(format!("{}: {e}", path.display())),
        };
        if let Err(e) = ctx.fs.write(&path, updated.as_bytes()).await {
            return ToolOutcome::err(format!("write {}: {e}", path.display()));
        }
        ToolOutcome::ok(report)
    }
}

// ----------------------------------------------------------------- notebook_run

pub struct NotebookRun;

#[derive(Debug, Deserialize)]
struct RunArgs {
    path: String,
    #[serde(default)]
    cell: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const JUPYTER_MISSING: &str = "`jupyter` was not found on PATH. notebook_run executes \
    notebooks with `jupyter nbconvert`: install it into the environment the notebook needs \
    (`pip install nbconvert ipykernel`, or `pip install jupyter`) and retry. \
    `wingman doctor` reports whether it is found.";

/// The shell command `notebook_run` hands to `run_shell`'s contained path,
/// run with the notebook's directory as cwd.
///
/// `--allow-errors` so a failing cell is written into the notebook as an
/// error output (which is what gets reported) instead of aborting the run
/// with nothing saved. The timeout is nbconvert's per-cell limit; the whole
/// process is additionally bounded by the same value as a spawn timeout.
fn nbconvert_command(file_name: &str, timeout_secs: u64) -> Result<String, String> {
    // `./` so a name starting with `-` is not read as an option.
    let arg = format!("./{file_name}");
    let arg = if cfg!(windows) {
        // ponytail: cmd.exe cannot be handed a quoted argument through
        // tokio's `.arg()` (it escapes `"` as `\"`, which cmd does not
        // understand), so names that would need quoting are refused. Lift
        // this with `raw_arg` in `run_shell::prepare` when that path is fixed.
        if file_name
            .chars()
            .any(|c| c.is_whitespace() || "\"%^&|<>()!".contains(c))
        {
            return Err(format!(
                "notebook_run cannot pass `{file_name}` to cmd.exe safely (it contains a space \
                 or shell metacharacter) — rename the notebook, or run jupyter via run_shell"
            ));
        }
        arg
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    };
    Ok(format!(
        "jupyter nbconvert --to notebook --execute --inplace \
         --ExecutePreprocessor.allow_errors=True \
         --ExecutePreprocessor.timeout={timeout_secs} {arg}"
    ))
}

/// The report for an executed notebook: errors (with tracebacks) first, then
/// the cells, bounded. `only` restricts both to one cell.
fn render_run(text: &str, only: Option<usize>) -> Result<String, String> {
    let nb: Value = serde_json::from_str(text)
        .map_err(|e| format!("executed notebook is not valid JSON: {e}"))?;
    let cells = nb
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("executed notebook has no `cells` array")?;
    let lang = notebook_language(&nb);

    let (mut errors, mut shown, mut elsewhere) = (String::new(), 0, 0);
    for (i, cell) in cells.iter().enumerate() {
        for o in outputs_of(cell)
            .iter()
            .filter(|o| o.get("output_type").and_then(Value::as_str) == Some("error"))
        {
            if only.is_some_and(|k| k != i) {
                elsewhere += 1;
                continue;
            }
            shown += 1;
            let tb = o
                .get("traceback")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            errors.push_str(&format!("### cell {i}: {}\n", error_line(o)));
            let tb = strip_ansi(&tb);
            if !tb.trim().is_empty() {
                errors.push_str(&clip(tb.trim_end(), OUTPUT_BLOCK_MAX, true));
                errors.push('\n');
            }
            errors.push('\n');
        }
    }

    let mut out = format!("executed; {shown} error(s)");
    if elsewhere > 0 {
        out.push_str(&format!(" ({elsewhere} more in other cells)"));
    }
    out.push_str("\n\n");
    if !errors.is_empty() {
        out.push_str("## errors\n");
        out.push_str(&errors);
    }
    let body: String = match only {
        Some(k) => cells
            .get(k)
            .map(|c| render_cell(k, c, &lang))
            .unwrap_or_default(),
        None => cells
            .iter()
            .enumerate()
            .map(|(i, c)| render_cell(i, c, &lang))
            .collect(),
    };
    out.push_str("## cells\n");
    out.push_str(&clip(&body, RUN_REPORT_MAX, false));
    Ok(out)
}

#[async_trait]
impl Tool for NotebookRun {
    /// Executes a subprocess that rewrites the notebook in place.
    fn capabilities(&self) -> Capability {
        Capability::READ | Capability::WRITE | Capability::SHELL
    }

    /// Bounded by its own `timeout_secs` (default 300, max 600), which is
    /// legitimately longer than the registry backstop.
    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notebook_run".into(),
            description: "Execute a Jupyter notebook (.ipynb) top to bottom with `jupyter \
                          nbconvert --execute --inplace`, saving the outputs into the file, and \
                          return them: errors with tracebacks first, then each cell's outputs \
                          (images are named, not inlined). A failing cell does not stop the \
                          run. Set `cell` to report only that cell's outputs (the whole \
                          notebook still runs). Needs jupyter on PATH."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the .ipynb file." },
                    "cell": { "type": "integer", "minimum": 0, "description": "Report only this 0-based cell." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Per-cell and overall limit; default 300." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: RunArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let path = ctx.resolve(&args.path);
        if !is_ipynb(&path) {
            return ToolOutcome::err(format!(
                "notebook_run only runs .ipynb files, not {}",
                path.display()
            ));
        }
        // `--inplace` rewrites the file, so it is a write as well as a shell.
        if !ctx.allows_write(&path) {
            return ToolOutcome::err(ctx.write_denial_reason(&path));
        }
        // Validate before spending minutes executing.
        let before = match ctx.fs.read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };
        let count = match serde_json::from_str::<Value>(&before) {
            Ok(nb) => match nb.get("cells").and_then(Value::as_array) {
                Some(c) => c.len(),
                None => return ToolOutcome::err("notebook has no `cells` array"),
            },
            Err(e) => return ToolOutcome::err(format!("not valid notebook JSON: {e}")),
        };
        if let Some(k) = args.cell.filter(|k| *k >= count) {
            return ToolOutcome::err(format!(
                "cell index {k} is out of range: the notebook has {count} cells"
            ));
        }
        if wingman_lsp::server::which_on_path("jupyter").is_none() {
            return ToolOutcome::err(JUPYTER_MISSING);
        }
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return ToolOutcome::err(format!("cannot run {}", path.display()));
        };
        let secs = args.timeout_secs.unwrap_or(300).clamp(1, 600);
        let command = match nbconvert_command(name, secs) {
            Ok(c) => c,
            Err(e) => return ToolOutcome::err(e),
        };
        let ran = super::run_shell::run_contained(
            &command,
            None,
            Some(dir.to_string_lossy().into_owned()),
            Duration::from_secs(secs),
            ctx,
        )
        .await;
        if ran.is_error {
            // nbconvert's own failure (no kernel, timeout, bad JSON): the
            // Python exception is at the end of stderr.
            return ToolOutcome::err(format!(
                "notebook_run: jupyter nbconvert failed; the notebook was not updated\n{}",
                clip(&ran.content, OUTPUT_BLOCK_MAX, true)
            ));
        }
        let after = match ctx.fs.read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };
        match render_run(&after, args.cell) {
            Ok(report) => ToolOutcome::ok(report),
            Err(e) => ToolOutcome::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    /// nbformat 4.4 as Jupyter writes it: sorted keys, indent 1, trailing
    /// newline, no cell ids, a code cell carrying outputs.
    const V44: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "metadata": {},
   "source": [
    "# Analysis\n",
    "Some prose — with unicode."
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 3,
   "metadata": {
    "tags": [
     "keep"
    ]
   },
   "outputs": [
    {
     "name": "stdout",
     "output_type": "stream",
     "text": [
      "hello\n"
     ]
    }
   ],
   "source": [
    "x = 1\n",
    "print('hello')"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 4,
   "metadata": {},
   "outputs": [
    {
     "data": {
      "image/png": "iVBORw0KGgo=",
      "text/plain": [
       "<Figure size 640x480 with 1 Axes>"
      ]
     },
     "metadata": {},
     "output_type": "display_data"
    }
   ],
   "source": [
    "plot()"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "language_info": {
   "name": "python",
   "version": "3.11.4"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 4
}
"##;

    /// nbformat 4.5: every cell has an id.
    const V45: &str = r#"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": 1,
   "id": "a1b2c3d4",
   "metadata": {},
   "outputs": [
    {
     "data": {
      "text/plain": [
       "2"
      ]
     },
     "execution_count": 1,
     "metadata": {},
     "output_type": "execute_result"
    }
   ],
   "source": [
    "1 + 1"
   ]
  },
  {
   "cell_type": "markdown",
   "id": "e5f6a7b8",
   "metadata": {},
   "source": [
    "notes"
   ]
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;

    fn args(v: Value) -> EditArgs {
        serde_json::from_value(v).expect("args")
    }

    fn edit(text: &str, v: Value) -> Result<(Value, String, String), String> {
        let (out, report) = apply_edit(text, &args(v))?;
        Ok((serde_json::from_str(&out).expect("valid json"), out, report))
    }

    #[test]
    fn replace_clears_outputs_and_leaves_everything_else_byte_identical() {
        let (nb, out, report) = edit(
            V44,
            json!({"path": "a.ipynb", "operation": "replace", "index": 1, "source": "y = 2\nprint(y)"}),
        )
        .unwrap();
        let cell = &nb["cells"][1];
        assert_eq!(cell["source"], json!(["y = 2\n", "print(y)"]));
        assert_eq!(cell["outputs"], json!([]));
        assert_eq!(cell["execution_count"], Value::Null);
        assert_eq!(cell["metadata"]["tags"], json!(["keep"]));
        // The edit is the only difference: restoring the old cell gives back
        // the original file byte for byte (indent 1, unicode, trailing \n).
        let mut restored: Value = serde_json::from_str(&out).unwrap();
        restored["cells"][1] = serde_json::from_str::<Value>(V44).unwrap()["cells"][1].clone();
        assert_eq!(serialize_like(&restored, V44), V44);
        assert!(report.contains("-print('hello')"), "{report}");
        assert!(report.contains("+print(y)"), "{report}");
        assert!(report.contains("outputs and execution_count were cleared"));
    }

    #[test]
    fn untouched_round_trip_is_byte_identical() {
        for fixture in [V44, V45] {
            let nb: Value = serde_json::from_str(fixture).unwrap();
            assert_eq!(serialize_like(&nb, fixture), fixture);
        }
    }

    #[test]
    fn replace_markdown_has_no_outputs_to_clear() {
        let (nb, _, report) = edit(
            V44,
            json!({"path": "a.ipynb", "operation": "replace", "index": 0, "source": "# Renamed\n"}),
        )
        .unwrap();
        assert_eq!(nb["cells"][0]["source"], json!(["# Renamed\n"]));
        assert!(nb["cells"][0].get("outputs").is_none());
        assert!(!report.contains("cleared"));
        // Other cells' outputs survive.
        assert_eq!(nb["cells"][1]["outputs"][0]["text"], json!(["hello\n"]));
    }

    #[test]
    fn insert_without_ids_on_v44() {
        let (nb, _, _) = edit(
            V44,
            json!({"path": "a.ipynb", "operation": "insert", "index": 3, "cell_type": "markdown", "source": "end"}),
        )
        .unwrap();
        let cells = nb["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[3]["cell_type"], "markdown");
        assert!(cells[3].get("id").is_none(), "4.4 cells must not grow ids");
    }

    #[test]
    fn insert_and_address_by_id_on_v45() {
        let (nb, out, report) = edit(
            V45,
            json!({"path": "a.ipynb", "operation": "insert", "cell_id": "e5f6a7b8", "source": "import os\n"}),
        )
        .unwrap();
        let cells = nb["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1]["cell_type"], "code");
        assert_eq!(cells[1]["outputs"], json!([]));
        let id = cells[1]["id"].as_str().unwrap();
        assert_eq!(id.len(), 8);
        assert_ne!(id, "a1b2c3d4");
        assert!(report.contains(id));
        assert_eq!(cells[2]["id"], "e5f6a7b8");

        let (nb, _, _) = edit(
            &out,
            json!({"path": "a.ipynb", "operation": "replace", "cell_id": "a1b2c3d4", "source": "2 + 2"}),
        )
        .unwrap();
        assert_eq!(nb["cells"][0]["source"], json!(["2 + 2"]));
        assert_eq!(nb["cells"][0]["outputs"], json!([]));

        let (nb, _, _) = edit(
            V45,
            json!({"path": "a.ipynb", "operation": "delete", "cell_id": "a1b2c3d4"}),
        )
        .unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
        assert_eq!(nb["cells"][0]["id"], "e5f6a7b8");
    }

    #[test]
    fn delete_by_index() {
        let (nb, _, report) = edit(
            V44,
            json!({"path": "a.ipynb", "operation": "delete", "index": 0}),
        )
        .unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 2);
        assert!(report.contains("-# Analysis"));
        assert_eq!(nb["nbformat_minor"], 4);
    }

    #[test]
    fn bad_addressing_is_an_error() {
        let cases = [
            (json!({"operation": "delete", "index": 3}), "out of range"),
            (json!({"operation": "insert", "index": 4}), "out of range"),
            (
                json!({"operation": "replace", "index": 9, "source": "x"}),
                "valid: 0..=2",
            ),
            (
                json!({"operation": "delete", "cell_id": "nope"}),
                "no cell ids",
            ),
            (json!({"operation": "delete"}), "`index` or `cell_id`"),
            (
                json!({"operation": "delete", "index": 0, "cell_id": "x"}),
                "not both",
            ),
            (
                json!({"operation": "replace", "index": 1}),
                "needs `source`",
            ),
            (
                json!({"operation": "replace", "index": 1, "source": "x", "cell_type": "markdown"}),
                "not supported",
            ),
            (
                json!({"operation": "insert", "index": 0, "cell_type": "raw"}),
                "`code` or `markdown`",
            ),
        ];
        for (mut v, want) in cases {
            v["path"] = json!("a.ipynb");
            let err = apply_edit(V44, &args(v.clone())).expect_err("should fail");
            assert!(err.contains(want), "{v}: {err}");
        }
        let err = apply_edit(
            V45,
            &args(json!({"path": "a", "operation": "delete", "cell_id": "zzz"})),
        )
        .expect_err("unknown id");
        assert!(err.contains("no cell with id `zzz`"), "{err}");
        let v3 = r#"{"nbformat": 3, "nbformat_minor": 0, "worksheets": []}"#;
        assert!(apply_edit(
            v3,
            &args(json!({"path": "a", "operation": "delete", "index": 0}))
        )
        .unwrap_err()
        .contains("nbformat 3"));
    }

    #[test]
    fn crlf_and_missing_trailing_newline_are_kept() {
        let crlf = V45.trim_end().replace('\n', "\r\n");
        let (_, out, _) = edit(
            &crlf,
            json!({"path": "a.ipynb", "operation": "replace", "index": 1, "source": "n"}),
        )
        .unwrap();
        assert!(out.contains("\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert!(!out.ends_with('\n'));
    }

    #[tokio::test]
    async fn tool_writes_through_ctx_and_respects_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.ipynb");
        std::fs::write(&path, V44).unwrap();
        let call = json!({"path": "n.ipynb", "operation": "delete", "index": 2});

        let ro = ToolCtx::new(
            PermissionMode::ReadOnly,
            dir.path().into(),
            dir.path().into(),
        );
        let denied = NotebookEdit.run(call.clone(), &ro).await;
        assert!(
            denied.is_error && denied.content.contains("denied"),
            "{}",
            denied.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), V44);

        let ctx = ToolCtx::new(
            PermissionMode::AutoEdit,
            dir.path().into(),
            dir.path().into(),
        );
        let out = NotebookEdit.run(call, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let nb: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 2);

        std::fs::write(dir.path().join("x.json"), "{}").unwrap();
        let wrong = NotebookEdit
            .run(
                json!({"path": "x.json", "operation": "delete", "index": 0}),
                &ctx,
            )
            .await;
        assert!(wrong.is_error && wrong.content.contains("edit_file"));
    }

    #[test]
    fn outputs_render_with_images_named_not_inlined() {
        let r = render_notebook(V44).unwrap();
        assert!(r.contains("> stdout:\n> hello\n"), "{r}");
        assert!(
            r.contains("> display:\n> <Figure size 640x480 with 1 Axes>"),
            "{r}"
        );
        assert!(r.contains("> [image/png output not shown]"), "{r}");
        assert!(!r.contains("iVBORw0KGgo"));
        assert!(render_notebook(V45).unwrap().contains("> result:\n> 2\n"));
    }

    fn executed() -> String {
        let mut nb: Value = serde_json::from_str(V44).unwrap();
        let noisy = "x".repeat(OUTPUT_BLOCK_MAX * 10);
        nb["cells"][1]["outputs"] = json!([
            {"name": "stdout", "output_type": "stream", "text": ["a"]},
            {"name": "stdout", "output_type": "stream", "text": ["b\n", noisy]},
        ]);
        nb["cells"][2]["outputs"] = json!([{
            "ename": "NameError",
            "evalue": "name 'plot' is not defined",
            "output_type": "error",
            "traceback": [
                "\u{1b}[0;31m---------------------------------------------------------------------------\u{1b}[0m",
                "\u{1b}[0;31mNameError\u{1b}[0m: name 'plot' is not defined"
            ]
        }]);
        nb.to_string()
    }

    #[test]
    fn run_report_puts_errors_first_and_is_bounded() {
        let r = render_run(&executed(), None).unwrap();
        let err_at = r.find("## errors").expect("errors section");
        assert!(err_at < r.find("## cells").unwrap());
        assert!(r.starts_with("executed; 1 error(s)"), "{r}");
        assert!(r.contains("### cell 2: NameError: name 'plot' is not defined"));
        assert!(r.contains("NameError: name 'plot' is not defined\n"));
        assert!(!r.contains('\u{1b}'), "ANSI escapes must be stripped");
        // Adjacent stream chunks merge into one block, clipped.
        assert_eq!(r.matches("> stdout:").count(), 1);
        assert!(r.contains("> ab\n"));
        assert!(r.contains("more bytes not shown"));
        assert!(r.len() < OUTPUT_BLOCK_MAX * 3, "len {}", r.len());
    }

    #[test]
    fn run_report_for_one_cell() {
        let r = render_run(&executed(), Some(0)).unwrap();
        assert!(
            r.starts_with("executed; 0 error(s) (1 more in other cells)"),
            "{r}"
        );
        assert!(!r.contains("## errors"));
        assert!(r.contains("<!-- cell 0: markdown -->"));
        assert!(!r.contains("<!-- cell 1"));
    }

    #[test]
    fn nbconvert_argv() {
        let cmd = nbconvert_command("analysis.ipynb", 120).unwrap();
        assert!(cmd.starts_with("jupyter nbconvert --to notebook --execute --inplace "));
        assert!(cmd.contains("--ExecutePreprocessor.allow_errors=True"));
        assert!(cmd.contains("--ExecutePreprocessor.timeout=120"));
        if cfg!(windows) {
            assert!(cmd.ends_with(" ./analysis.ipynb"), "{cmd}");
            assert!(nbconvert_command("my notebook.ipynb", 1).is_err());
            assert!(nbconvert_command("a&calc.ipynb", 1).is_err());
        } else {
            assert!(cmd.ends_with(" './analysis.ipynb'"), "{cmd}");
            assert!(nbconvert_command("it's; rm -rf ~.ipynb", 1)
                .unwrap()
                .ends_with(r#" './it'\''s; rm -rf ~.ipynb'"#));
        }
    }

    #[tokio::test]
    async fn run_validates_before_executing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.ipynb"), V44).unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.path().into(), dir.path().into());
        let out = NotebookRun
            .run(json!({"path": "n.ipynb", "cell": 7}), &ctx)
            .await;
        assert!(
            out.is_error && out.content.contains("out of range"),
            "{}",
            out.content
        );
        let out = NotebookRun.run(json!({"path": "n.py"}), &ctx).await;
        assert!(out.is_error && out.content.contains(".ipynb"));
    }
}

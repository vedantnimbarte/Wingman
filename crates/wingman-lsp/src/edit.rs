//! Apply a server-produced `WorkspaceEdit` to the filesystem.
//!
//! `textDocument/rename` returns a `WorkspaceEdit` describing every edit needed
//! across the project to perform the rename consistently. This module parses
//! that (both the `changes` map and the newer `documentChanges` array shapes)
//! and applies the edits.
//!
//! LSP positions are `(line, character)` where `character` counts **UTF-16 code
//! units**, so we convert carefully rather than assuming one char == one byte.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::client::uri_to_path;

/// A single text replacement over a half-open range, positions 0-based.
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub start_line: u32,
    pub start_char: u32,
    pub end_line: u32,
    pub end_char: u32,
    pub new_text: String,
}

/// All edits targeting one file.
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

fn parse_edit(v: &Value) -> Option<TextEdit> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(TextEdit {
        start_line: start.get("line")?.as_u64()? as u32,
        start_char: start.get("character")?.as_u64()? as u32,
        end_line: end.get("line")?.as_u64()? as u32,
        end_char: end.get("character")?.as_u64()? as u32,
        new_text: v
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Parse a `WorkspaceEdit` JSON into per-file edit lists. Handles both the
/// `changes` object and the `documentChanges` array forms.
pub fn parse_workspace_edit(we: &Value) -> Vec<FileEdit> {
    let mut by_uri: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();

    if let Some(changes) = we.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(arr) = edits.as_array() {
                let list = by_uri.entry(uri.clone()).or_default();
                list.extend(arr.iter().filter_map(parse_edit));
            }
        }
    }

    if let Some(doc_changes) = we.get("documentChanges").and_then(Value::as_array) {
        for dc in doc_changes {
            // Only plain text-edit document changes; skip create/rename/delete
            // file operations (they carry no `edits`).
            let Some(uri) = dc
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if let Some(arr) = dc.get("edits").and_then(Value::as_array) {
                let list = by_uri.entry(uri.to_string()).or_default();
                list.extend(arr.iter().filter_map(parse_edit));
            }
        }
    }

    by_uri
        .into_iter()
        .filter_map(|(uri, edits)| uri_to_path(&uri).map(|path| FileEdit { path, edits }))
        .collect()
}

/// Byte offsets of the first character of each line (line 0 starts at 0).
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Convert an LSP `(line, utf16_char)` position to a byte offset in `text`.
/// Clamps out-of-range positions to the end of the line / text so a slightly
/// stale position never panics.
fn position_to_byte(text: &str, starts: &[usize], line: u32, utf16_char: u32) -> usize {
    let line = line as usize;
    if line >= starts.len() {
        return text.len();
    }
    let line_start = starts[line];
    let line_end = if line + 1 < starts.len() {
        starts[line + 1]
    } else {
        text.len()
    };
    let line_text = &text[line_start..line_end];

    let mut utf16_seen = 0u32;
    for (byte_off, ch) in line_text.char_indices() {
        if utf16_seen >= utf16_char {
            return line_start + byte_off;
        }
        utf16_seen += ch.len_utf16() as u32;
    }
    // Position is at or past end of line content.
    line_end
}

/// Apply `edits` to `text`, returning the new text. Edits are applied from the
/// last position to the first so earlier byte offsets stay valid.
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let starts = line_starts(text);
    // Resolve each edit to byte ranges, then sort by start descending.
    let mut resolved: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let s = position_to_byte(text, &starts, e.start_line, e.start_char);
            let en = position_to_byte(text, &starts, e.end_line, e.end_char);
            (s.min(en), s.max(en), e.new_text.as_str())
        })
        .collect();
    resolved.sort_by_key(|&(start, _, _)| std::cmp::Reverse(start));

    let mut out = text.to_string();
    for (start, end, new_text) in resolved {
        if start <= out.len() && end <= out.len() {
            out.replace_range(start..end, new_text);
        }
    }
    out
}

/// Apply a whole `WorkspaceEdit` to disk. Returns the list of paths changed.
/// Best-effort: a file that can't be read is skipped (reported via the return
/// set only including files actually written).
pub async fn apply_workspace_edit(we: &Value) -> std::io::Result<Vec<PathBuf>> {
    let file_edits = parse_workspace_edit(we);
    let mut changed = Vec::new();
    for fe in file_edits {
        let Ok(text) = tokio::fs::read_to_string(&fe.path).await else {
            continue;
        };
        let updated = apply_edits(&text, &fe.edits);
        if updated != text {
            tokio::fs::write(&fe.path, updated).await?;
            changed.push(fe.path);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_line_replace() {
        let text = "let foo = 1;\n";
        let edits = vec![TextEdit {
            start_line: 0,
            start_char: 4,
            end_line: 0,
            end_char: 7,
            new_text: "bar".into(),
        }];
        assert_eq!(apply_edits(text, &edits), "let bar = 1;\n");
    }

    #[test]
    fn multiple_edits_same_line_apply_right_to_left() {
        let text = "foo + foo\n";
        let edits = vec![
            TextEdit {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 3,
                new_text: "bar".into(),
            },
            TextEdit {
                start_line: 0,
                start_char: 6,
                end_line: 0,
                end_char: 9,
                new_text: "baz".into(),
            },
        ];
        assert_eq!(apply_edits(text, &edits), "bar + baz\n");
    }

    #[test]
    fn multi_line_range_replace() {
        let text = "a\nb\nc\n";
        // Replace from (0,0) to (2,0): removes "a\nb\n".
        let edits = vec![TextEdit {
            start_line: 0,
            start_char: 0,
            end_line: 2,
            end_char: 0,
            new_text: "X\n".into(),
        }];
        assert_eq!(apply_edits(text, &edits), "X\nc\n");
    }

    #[test]
    fn utf16_offsets_account_for_wide_chars() {
        // "😀" is 2 UTF-16 units, 4 UTF-8 bytes. The identifier after it starts
        // at UTF-16 char 3 (emoji=0..2, space=2..3? here we put emoji then id).
        let text = "let 😀x = 1;\n"; // chars: l e t sp 😀 x ...
                                     // 'x' is at UTF-16 column: "let " = 4 units, 😀 = 2 → x at column 6.
        let edits = vec![TextEdit {
            start_line: 0,
            start_char: 6,
            end_line: 0,
            end_char: 7,
            new_text: "y".into(),
        }];
        assert_eq!(apply_edits(text, &edits), "let 😀y = 1;\n");
    }

    #[test]
    fn parses_changes_and_document_changes() {
        let we = json!({
            "changes": {
                "file:///a.rs": [
                    { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "newText": "Z" }
                ]
            }
        });
        let fes = parse_workspace_edit(&we);
        assert_eq!(fes.len(), 1);
        assert_eq!(fes[0].edits.len(), 1);

        let we2 = json!({
            "documentChanges": [
                { "textDocument": { "uri": "file:///b.rs", "version": 1 }, "edits": [
                    { "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 3 } }, "newText": "Q" }
                ]}
            ]
        });
        let fes2 = parse_workspace_edit(&we2);
        assert_eq!(fes2.len(), 1);
        assert_eq!(fes2[0].edits[0].new_text, "Q");
    }

    #[test]
    fn out_of_range_position_clamps_without_panic() {
        let text = "short\n";
        let edits = vec![TextEdit {
            start_line: 99,
            start_char: 99,
            end_line: 99,
            end_char: 99,
            new_text: "!".into(),
        }];
        // Should not panic; appends at end.
        let out = apply_edits(text, &edits);
        assert!(out.ends_with('!'));
    }
}

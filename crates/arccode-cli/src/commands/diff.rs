//! `arccode diff` — interactive unified-diff hunk viewer with accept /
//! reject per hunk.
//!
//! Two modes:
//!   - `arccode diff <file>` → runs `git diff -- <file>` and walks each hunk
//!     against the working-tree file. Accepting keeps the change; rejecting
//!     restores the pre-change lines.
//!   - `arccode diff --patch <file.patch>` → applies a unified-diff file
//!     against on-disk paths the same way.
//!
//! Controls per hunk:
//!     a  accept the hunk
//!     r  reject (keep current file content for this hunk)
//!     s  skip (decide later — same as reject for this run)
//!     q  quit without writing
//!     ?  show help

use anyhow::{Context, Result};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

pub async fn run(file: Option<String>, patch: Option<String>) -> Result<ExitCode> {
    let diff_text = if let Some(p) = patch {
        std::fs::read_to_string(&p).with_context(|| format!("read patch {p}"))?
    } else if let Some(f) = file {
        run_git_diff(&f)?
    } else {
        eprintln!("arccode: pass <file> or --patch <file>");
        return Ok(ExitCode::from(1));
    };

    if diff_text.trim().is_empty() {
        println!("arccode: no diff to review");
        return Ok(ExitCode::SUCCESS);
    }

    let files = parse_unified_diff(&diff_text);
    if files.is_empty() {
        eprintln!("arccode: no parseable file sections in diff");
        return Ok(ExitCode::from(1));
    }

    let mut total_accept = 0usize;
    let mut total_reject = 0usize;
    let mut written_files = 0usize;
    let mut quit = false;

    for fd in files {
        if quit {
            break;
        }
        let path = match resolve_target(&fd) {
            Some(p) => p,
            None => {
                eprintln!("arccode: skipping file '{}' (no target path)", fd.new_path);
                continue;
            }
        };
        println!("\n=== {} → {} ({} hunk(s)) ===", fd.old_path, fd.new_path, fd.hunks.len());
        let original = if path.exists() {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        let original_lines: Vec<String> = original.lines().map(String::from).collect();
        // Build the new file by walking hunks in order.
        let mut new_lines: Vec<String> = Vec::new();
        let mut cursor = 0usize;
        let mut applied_any = false;
        for (i, h) in fd.hunks.iter().enumerate() {
            // Catch up unmodified prefix.
            let start = h.old_start.saturating_sub(1);
            while cursor < start.min(original_lines.len()) {
                new_lines.push(original_lines[cursor].clone());
                cursor += 1;
            }
            // Show the hunk and ask the user.
            print_hunk(i + 1, fd.hunks.len(), h);
            let choice = prompt_choice()?;
            match choice {
                Choice::Quit => {
                    quit = true;
                    break;
                }
                Choice::Accept => {
                    // Skip the "minus" + "context" lines of the hunk in the
                    // original (h.old_len lines), then push the "plus" +
                    // "context" lines into new_lines.
                    for nl in &h.new_block {
                        new_lines.push(nl.clone());
                    }
                    cursor = (start + h.old_len).min(original_lines.len());
                    total_accept += 1;
                    applied_any = true;
                }
                Choice::Reject | Choice::Skip => {
                    // Keep original lines unchanged.
                    for ol in original_lines.iter().skip(start).take(h.old_len) {
                        new_lines.push(ol.clone());
                    }
                    cursor = (start + h.old_len).min(original_lines.len());
                    total_reject += 1;
                }
            }
        }
        // Tail of original after the last hunk.
        while cursor < original_lines.len() {
            new_lines.push(original_lines[cursor].clone());
            cursor += 1;
        }

        if applied_any && !quit {
            let mut body = new_lines.join("\n");
            if original.ends_with('\n') || !original.is_empty() {
                body.push('\n');
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, body)
                .with_context(|| format!("write {}", path.display()))?;
            println!("→ wrote {}", path.display());
            written_files += 1;
        } else if quit {
            println!("(quitting without writing changes for {})", path.display());
        } else {
            println!("(no hunks accepted for {} — file untouched)", path.display());
        }
    }

    println!(
        "\ndone: accepted {}, rejected {}, files written {}",
        total_accept, total_reject, written_files
    );
    Ok(ExitCode::SUCCESS)
}

enum Choice {
    Accept,
    Reject,
    Skip,
    Quit,
}

fn prompt_choice() -> Result<Choice> {
    loop {
        print!("[a]ccept / [r]eject / [s]kip / [q]uit / [?] help: ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(Choice::Quit);
        }
        match line.trim() {
            "a" | "A" | "y" => return Ok(Choice::Accept),
            "r" | "R" | "n" => return Ok(Choice::Reject),
            "s" | "S" => return Ok(Choice::Skip),
            "q" | "Q" => return Ok(Choice::Quit),
            "?" | "h" => {
                println!(
                    "a accept this hunk · r reject (keep current) · s skip · q quit without writing"
                );
            }
            other => {
                println!("(unrecognized '{other}'; try a/r/s/q/?)");
            }
        }
    }
}

fn print_hunk(idx: usize, total: usize, h: &Hunk) {
    println!(
        "\n--- hunk {idx}/{total} @ -{},{} +{},{} ---",
        h.old_start, h.old_len, h.new_start, h.new_len
    );
    for line in &h.raw {
        let glyph = line.chars().next().unwrap_or(' ');
        let color = match glyph {
            '+' => "\x1b[32m",
            '-' => "\x1b[31m",
            _ => "",
        };
        let reset = if color.is_empty() { "" } else { "\x1b[0m" };
        println!("{color}{line}{reset}");
    }
}

fn run_git_diff(file: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["diff", "--no-color", "--", file])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("running `git diff`")?;
    if !out.status.success() {
        anyhow::bail!("`git diff -- {file}` failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A single file's section of a unified diff.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    /// Raw `+`/`-`/` ` lines exactly as they appeared in the diff.
    pub raw: Vec<String>,
    /// The "new" version of the hunk body (context + additions), ready to
    /// drop into the destination file.
    pub new_block: Vec<String>,
}

/// Parse a unified diff into per-file hunk lists. Tolerant: ignores
/// `\ No newline at end of file` markers and stray lines.
pub fn parse_unified_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut current_hunk: Option<Hunk> = None;

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("diff --git") {
            // Push the previous file (if any).
            if let Some(mut f) = current.take() {
                if let Some(h) = current_hunk.take() {
                    f.hunks.push(h);
                }
                files.push(f);
            }
            current = Some(FileDiff {
                old_path: String::new(),
                new_path: String::new(),
                hunks: Vec::new(),
            });
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if let Some(f) = current.as_mut() {
                f.old_path = rest.strip_prefix("a/").unwrap_or(rest).trim().to_string();
            }
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(f) = current.as_mut() {
                f.new_path = rest.strip_prefix("b/").unwrap_or(rest).trim().to_string();
            }
            i += 1;
            continue;
        }
        if line.starts_with("@@") {
            // Close any in-flight hunk.
            if let (Some(f), Some(h)) = (current.as_mut(), current_hunk.take()) {
                f.hunks.push(h);
            }
            current_hunk = parse_hunk_header(line);
            i += 1;
            continue;
        }
        if line.starts_with("\\ No newline") {
            i += 1;
            continue;
        }
        if let Some(h) = current_hunk.as_mut() {
            if line.starts_with('+') || line.starts_with('-') || line.starts_with(' ') {
                h.raw.push(line.to_string());
                if line.starts_with('+') {
                    h.new_block.push(line[1..].to_string());
                } else if line.starts_with(' ') {
                    h.new_block.push(line[1..].to_string());
                }
                // '-' lines are dropped from new_block by design.
            }
        }
        i += 1;
    }
    if let Some(mut f) = current.take() {
        if let Some(h) = current_hunk.take() {
            f.hunks.push(h);
        }
        files.push(f);
    }

    // Files without paths (unified diff with only --- / +++) are also
    // valid — but require old_path/new_path to be set or we cannot
    // resolve a target. Drop those silently.
    files
        .into_iter()
        .filter(|f| !f.new_path.is_empty() || !f.old_path.is_empty())
        .collect()
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    // Format: @@ -<old_start>[,<old_len>] +<new_start>[,<new_len>] @@ ...
    let body = line.trim_start_matches('@').trim();
    let parts: Vec<&str> = body.split_whitespace().collect();
    let mut old = (1usize, 1usize);
    let mut new = (1usize, 1usize);
    for p in parts {
        if let Some(rest) = p.strip_prefix('-') {
            old = parse_range(rest)?;
        } else if let Some(rest) = p.strip_prefix('+') {
            new = parse_range(rest)?;
        }
    }
    Some(Hunk {
        old_start: old.0,
        old_len: old.1,
        new_start: new.0,
        new_len: new.1,
        raw: Vec::new(),
        new_block: Vec::new(),
    })
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (start, len) = match s.split_once(',') {
        Some((a, b)) => (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?),
        None => (s.parse::<usize>().ok()?, 1),
    };
    Some((start, len))
}

fn resolve_target(fd: &FileDiff) -> Option<PathBuf> {
    let p = if fd.new_path.is_empty() || fd.new_path == "/dev/null" {
        &fd.old_path
    } else {
        &fd.new_path
    };
    if p.is_empty() || p == "/dev/null" {
        return None;
    }
    Some(PathBuf::from(p))
}

// Allow Read import even if unused on some platforms.
#[allow(dead_code)]
fn _hush_read(_: &dyn Read) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_diff() {
        let d = "diff --git a/x.txt b/x.txt\n\
                 --- a/x.txt\n+++ b/x.txt\n\
                 @@ -1,2 +1,2 @@\n-old\n+new\n unchanged\n";
        let files = parse_unified_diff(d);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path, "x.txt");
        assert_eq!(f.new_path, "x.txt");
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_len, 2);
        assert_eq!(h.new_block, vec!["new".to_string(), "unchanged".to_string()]);
    }

    #[test]
    fn parses_range_with_default_len() {
        assert_eq!(parse_range("12"), Some((12, 1)));
        assert_eq!(parse_range("12,4"), Some((12, 4)));
    }
}

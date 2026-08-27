//! `@file` attachment expansion for the TUI composer.
//!
//! Tokens of the form `@path/to/file` inside a user prompt are replaced with
//! the file's contents (text files) or a sentinel placeholder (image files).
//! Image files are base64-encoded and returned separately in [`ExpandResult`]
//! so that callers can forward them to providers that support vision.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// A single image attachment extracted from the prompt.
// Fields will be consumed once provider vision injection is fully wired.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    /// Original file path as typed by the user.
    pub path: String,
    /// MIME type derived from the file extension (e.g. `"image/png"`).
    pub media_type: String,
    /// Raw image bytes encoded as standard base64.
    pub base64: String,
}

/// Result returned by [`expand`].
#[derive(Debug, Default)]
pub struct ExpandResult {
    /// The prompt text with `@…` tokens replaced.
    pub prompt: String,
    /// Non-fatal warnings (e.g. file not found, unreadable).
    pub warnings: Vec<String>,
    /// Number of text attachments successfully inlined.
    pub attached: usize,
    /// Image attachments found during expansion (vision input).
    pub images: Vec<ImageAttachment>,
}

/// Image extensions that we handle as binary/vision data rather than text.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Most bytes one `@` token will inline.
///
/// Attaching a file is meant to *save* context — it spares the model a
/// `read_file` round trip. Inlining without a bound does the opposite:
/// `@debug.log` on a large file used to paste the whole thing into the prompt,
/// so the feature for spending less context was the easiest way to exhaust it,
/// and a big enough file could exhaust memory before the request was even
/// built.
///
/// 64 KiB covers essentially every source file while cutting off logs and
/// dumps. Anything larger is better read by the agent, which is what the
/// truncation marker tells it to do — and unlike tool output, an attachment
/// needs no spill file, because the original is already on disk at a path the
/// model has.
const MAX_ATTACHMENT_BYTES: usize = 64 * 1024;

/// Most bytes all `@` tokens in one prompt will inline between them.
///
/// A per-file cap alone still lets `@a @b @c …` add up to the same problem, so
/// the budget is shared across the whole expansion. Later tokens are reported
/// as skipped rather than silently dropped.
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 256 * 1024;

/// Largest image accepted, in raw bytes before base64's 4/3 expansion.
///
/// An image cannot be truncated into something meaningful, so an oversized one
/// is refused rather than mangled. 3 MiB encodes to ~4 MiB, under the ~5 MiB
/// per-image limit providers impose — a larger one would be rejected by the
/// API after being read, encoded, and sent.
const MAX_IMAGE_BYTES: u64 = 3 * 1024 * 1024;

/// Read at most `limit` bytes of `path` as UTF-8 text.
///
/// Returns the text and whether the file continued past the limit. Reads
/// `limit + 1` bytes so truncation is detectable without pulling in the rest
/// of a large file — the bound is on the *read*, not just on the result, which
/// is the half that protects memory.
///
/// Binary files are still refused, as they were when this used
/// `read_to_string`: invalid UTF-8 is an error, except for a single multi-byte
/// character straddling the cut, which is dropped.
fn read_bounded(path: &Path, limit: usize) -> std::io::Result<(String, bool)> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut buf)?;
    let overflowed = buf.len() > limit;
    buf.truncate(limit);
    match String::from_utf8(buf) {
        Ok(text) => Ok((text, overflowed)),
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            // Only tolerate invalid bytes at the very end, and only when we
            // actually cut the file — that is our own doing. Anything else is
            // a genuinely non-text file and stays an error.
            if overflowed && valid + 4 >= limit {
                let mut bytes = e.into_bytes();
                bytes.truncate(valid);
                Ok((String::from_utf8(bytes).unwrap_or_default(), true))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                ))
            }
        }
    }
}

/// Expand `@path` tokens in `prompt` relative to `root`.
///
/// - Text files are inlined as fenced code blocks.
/// - Image files are base64-encoded and stored in [`ExpandResult::images`];
///   a short `[IMAGE: filename]` placeholder is inserted in the prompt.
/// - Unresolvable tokens are left as-is and a warning is recorded.
pub fn expand(prompt: &str, root: &Path) -> ExpandResult {
    let mut result = ExpandResult {
        prompt: String::with_capacity(prompt.len()),
        ..Default::default()
    };

    // Shared across every token in this prompt, so a run of attachments
    // cannot add up to what one of them is not allowed to be.
    let mut budget = MAX_TOTAL_ATTACHMENT_BYTES;
    let mut remaining = prompt;
    while let Some(at_pos) = remaining.find('@') {
        // Copy everything before the `@`.
        result.prompt.push_str(&remaining[..at_pos]);
        remaining = &remaining[at_pos + 1..];

        // Collect the path token: runs until whitespace or end-of-string.
        let end = remaining
            .find(|c: char| c.is_whitespace())
            .unwrap_or(remaining.len());
        let token = &remaining[..end];
        remaining = &remaining[end..];

        if token.is_empty() {
            // Lone `@` with no path — pass through literally.
            result.prompt.push('@');
            continue;
        }

        let path: PathBuf = if Path::new(token).is_absolute() {
            PathBuf::from(token)
        } else {
            root.join(token)
        };

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            // --- Image attachment ---
            // Checked before reading: an image cannot be usefully truncated,
            // and a provider would reject an oversized one anyway — after we
            // had read, encoded, and uploaded it.
            let oversized = std::fs::metadata(&path)
                .map(|m| m.len() > MAX_IMAGE_BYTES)
                .unwrap_or(false);
            if oversized {
                let mib = MAX_IMAGE_BYTES / (1024 * 1024);
                result.warnings.push(format!(
                    "@{token}: image is larger than {mib} MiB and was not attached \
                     (providers reject images above roughly 5 MiB encoded)"
                ));
                result.prompt.push('@');
                result.prompt.push_str(token);
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let media_type = ext_to_media_type(&ext);
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or(token);
                    result.prompt.push_str(&format!("[IMAGE: {filename}]"));
                    result.images.push(ImageAttachment {
                        path: token.to_string(),
                        media_type: media_type.to_string(),
                        base64: b64,
                    });
                }
                Err(e) => {
                    result
                        .warnings
                        .push(format!("@{token}: cannot read image: {e}"));
                    result.prompt.push('@');
                    result.prompt.push_str(token);
                }
            }
        } else {
            // --- Text attachment ---
            if budget == 0 {
                result.warnings.push(format!(
                    "@{token}: not attached — this prompt already inlined its {} KiB \
                     of attachments; ask the agent to read it instead",
                    MAX_TOTAL_ATTACHMENT_BYTES / 1024
                ));
                result.prompt.push('@');
                result.prompt.push_str(token);
                continue;
            }
            let limit = MAX_ATTACHMENT_BYTES.min(budget);
            match read_bounded(&path, limit) {
                Ok((contents, truncated)) => {
                    budget = budget.saturating_sub(contents.len());
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or(token);
                    result
                        .prompt
                        .push_str(&format!("```{filename}\n{contents}"));
                    if truncated {
                        // Point at the original rather than spilling a copy:
                        // unlike tool output, the whole file is already on disk
                        // at a path the model can hand to `read_file`.
                        result.prompt.push_str(&format!(
                            "\n… [wingman] truncated at {} KiB — read the rest with \
                             read_file(path: \"{}\", offset, limit) …",
                            limit / 1024,
                            path.display()
                        ));
                        result
                            .warnings
                            .push(format!("@{token}: truncated at {} KiB", limit / 1024));
                    }
                    result.prompt.push_str("\n```");
                    result.attached += 1;
                }
                Err(e) => {
                    result.warnings.push(format!("@{token}: {e}"));
                    result.prompt.push('@');
                    result.prompt.push_str(token);
                }
            }
        }
    }

    // Append whatever's left after the last `@` (or the whole string if none).
    result.prompt.push_str(remaining);
    result
}

fn ext_to_media_type(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn no_at_tokens_passes_through() {
        let r = expand("hello world", Path::new("/tmp"));
        assert_eq!(r.prompt, "hello world");
        assert!(r.warnings.is_empty());
        assert_eq!(r.attached, 0);
        assert!(r.images.is_empty());
    }

    #[test]
    fn lone_at_passes_through() {
        let r = expand("email me @ foo", Path::new("/tmp"));
        assert_eq!(r.prompt, "email me @ foo");
    }

    #[test]
    fn missing_file_produces_warning() {
        let r = expand("see @does_not_exist.txt", Path::new("/tmp"));
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("does_not_exist.txt"));
    }

    #[test]
    fn text_file_is_inlined() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "hello").unwrap();
        let path = f.path().to_string_lossy().to_string();
        // Use absolute path so root doesn't matter.
        let prompt = format!("contents: @{path}");
        let r = expand(&prompt, Path::new("/tmp"));
        assert!(r.prompt.contains("hello"), "got: {}", r.prompt);
        assert_eq!(r.attached, 1);
        assert!(r.warnings.is_empty());
    }

    /// The foot-gun this closes: `@debug.log` used to paste an entire file
    /// into the prompt, so the feature meant to save context was the easiest
    /// way to exhaust it.
    #[test]
    fn a_large_file_is_truncated_and_says_where_the_rest_is() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let line = "x".repeat(99);
        for _ in 0..5000 {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        let path = f.path().to_string_lossy().to_string();

        let r = expand(&format!("look at @{path}"), Path::new("/tmp"));
        assert_eq!(r.attached, 1);
        assert!(
            r.prompt.len() < MAX_ATTACHMENT_BYTES + 1024,
            "inlined {} bytes despite the cap",
            r.prompt.len()
        );
        assert!(r.prompt.contains("truncated at"));
        // Actionable: the model is told how to get the part that was cut.
        assert!(r.prompt.contains("read_file"));
        assert!(r.prompt.contains(&path));
        // And the user is told, rather than it happening silently.
        assert!(r.warnings.iter().any(|w| w.contains("truncated")));
    }

    #[test]
    fn a_small_file_is_untouched_and_unmarked() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "fn main() {{}}").unwrap();
        f.flush().unwrap();
        let path = f.path().to_string_lossy().to_string();
        let r = expand(&format!("@{path}"), Path::new("/tmp"));
        assert!(r.prompt.contains("fn main()"));
        assert!(!r.prompt.contains("truncated"));
        assert!(r.warnings.is_empty());
    }

    /// A per-file cap alone still lets a run of attachments add up.
    #[test]
    fn the_budget_is_shared_across_one_prompt() {
        let mut files = Vec::new();
        let mut prompt = String::new();
        for _ in 0..8 {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            let chunk = "y".repeat(1000);
            for _ in 0..60 {
                writeln!(f, "{chunk}").unwrap();
            }
            f.flush().unwrap();
            prompt.push_str(&format!("@{} ", f.path().to_string_lossy()));
            files.push(f);
        }
        let r = expand(&prompt, Path::new("/tmp"));
        assert!(
            r.prompt.len() < MAX_TOTAL_ATTACHMENT_BYTES + 8 * 1024,
            "eight attachments inlined {} bytes, over the shared budget",
            r.prompt.len()
        );
        assert!(
            r.warnings.iter().any(|w| w.contains("already inlined")),
            "a skipped attachment must be reported, not silently dropped: {:?}",
            r.warnings
        );
    }

    #[test]
    fn a_binary_file_is_still_refused_rather_than_inlined_as_garbage() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[0xff, 0xfe, 0x00, 0x01, 0xff]).unwrap();
        f.flush().unwrap();
        let path = f.path().to_string_lossy().to_string();
        let r = expand(&format!("@{path}"), Path::new("/tmp"));
        assert_eq!(r.attached, 0, "binary content must not be inlined");
        assert_eq!(r.warnings.len(), 1);
        assert!(r.prompt.contains(&format!("@{path}")), "token left literal");
    }

    /// Truncating mid-character must not corrupt the text or panic.
    #[test]
    fn cutting_through_a_multibyte_character_is_clean() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Three-byte characters guarantee the cut lands mid-character for at
        // least one of the offsets a fixed byte limit can produce.
        let blob = "日".repeat(40_000);
        f.write_all(blob.as_bytes()).unwrap();
        f.flush().unwrap();
        let (text, truncated) = read_bounded(f.path(), MAX_ATTACHMENT_BYTES).unwrap();
        assert!(truncated);
        assert!(text.chars().all(|c| c == '日'), "text was corrupted");
        assert!(text.len() <= MAX_ATTACHMENT_BYTES);
    }

    #[test]
    fn an_oversized_image_is_refused_not_encoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.png");
        std::fs::write(&path, vec![0u8; (MAX_IMAGE_BYTES + 1) as usize]).unwrap();
        let r = expand(&format!("@{}", path.to_string_lossy()), Path::new("/tmp"));
        assert!(
            r.images.is_empty(),
            "an oversized image must not be encoded"
        );
        assert!(r.warnings.iter().any(|w| w.contains("larger than")));
    }

    #[test]
    fn a_normal_image_still_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.png");
        std::fs::write(&path, vec![0u8; 128]).unwrap();
        let r = expand(&format!("@{}", path.to_string_lossy()), Path::new("/tmp"));
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].media_type, "image/png");
        assert!(r.prompt.contains("[IMAGE: small.png]"));
    }
}

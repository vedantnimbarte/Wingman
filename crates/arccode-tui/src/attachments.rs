//! `@file` attachment expansion for the TUI composer.
//!
//! Tokens of the form `@path/to/file` inside a user prompt are replaced with
//! the file's contents (text files) or a sentinel placeholder (image files).
//! Image files are base64-encoded and returned separately in [`ExpandResult`]
//! so that callers can forward them to providers that support vision.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// A single image attachment extracted from the prompt.
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
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let media_type = ext_to_media_type(&ext);
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(token);
                    result
                        .prompt
                        .push_str(&format!("[IMAGE: {filename}]"));
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
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(token);
                    result.prompt.push_str(&format!(
                        "```{filename}\n{contents}\n```"
                    ));
                    result.attached += 1;
                }
                Err(e) => {
                    result
                        .warnings
                        .push(format!("@{token}: {e}"));
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
}

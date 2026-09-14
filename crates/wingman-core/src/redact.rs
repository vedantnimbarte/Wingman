//! High-confidence secret redaction.
//!
//! One scanner for every place Wingman shows or hands on text it did not
//! write: tool output on its way to the model (`wingman-tools`' registry) and
//! session exports on their way to a PR description or a colleague. Only
//! unambiguous credential shapes match, so ordinary code and prose — commit
//! hashes included — pass through untouched.

/// Redact high-confidence secret tokens in `text`, returning `(redacted, n)`.
/// Only matches unambiguous credential shapes so it doesn't mangle normal
/// output.
pub fn redact_output_secrets(text: &str) -> (String, usize) {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_-]{20,}",        // OpenAI / Anthropic-style
            r"gh[pousr]_[A-Za-z0-9]{30,}",   // GitHub tokens
            r"AKIA[0-9A-Z]{16}",             // AWS access key id
            r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack tokens
            r"AIza[0-9A-Za-z_-]{35}",        // Google API key
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", // JWT
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", // PEM keys
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    });
    let mut out = text.to_string();
    let mut n = 0usize;
    for re in patterns {
        let count = re.find_iter(&out).count();
        if count > 0 {
            n += count;
            out = re.replace_all(&out, "[redacted-secret]").into_owned();
        }
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_high_confidence_output_tokens() {
        let (out, n) = redact_output_secrets(
            "key=sk-abcdefghij0123456789ABCDEF and AKIAIOSFODNN7EXAMPLE plus normal text",
        );
        assert_eq!(n, 2);
        assert!(!out.contains("sk-abcdefghij"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(out.contains("normal text"));
        assert!(out.contains("[redacted-secret]"));
    }

    #[test]
    fn leaves_ordinary_output_untouched() {
        let text = "fn main() { let key = compute(); println!(\"{key}\"); }";
        let (out, n) = redact_output_secrets(text);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }
}

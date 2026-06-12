//! Best-effort secret scrubbing applied before session text is embedded
//! into the recall stores. Sessions routinely contain pasted env files,
//! shell exports, and provider keys; those must never end up inside
//! `sessions.db` where they outlive the transcript.
//!
//! Deliberately dependency-free (no regex crate): we scan for well-known
//! credential prefixes and for `key = value` assignments whose key looks
//! secret-ish, then replace the value with `[REDACTED]`.

/// Known credential prefixes. A match redacts the prefix and the token-run
/// that follows it (alphanumerics plus `-_.`), provided the run is long
/// enough to plausibly be a credential.
const PREFIXES: &[&str] = &[
    "sk-ant-",        // Anthropic
    "sk-proj-",       // OpenAI project keys
    "sk-",            // OpenAI (after the more specific ones)
    "ghp_",           // GitHub personal access token
    "gho_",           // GitHub OAuth token
    "ghs_",           // GitHub server token
    "github_pat_",    // GitHub fine-grained PAT
    "xoxb-",          // Slack bot token
    "xoxp-",          // Slack user token
    "AKIA",           // AWS access key id
    "AIza",           // Google API key
    "glpat-",         // GitLab PAT
];

/// Key names that mark the value of a `key=value` / `key: value` pair as
/// secret. Compared case-insensitively against the end of the key.
const SECRET_KEYS: &[&str] = &["api_key", "apikey", "secret", "password", "passwd", "token"];

const REPLACEMENT: &str = "[REDACTED]";

/// Minimum token-run length after a known prefix before we treat it as a
/// credential (avoids redacting prose like "sk-learn").
const MIN_TOKEN_LEN: usize = 8;

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=')
}

/// Scrub one text blob. Returns the input unchanged (no allocation churn
/// beyond one pass) when nothing matched.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

fn redact_line(line: &str) -> String {
    let mut result = line.to_string();

    // Pass 1: known credential prefixes anywhere in the line.
    let mut scan_from = 0;
    while scan_from < result.len() {
        let Some((abs_start, prefix_len)) = find_prefix(&result[scan_from..])
            .map(|(rel, plen)| (scan_from + rel, plen))
        else {
            break;
        };
        let token_start = abs_start + prefix_len;
        let token_len = result[token_start..]
            .chars()
            .take_while(|&c| is_token_char(c))
            .map(char::len_utf8)
            .sum::<usize>();
        if token_len >= MIN_TOKEN_LEN {
            result.replace_range(abs_start..token_start + token_len, REPLACEMENT);
            scan_from = abs_start + REPLACEMENT.len();
        } else {
            scan_from = token_start;
        }
    }

    // Pass 2: `secret_key = value` style assignments.
    for sep in ['=', ':'] {
        if let Some(idx) = result.find(sep) {
            let key = result[..idx].trim().trim_matches('"').trim_matches('\'');
            let key_lower = key.to_ascii_lowercase();
            let is_secret_key = SECRET_KEYS
                .iter()
                .any(|s| key_lower == *s || key_lower.ends_with(&format!("_{s}")));
            if is_secret_key {
                let value = result[idx + 1..].trim();
                if value.len() >= MIN_TOKEN_LEN && !value.contains(REPLACEMENT) {
                    let had_newline = result.ends_with('\n');
                    result = format!("{}{sep} {REPLACEMENT}", &result[..idx]);
                    if had_newline {
                        result.push('\n');
                    }
                }
                break;
            }
        }
    }

    result
}

/// Find the earliest known prefix in `s`, longest match first at each
/// position so `sk-ant-` wins over `sk-`.
fn find_prefix(s: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for p in PREFIXES {
        if let Some(i) = s.find(p) {
            match best {
                Some((bi, bl)) if bi < i || (bi == i && bl >= p.len()) => {}
                _ => best = Some((i, p.len())),
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_prefixes() {
        let s = "export ANTHROPIC_KEY=sk-ant-api03-AbCdEf123456789xyz and done";
        let r = redact_secrets(s);
        assert!(!r.contains("sk-ant-api03"), "got: {r}");
        assert!(r.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_github_and_aws_tokens() {
        let r = redact_secrets("token ghp_AbCdEf123456789xyzAbCdEf12 then AKIAIOSFODNN7EXAMPLE");
        assert!(!r.contains("ghp_AbCdEf"), "got: {r}");
        assert!(!r.contains("AKIAIOSFODNN7"), "got: {r}");
    }

    #[test]
    fn redacts_secret_assignments() {
        let r = redact_secrets("DB_PASSWORD=hunter2hunter2\napi_key: abcdefghijklmnop\n");
        assert!(!r.contains("hunter2"), "got: {r}");
        assert!(!r.contains("abcdefghijklmnop"), "got: {r}");
        assert_eq!(r.lines().count(), 2);
    }

    #[test]
    fn leaves_normal_text_alone() {
        let s = "USER: how does the cache work in the loop?\nASSIST: via tool_cache.\n";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn short_runs_are_not_credentials() {
        let s = "we use sk-learn for clustering";
        assert_eq!(redact_secrets(s), s);
    }
}

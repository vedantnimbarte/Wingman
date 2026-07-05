//! J3 — multi-channel intake (talk to it from anywhere).
//!
//! Goals shouldn't only arrive via CLI. Pluggable adapters (GitHub issue
//! / comment, Slack, email, webhook, file-drop) each normalise their raw
//! input into a single [`Goal`] with a [`TrustLevel`], which the daemon
//! queue and the J2 scorer consume uniformly. The adapters themselves are
//! I/O; this module is the normalisation + trust-classification core.

/// Where a goal came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Cli,
    GithubIssue,
    GithubComment,
    Slack,
    Email,
    Webhook,
    FileDrop,
    /// J14 — voice intake, after transcription.
    Voice,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::GithubIssue => "github_issue",
            Self::GithubComment => "github_comment",
            Self::Slack => "slack",
            Self::Email => "email",
            Self::Webhook => "webhook",
            Self::FileDrop => "file_drop",
            Self::Voice => "voice",
        }
    }
}

/// How much rope a goal from this source gets, before the J2 scorer even
/// runs. The CLI is implicitly trusted (the operator typed it); other
/// channels earn trust via allowlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustLevel {
    /// Unknown author / unlisted source — never auto-runs.
    Untrusted,
    /// Recognised source but not an allowlisted author.
    Known,
    /// Allowlisted author or self-generated — eligible for auto-run.
    Trusted,
}

/// A normalised goal from any channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    pub text: String,
    pub source: Channel,
    pub author: Option<String>,
    pub trust_level: TrustLevel,
}

/// Strip a leading trigger token (e.g. `@wingman`, `/wingman`) from a
/// comment/mention body and return the goal text.
pub fn strip_trigger(body: &str, trigger: &str) -> Option<String> {
    let b = body.trim();
    let t = trigger.trim();
    if t.is_empty() {
        return Some(b.to_string());
    }
    // Match the trigger at the start (case-insensitive).
    if b.len() >= t.len() && b[..t.len()].eq_ignore_ascii_case(t) {
        let rest = b[t.len()..].trim_start_matches([':', ' ', '\t']).trim();
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    } else {
        None
    }
}

/// Classify trust for a goal arriving on `source` from `author`, given the
/// allowlists. CLI is always trusted; an author on `trusted_authors` is
/// trusted; a recognised source with an unknown author is `Known`;
/// anything else `Untrusted`.
pub fn classify_trust(
    source: Channel,
    author: Option<&str>,
    trusted_authors: &[String],
) -> TrustLevel {
    if source == Channel::Cli {
        return TrustLevel::Trusted;
    }
    if let Some(a) = author {
        if trusted_authors.iter().any(|t| t.eq_ignore_ascii_case(a)) {
            return TrustLevel::Trusted;
        }
        return TrustLevel::Known;
    }
    TrustLevel::Untrusted
}

/// Normalise a raw inbound message into a [`Goal`]. For comment/mention
/// channels, `trigger` strips the leading token; returns `None` when the
/// trigger is required but absent, or the text is empty.
pub fn normalize(
    source: Channel,
    raw_text: &str,
    author: Option<&str>,
    trigger: Option<&str>,
    trusted_authors: &[String],
) -> Option<Goal> {
    let text = match trigger {
        Some(t) => strip_trigger(raw_text, t)?,
        None => {
            let t = raw_text.trim();
            if t.is_empty() {
                return None;
            }
            t.to_string()
        }
    };
    Some(Goal {
        text,
        source,
        author: author.map(String::from),
        trust_level: classify_trust(source, author, trusted_authors),
    })
}

/// J3 file-drop adapter: scan `<dir>` for `*.md` goal files and return one
/// [`Goal`] per non-empty file (source `FileDrop`). Fully local — the
/// daemon points this at `.wingman/inbox/`. Files are read in sorted order
/// for determinism; unreadable/empty files are skipped. The caller is
/// responsible for deleting/archiving processed files.
pub fn scan_inbox(dir: &std::path::Path, trusted_authors: &[String]) -> Vec<Goal> {
    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();
    let mut goals = Vec::new();
    for path in entries {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Author convention: an optional first line `author: <name>`.
        let (author, text) = parse_inbox_body(&body);
        if let Some(goal) = normalize(
            Channel::FileDrop,
            &text,
            author.as_deref(),
            None,
            trusted_authors,
        ) {
            goals.push(goal);
        }
    }
    goals
}

/// Split an optional leading `author: <name>` line from the goal body.
fn parse_inbox_body(body: &str) -> (Option<String>, String) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("author:") {
        if let Some((first, remainder)) = rest.split_once('\n') {
            return (Some(first.trim().to_string()), remainder.trim().to_string());
        }
    }
    (None, body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_always_trusted() {
        assert_eq!(classify_trust(Channel::Cli, None, &[]), TrustLevel::Trusted);
    }

    #[test]
    fn allowlisted_author_is_trusted() {
        let allow = vec!["vedantnimbarte".to_string()];
        assert_eq!(
            classify_trust(Channel::GithubIssue, Some("VedantNimbarte"), &allow),
            TrustLevel::Trusted
        );
    }

    #[test]
    fn known_source_unknown_author_is_known() {
        assert_eq!(
            classify_trust(Channel::Slack, Some("randomperson"), &[]),
            TrustLevel::Known
        );
    }

    #[test]
    fn anonymous_non_cli_is_untrusted() {
        assert_eq!(
            classify_trust(Channel::Webhook, None, &[]),
            TrustLevel::Untrusted
        );
    }

    #[test]
    fn strip_trigger_handles_mention() {
        assert_eq!(
            strip_trigger("@wingman fix the parser", "@wingman"),
            Some("fix the parser".to_string())
        );
        assert_eq!(
            strip_trigger("/wingman: add a flag", "/wingman"),
            Some("add a flag".to_string())
        );
    }

    #[test]
    fn strip_trigger_rejects_missing_trigger() {
        assert_eq!(strip_trigger("just a normal comment", "@wingman"), None);
    }

    #[test]
    fn strip_trigger_rejects_empty_body() {
        assert_eq!(strip_trigger("@wingman   ", "@wingman"), None);
    }

    #[test]
    fn normalize_comment_with_trigger() {
        let allow = vec!["me".to_string()];
        let g = normalize(
            Channel::GithubComment,
            "/wingman add dark mode",
            Some("me"),
            Some("/wingman"),
            &allow,
        )
        .unwrap();
        assert_eq!(g.text, "add dark mode");
        assert_eq!(g.source, Channel::GithubComment);
        assert_eq!(g.trust_level, TrustLevel::Trusted);
    }

    #[test]
    fn normalize_plain_channel_without_trigger() {
        let g = normalize(Channel::Cli, "do the thing", None, None, &[]).unwrap();
        assert_eq!(g.text, "do the thing");
        assert_eq!(g.trust_level, TrustLevel::Trusted);
    }

    #[test]
    fn normalize_returns_none_on_missing_trigger() {
        assert!(normalize(Channel::Slack, "hi there", Some("x"), Some("@wingman"), &[]).is_none());
    }

    #[test]
    fn trust_levels_order() {
        assert!(TrustLevel::Trusted > TrustLevel::Known);
        assert!(TrustLevel::Known > TrustLevel::Untrusted);
    }

    #[test]
    fn scan_inbox_reads_goal_files() {
        let dir = std::env::temp_dir().join(format!("wingman-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "author: vedant\nadd a dark mode toggle").unwrap();
        std::fs::write(dir.join("b.md"), "fix the parser").unwrap();
        std::fs::write(dir.join("ignore.txt"), "not a goal").unwrap();
        std::fs::write(dir.join("empty.md"), "   ").unwrap();

        let goals = scan_inbox(&dir, &["vedant".to_string()]);
        assert_eq!(goals.len(), 2);
        let a = goals.iter().find(|g| g.text.contains("dark mode")).unwrap();
        assert_eq!(a.author.as_deref(), Some("vedant"));
        assert_eq!(a.trust_level, TrustLevel::Trusted);
        assert_eq!(a.source, Channel::FileDrop);
        let b = goals.iter().find(|g| g.text == "fix the parser").unwrap();
        assert_eq!(b.trust_level, TrustLevel::Untrusted); // no author
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_inbox_missing_dir_is_empty() {
        let dir = std::env::temp_dir().join("wingman-no-inbox-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(scan_inbox(&dir, &[]).is_empty());
    }
}

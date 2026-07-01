//! J14 — voice intake (opt-in).
//!
//! A local STT shim (whisper.cpp or platform-native) bound to a hotkey
//! captures speech, transcribes it, and dispatches the text to the daemon
//! intake queue as a [`Channel::Voice`](crate::intake::Channel::Voice)
//! goal. This is mostly UX gloss — useful for kicking off a goal while
//! context-switching, not for control — so it's off by default behind
//! `[pilot.intake.voice].enabled`.
//!
//! The audio capture + transcription are platform I/O (and need
//! whisper.cpp the plan defers to the user). This module is the gating +
//! normalisation: a disabled shim accepts nothing; an enabled one routes a
//! transcript through the same [`crate::intake`] path every other channel
//! uses.

use crate::intake::{self, Channel, Goal};
use crate::pr::CommandRunner;

/// Turn a transcript into an intake [`Goal`], or `None` when voice is
/// disabled or the transcript is empty. The author is whoever the hotkey
/// session is attributed to (usually the operator → trusted).
pub fn transcript_to_goal(
    transcript: &str,
    enabled: bool,
    author: Option<&str>,
    trusted_authors: &[String],
) -> Option<Goal> {
    if !enabled {
        return None;
    }
    intake::normalize(Channel::Voice, transcript, author, None, trusted_authors)
}

/// J14 capture shell: build the `whisper.cpp` (`whisper-cli`) argv to
/// transcribe a captured audio file to text on stdout. The mic capture
/// itself needs hardware; the transcription command is pure and testable.
pub fn whisper_argv(model_path: &str, audio_path: &str) -> Vec<String> {
    vec![
        "-m".to_string(),
        model_path.to_string(),
        "-f".to_string(),
        audio_path.to_string(),
        "-nt".to_string(), // no timestamps — we want clean text
        "-otxt".to_string(),
    ]
}

/// J14 transcription: run whisper.cpp (built by [`whisper_argv`]) on a
/// captured audio file via a [`CommandRunner`] and return the trimmed
/// transcript. The mic capture that produces `audio_path` needs hardware;
/// transcribing an existing file is testable through the runner seam.
pub fn transcribe_file(
    runner: &dyn CommandRunner,
    whisper_bin: &str,
    model_path: &str,
    audio_path: &str,
) -> Result<String, String> {
    let argv = whisper_argv(model_path, audio_path);
    let args: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let out = runner
        .run(whisper_bin, &args, std::path::Path::new("."))
        .map_err(|e| format!("whisper failed: {e}"))?;
    if !out.success() {
        return Err(format!("whisper exited non-zero: {}", out.stderr.trim()));
    }
    Ok(out.stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::TrustLevel;
    use crate::pr::{CommandOut, CommandRunner};
    use std::path::Path as StdPath;

    #[test]
    fn disabled_shim_accepts_nothing() {
        assert!(transcript_to_goal("add a flag", false, Some("me"), &["me".into()]).is_none());
    }

    #[test]
    fn enabled_shim_routes_through_intake() {
        let g =
            transcript_to_goal("add a dark mode toggle", true, Some("me"), &["me".into()]).unwrap();
        assert_eq!(g.text, "add a dark mode toggle");
        assert_eq!(g.source, Channel::Voice);
        assert_eq!(g.trust_level, TrustLevel::Trusted);
    }

    #[test]
    fn empty_transcript_is_none() {
        assert!(transcript_to_goal("   ", true, Some("me"), &[]).is_none());
    }

    #[test]
    fn transcribe_file_returns_trimmed_transcript() {
        struct FakeWhisper;
        impl CommandRunner for FakeWhisper {
            fn run(&self, _p: &str, _a: &[&str], _c: &StdPath) -> std::io::Result<CommandOut> {
                Ok(CommandOut {
                    status: Some(0),
                    stdout: "  add a dark mode toggle\n".into(),
                    stderr: String::new(),
                })
            }
        }
        let text = transcribe_file(&FakeWhisper, "whisper-cli", "m.bin", "clip.wav").unwrap();
        assert_eq!(text, "add a dark mode toggle");
    }

    #[test]
    fn whisper_argv_includes_model_and_audio() {
        let argv = whisper_argv("models/ggml-base.bin", "/tmp/clip.wav");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-m" && w[1] == "models/ggml-base.bin"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-f" && w[1] == "/tmp/clip.wav"));
    }

    #[test]
    fn unknown_speaker_is_known_not_trusted() {
        let g = transcript_to_goal("do the thing", true, Some("stranger"), &[]).unwrap();
        assert_eq!(g.trust_level, TrustLevel::Known);
    }
}

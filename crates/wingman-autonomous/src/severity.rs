//! Shared severity scale for findings produced by the per-task reviewer
//! (E7), the security pass (R6), the critic (J10), and the auto-merge gate
//! (E8). Keeping one ordered scale means every gate compares apples to
//! apples — `auto_merge_max_severity = "low"` means the same thing
//! whether the finding came from `wingman review` or `gitleaks`.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational / nitpick. Never blocks.
    Info,
    Low,
    Medium,
    High,
    /// Must-fix; always blocks auto-merge and escalates.
    Critical,
}

impl Severity {
    /// Rank for ordered comparison (higher = worse).
    pub fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Critical => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// True when `self` is at or above the configured `gate` threshold —
    /// i.e. this finding should block whatever the gate protects.
    pub fn meets_or_exceeds(self, gate: Severity) -> bool {
        self.rank() >= gate.rank()
    }
}

impl PartialOrd for Severity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Severity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "info" | "informational" | "nit" => Ok(Self::Info),
            "low" => Ok(Self::Low),
            "medium" | "moderate" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" | "crit" => Ok(Self::Critical),
            other => Err(format!("unknown severity '{other}'")),
        }
    }
}

/// The highest severity in a slice of findings, or `None` if empty.
pub fn max_severity<T>(findings: &[T], sev: impl Fn(&T) -> Severity) -> Option<Severity> {
    findings.iter().map(sev).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_monotonic() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Info);
    }

    #[test]
    fn parse_tolerates_aliases_and_case() {
        assert_eq!("HIGH".parse::<Severity>().unwrap(), Severity::High);
        assert_eq!("nit".parse::<Severity>().unwrap(), Severity::Info);
        assert_eq!("moderate".parse::<Severity>().unwrap(), Severity::Medium);
        assert!("bogus".parse::<Severity>().is_err());
    }

    #[test]
    fn meets_or_exceeds_gate() {
        assert!(Severity::Medium.meets_or_exceeds(Severity::Low));
        assert!(Severity::Low.meets_or_exceeds(Severity::Low));
        assert!(!Severity::Info.meets_or_exceeds(Severity::Low));
    }

    #[test]
    fn max_severity_picks_worst() {
        let sevs = [Severity::Low, Severity::Critical, Severity::Medium];
        assert_eq!(max_severity(&sevs, |s| *s), Some(Severity::Critical));
        let empty: [Severity; 0] = [];
        assert_eq!(max_severity(&empty, |s| *s), None);
    }

    #[test]
    fn roundtrip_str() {
        for s in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(s.as_str().parse::<Severity>().unwrap(), s);
        }
    }
}

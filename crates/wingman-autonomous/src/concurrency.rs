//! E9 — adaptive concurrency control.
//!
//! M1 uses a static `max_concurrent_agents`. That over-spawns when a
//! provider is rate-limiting (every worker eats 429s) and under-utilises
//! when there's headroom. This controller scales the live cap between
//! `[min, max]` from three signals:
//!
//! - **Rate-limit headroom** — recent 429 count and the largest
//!   `Retry-After` seen. Any active backoff clamps hard toward `min`.
//! - **Host CPU load** — a load factor in `[0,1]` (1.0 = saturated)
//!   linearly scales the ceiling down.
//! - **Budget burn** — `usd_spent / max_usd`. As the run approaches its
//!   cap, throttle so a runaway wave can't blow the budget in one tick.
//!
//! [`recommended_concurrency`] is a pure function. The orchestrator feeds it
//! at every assignment: rate-limit hits arrive as `agent.rate_limit` events
//! (a worker's provider reported them, see [`RateLimitWindow`]), CPU load
//! comes from a background [`CpuSampler`], and spend from the run totals.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a rate-limit hit keeps narrowing the cap after it arrives.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Rate-limit hits the run's workers reported, kept for [`RATE_LIMIT_WINDOW`].
#[derive(Debug, Default)]
pub struct RateLimitWindow {
    /// When each hit arrived, and the `Retry-After` it carried (0 = none).
    hits: VecDeque<(Instant, u32)>,
}

impl RateLimitWindow {
    pub fn record(&mut self, at: Instant, retry_after_secs: Option<u32>) {
        self.hits.push_back((at, retry_after_secs.unwrap_or(0)));
    }

    /// `(recent hits, largest Retry-After still in effect)` as of `now`,
    /// forgetting hits older than the window.
    pub fn sample(&mut self, now: Instant) -> (u32, u32) {
        while let Some((at, retry_after)) = self.hits.front() {
            let expiry = RATE_LIMIT_WINDOW.max(Duration::from_secs(u64::from(*retry_after)));
            if now.saturating_duration_since(*at) < expiry {
                break;
            }
            self.hits.pop_front();
        }
        let active = self
            .hits
            .iter()
            .map(|(at, retry_after)| {
                u64::from(*retry_after).saturating_sub(now.saturating_duration_since(*at).as_secs())
            })
            .max()
            .unwrap_or(0);
        (self.hits.len() as u32, active as u32)
    }
}

/// Host CPU load in `[0,1]`, measured over the interval between two calls to
/// [`CpuSampler::sample`].
///
/// Linux reads `/proc/stat`, Windows asks `GetSystemTimes`, macOS divides the
/// one-minute load average by the core count. Anywhere else, or when the
/// reading fails, `sample` returns `None` and the cap ignores CPU.
#[derive(Debug, Default)]
pub struct CpuSampler {
    /// `(idle, total)` CPU time at the previous sample.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    prev: Option<(u64, u64)>,
}

impl CpuSampler {
    pub fn sample(&mut self) -> Option<f64> {
        #[cfg(any(target_os = "linux", windows))]
        {
            let now = cpu_times()?;
            let (prev_idle, prev_total) = self.prev.replace(now)?;
            busy_fraction((prev_idle, prev_total), now)
        }
        #[cfg(target_os = "macos")]
        {
            let out = std::process::Command::new("sysctl")
                .args(["-n", "vm.loadavg"])
                .output()
                .ok()?;
            let one_minute: f64 = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .find_map(|w| w.parse().ok())?;
            let cores = std::thread::available_parallelism().ok()?.get() as f64;
            Some((one_minute / cores).clamp(0.0, 1.0))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            None
        }
    }
}

/// Share of CPU time spent busy between two `(idle, total)` readings.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn busy_fraction(before: (u64, u64), after: (u64, u64)) -> Option<f64> {
    let total = after.1.checked_sub(before.1)?;
    let idle = after.0.checked_sub(before.0)?;
    if total == 0 {
        return None;
    }
    Some((1.0 - idle as f64 / total as f64).clamp(0.0, 1.0))
}

/// `(idle, total)` from the aggregate `cpu` line of `/proc/stat`. Idle counts
/// `iowait` too: a core waiting on disk is free to run another worker.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
    Some((idle, fields.iter().sum()))
}

#[cfg(target_os = "linux")]
fn cpu_times() -> Option<(u64, u64)> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(windows)]
fn cpu_times() -> Option<(u64, u64)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut idle, mut kernel, mut user) = (zero(), zero(), zero());
    // SAFETY: three valid, writable FILETIME out-pointers, as the call expects.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    // Kernel time already includes idle time.
    Some((ticks(idle), ticks(kernel) + ticks(user)))
}

/// Signals sampled at a scheduling tick.
#[derive(Debug, Clone)]
pub struct ConcurrencySignals {
    /// Configured hard ceiling (`max_concurrent_agents`).
    pub max_agents: u32,
    /// Floor — never drop below this (keep at least one worker moving).
    pub min_agents: u32,
    /// 429s seen since the last tick.
    pub recent_rate_limit_hits: u32,
    /// Largest `Retry-After` (seconds) currently in effect; 0 = none.
    pub active_retry_after_secs: u32,
    /// Host CPU load in `[0,1]` (1.0 = fully saturated).
    pub cpu_load: f64,
    /// USD spent so far.
    pub usd_spent: f64,
    /// USD cap (0 = uncapped).
    pub max_usd: f64,
}

/// Recommend a concurrency cap for the next wave.
pub fn recommended_concurrency(s: &ConcurrencySignals) -> u32 {
    let min = s.min_agents.max(1);
    let max = s.max_agents.max(min);

    // Hard backoff: if a Retry-After is in effect, collapse to the floor.
    if s.active_retry_after_secs > 0 {
        return min;
    }

    let span = (max - min) as f64;
    let mut factor = 1.0_f64;

    // Rate-limit pressure: each recent 429 shaves 25% off the ceiling.
    if s.recent_rate_limit_hits > 0 {
        factor *= (1.0 - 0.25 * s.recent_rate_limit_hits as f64).max(0.0);
    }

    // CPU load: linearly scale down. load 0 → ×1, load 1 → ×0.
    factor *= (1.0 - s.cpu_load.clamp(0.0, 1.0)).max(0.0);

    // Budget burn: throttle as we approach the cap. burn 0 → ×1,
    // burn ≥0.9 → ×~0.1, burn ≥1.0 → floor.
    if s.max_usd > 0.0 {
        let burn = (s.usd_spent / s.max_usd).clamp(0.0, 1.0);
        if burn >= 1.0 {
            return min;
        }
        factor *= 1.0 - burn;
    }

    let scaled = min as f64 + span * factor.clamp(0.0, 1.0);
    (scaled.round() as u32).clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ConcurrencySignals {
        ConcurrencySignals {
            max_agents: 8,
            min_agents: 1,
            recent_rate_limit_hits: 0,
            active_retry_after_secs: 0,
            cpu_load: 0.0,
            usd_spent: 0.0,
            max_usd: 10.0,
        }
    }

    #[test]
    fn idle_healthy_run_uses_full_ceiling() {
        assert_eq!(recommended_concurrency(&base()), 8);
    }

    #[test]
    fn active_retry_after_collapses_to_floor() {
        let s = ConcurrencySignals {
            active_retry_after_secs: 30,
            ..base()
        };
        assert_eq!(recommended_concurrency(&s), 1);
    }

    #[test]
    fn rate_limit_hits_reduce_cap() {
        let s = ConcurrencySignals {
            recent_rate_limit_hits: 2,
            ..base()
        };
        // factor 1 - 0.5 = 0.5 → 1 + 7*0.5 = 4.5 → 5
        assert_eq!(recommended_concurrency(&s), 5);
    }

    #[test]
    fn high_cpu_load_throttles() {
        let s = ConcurrencySignals {
            cpu_load: 1.0,
            ..base()
        };
        assert_eq!(recommended_concurrency(&s), 1);
    }

    #[test]
    fn budget_exhausted_collapses_to_floor() {
        let s = ConcurrencySignals {
            usd_spent: 10.0,
            max_usd: 10.0,
            ..base()
        };
        assert_eq!(recommended_concurrency(&s), 1);
    }

    #[test]
    fn budget_near_cap_throttles_proportionally() {
        let s = ConcurrencySignals {
            usd_spent: 9.0,
            max_usd: 10.0,
            ..base()
        };
        // burn 0.9 → factor 0.1 → 1 + 7*0.1 = 1.7 → 2
        assert_eq!(recommended_concurrency(&s), 2);
    }

    #[test]
    fn never_below_floor_or_above_ceiling() {
        let s = ConcurrencySignals {
            recent_rate_limit_hits: 100,
            cpu_load: 1.0,
            min_agents: 2,
            ..base()
        };
        let c = recommended_concurrency(&s);
        assert!((2..=8).contains(&c));
    }

    #[test]
    fn min_clamped_to_at_least_one() {
        let s = ConcurrencySignals {
            min_agents: 0,
            max_agents: 0,
            ..base()
        };
        assert_eq!(recommended_concurrency(&s), 1);
    }

    #[test]
    fn rate_limit_window_ages_out_hits_and_retry_after() {
        let t0 = Instant::now();
        let mut w = RateLimitWindow::default();
        assert_eq!(w.sample(t0), (0, 0));
        w.record(t0, None);
        w.record(t0, Some(30));
        // Both hits count; the Retry-After has 20s left.
        assert_eq!(w.sample(t0 + Duration::from_secs(10)), (2, 20));
        // Past the Retry-After but inside the window: pressure, no backoff.
        assert_eq!(w.sample(t0 + Duration::from_secs(45)), (2, 0));
        // Past the window: forgotten.
        assert_eq!(w.sample(t0 + Duration::from_secs(61)), (0, 0));
    }

    #[test]
    fn a_retry_after_longer_than_the_window_is_kept() {
        let t0 = Instant::now();
        let mut w = RateLimitWindow::default();
        w.record(t0, Some(90));
        assert_eq!(w.sample(t0 + Duration::from_secs(70)), (1, 20));
    }

    #[test]
    fn proc_stat_busy_share() {
        let before = parse_proc_stat(
            "cpu  100 0 100 700 100 0 0 0 0 0
cpu0 1 2 3 4
",
        )
        .unwrap();
        assert_eq!(before, (800, 1000));
        let after = parse_proc_stat(
            "cpu  400 0 100 1000 100 0 0 0 0 0
",
        )
        .unwrap();
        // 600 ticks passed, 300 of them idle.
        assert_eq!(busy_fraction(before, after), Some(0.5));
        assert_eq!(busy_fraction(after, after), None);
        assert!(parse_proc_stat("intr 1 2 3").is_none());
    }

    /// Every supported platform yields a load in range once it has two
    /// readings to compare.
    #[test]
    fn the_host_sampler_reads_a_load_in_range() {
        let mut s = CpuSampler::default();
        let first = s.sample();
        std::thread::sleep(Duration::from_millis(250));
        let load = s.sample().or(first);
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            let load = load.expect("a supported platform yields a reading");
            assert!((0.0..=1.0).contains(&load), "{load}");
        }
    }

    #[test]
    fn signals_compound() {
        // Moderate load + one 429 + half budget burned.
        let s = ConcurrencySignals {
            recent_rate_limit_hits: 1, // ×0.75
            cpu_load: 0.5,             // ×0.5
            usd_spent: 5.0,
            max_usd: 10.0, // ×0.5
            ..base()
        };
        // factor = 0.75*0.5*0.5 = 0.1875 → 1 + 7*0.1875 = 2.31 → 2
        assert_eq!(recommended_concurrency(&s), 2);
    }
}

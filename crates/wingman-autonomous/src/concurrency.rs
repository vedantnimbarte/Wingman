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
//! Pure function: the orchestrator samples the signals each scheduling
//! tick and calls [`recommended_concurrency`].

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

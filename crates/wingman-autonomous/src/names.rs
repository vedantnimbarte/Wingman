//! Docker-style friendly names for agents.
//!
//! Agents keep their stable `agent-000N` id (session filenames, event keys,
//! `session fork` all key off it). On top of that we derive a human-friendly
//! `adjective_animal` display name — e.g. `brave_otter`, `lucid_lynx` — so the
//! dashboard reads like a crew roster instead of a sequence of counters.
//!
//! The name is a **pure, deterministic** function of `(run_id, agent_id)`:
//! replaying the same run always yields the same names, and tests can assert
//! exact values. Within a single run the mapping is collision-free for any
//! realistic agent count (see [`agent_name`]); [`crate::model::apply`] adds a
//! numeric suffix as a belt-and-suspenders guard for the impossible case.

/// 32 friendly adjectives. Keep the count a power of two — combined with
/// [`ANIMALS`] it makes the name space `32 * 32 = 1024`, which the
/// [`agent_name`] permutation relies on for collision-free naming.
const ADJECTIVES: [&str; 32] = [
    "amber", "brave", "bright", "calm", "clever", "cosmic", "crimson", "daring", "eager", "fuzzy",
    "gentle", "jolly", "keen", "lively", "lucid", "mellow", "nimble", "noble", "placid", "quiet",
    "rapid", "rusty", "sage", "silent", "sleek", "snowy", "stoic", "sunny", "swift", "vivid",
    "witty", "zesty",
];

/// 32 animals. See [`ADJECTIVES`] for why the count is a power of two.
const ANIMALS: [&str; 32] = [
    "badger", "bison", "cobra", "crane", "dingo", "otter", "falcon", "ferret", "gecko", "heron",
    "ibex", "jackal", "koala", "lemur", "lynx", "marten", "narwhal", "ocelot", "panda", "quokka",
    "raven", "seal", "shrew", "stoat", "tapir", "urchin", "viper", "walrus", "weasel", "wombat",
    "yak", "zebra",
];

/// Odd multiplier used to scatter consecutive sequence numbers across the
/// name space. Being odd makes it coprime with the power-of-two space size,
/// so `n -> (n * STRIDE) mod 1024` is a bijection — distinct agents in a run
/// get distinct names without any collision bookkeeping.
const STRIDE: u64 = 0x9E37_79B1;

/// FNV-1a 64-bit hash — small, dependency-free, good enough for seeding.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Sequence number encoded in an `agent-000N` id, if present.
fn seq_of(agent_id: &str) -> Option<u64> {
    agent_id.strip_prefix("agent-")?.parse().ok()
}

/// The friendly `adjective_animal` name for `agent_id` within `run_id`.
///
/// Deterministic and, for `agent-000N` ids with `N < 1024`, unique within a
/// run. Non-`agent-000N` ids fall back to hashing the id itself.
pub fn agent_name(run_id: &str, agent_id: &str) -> String {
    let n = seq_of(agent_id).unwrap_or_else(|| fnv1a(agent_id.as_bytes()));
    let seed = fnv1a(run_id.as_bytes());
    let total = (ADJECTIVES.len() * ANIMALS.len()) as u64;
    let idx = seed.wrapping_add(n.wrapping_mul(STRIDE)) % total;
    let adj = ADJECTIVES[(idx / ANIMALS.len() as u64) as usize];
    let animal = ANIMALS[(idx % ANIMALS.len() as u64) as usize];
    format!("{adj}_{animal}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn deterministic_for_same_inputs() {
        assert_eq!(
            agent_name("2026-07-01-0707-hq27zr", "agent-0001"),
            agent_name("2026-07-01-0707-hq27zr", "agent-0001"),
        );
    }

    #[test]
    fn differs_across_runs() {
        // Same agent seq, different run → (almost surely) different name.
        assert_ne!(
            agent_name("run-a", "agent-0001"),
            agent_name("run-b", "agent-0001"),
        );
    }

    #[test]
    fn shape_is_adjective_underscore_animal() {
        let name = agent_name("run-a", "agent-0003");
        let (adj, animal) = name.split_once('_').expect("underscore separator");
        assert!(ADJECTIVES.contains(&adj), "unknown adjective: {adj}");
        assert!(ANIMALS.contains(&animal), "unknown animal: {animal}");
    }

    #[test]
    fn collision_free_across_a_full_run() {
        // Every agent-000N for N in [0, 1024) must be unique within one run.
        // This locks the power-of-two / odd-stride invariant: if someone
        // resizes the word lists and breaks it, this test fails loudly.
        let mut seen = HashSet::new();
        for n in 0..(ADJECTIVES.len() * ANIMALS.len()) {
            let id = format!("agent-{n:04}");
            assert!(
                seen.insert(agent_name("2026-07-01-0707-hq27zr", &id)),
                "duplicate name at n={n}"
            );
        }
    }
}

//! Measure what the `dyn` dispatch in the A2 seams actually costs.
//!
//! Decision record 0014 recorded the cost as *unmeasured* rather than
//! assuming it away. This is the measurement.
//!
//!     cargo run --release --example dispatch_cost -p wingman-tools
//!
//! Release only. Debug numbers say nothing about shipped code.
//!
//! "Is dynamic dispatch slow" is the wrong question; the right one is "how
//! does it compare to the work it wraps, and to the noise in measuring that
//! work". So each I/O comparison also measures a **noise floor**: the same
//! implementation timed twice. A difference smaller than that floor is not a
//! result, and is reported as such rather than as a number.
//!
//! `black_box` guards both arms. Without it the compiler devirtualizes and
//! then deletes the static arm, and the comparison measures nothing.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use wingman_tools::filesystem::{FileSystem, OsFileSystem};

/// A method with no body, to isolate the dispatch itself.
trait Trivial {
    fn value(&self, x: u64) -> u64;
}

struct Concrete;
impl Trivial for Concrete {
    // Without this the static arm inlines away and the comparison is against
    // nothing at all.
    #[inline(never)]
    fn value(&self, x: u64) -> u64 {
        x.wrapping_add(1)
    }
}

/// A trivial async method, to isolate what `#[async_trait]` adds beyond a
/// vtable lookup: it returns a boxed future, so every call heap-allocates.
#[async_trait::async_trait]
trait TrivialAsync {
    async fn value(&self, x: u64) -> u64;
}

struct TrivialAsyncImpl;

#[async_trait::async_trait]
impl TrivialAsync for TrivialAsyncImpl {
    async fn value(&self, x: u64) -> u64 {
        x.wrapping_add(1)
    }
}

/// The same work without the trait, so the delta is the boxing.
#[inline(never)]
async fn plain_async(x: u64) -> u64 {
    x.wrapping_add(1)
}

/// Best of several trials.
///
/// Mean and median both absorb scheduler noise on a developer machine; the
/// minimum is the closest available estimate of the cost with interference
/// removed.
fn best_of(trials: usize, mut f: impl FnMut() -> Duration) -> Duration {
    (0..trials).map(|_| f()).min().expect("at least one trial")
}

fn ns_per_op(d: Duration, ops: u64) -> f64 {
    d.as_secs_f64() * 1e9 / ops as f64
}

/// Report a comparison against the noise floor, so a delta that is really
/// measurement jitter is not presented as a finding.
fn report(label: &str, direct_ns: f64, seam_ns: f64, noise_ns: f64) {
    let delta = seam_ns - direct_ns;
    println!("   direct           : {direct_ns:>10.1} ns/op");
    println!("   through the seam : {seam_ns:>10.1} ns/op");
    println!("   difference       : {delta:>10.1} ns/op");
    println!("   noise floor      : {noise_ns:>10.1} ns/op  (same impl, timed twice)");
    if delta.abs() <= noise_ns.abs().max(1.0) {
        println!("   -> {label}: dispatch cost is BELOW the noise floor; not measurable here.");
    } else {
        println!(
            "   -> {label}: {:+.4}% versus direct.",
            delta / direct_ns * 100.0
        );
    }
    println!();
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("WARNING: built without --release; these numbers are meaningless.\n");
    }

    // ---- 1. The dispatch floor -------------------------------------------
    // An empty method, so there is no work for the vtable lookup to hide
    // behind. This is the largest the overhead can ever look.
    const CALLS: u64 = 50_000_000;
    let concrete = Concrete;
    let dynamic: &dyn Trivial = &concrete;
    let _ = black_box(concrete.value(black_box(1)));

    let static_time = best_of(5, || {
        let start = Instant::now();
        let mut acc = 0u64;
        for i in 0..CALLS {
            acc = acc.wrapping_add(concrete.value(black_box(i)));
        }
        black_box(acc);
        start.elapsed()
    });
    let dyn_time = best_of(5, || {
        let start = Instant::now();
        let mut acc = 0u64;
        for i in 0..CALLS {
            acc = acc.wrapping_add(dynamic.value(black_box(i)));
        }
        black_box(acc);
        start.elapsed()
    });

    let static_ns = ns_per_op(static_time, CALLS);
    let dyn_ns = ns_per_op(dyn_time, CALLS);
    println!("1. dispatch floor — empty method, {CALLS} calls");
    println!("   static           : {static_ns:>10.3} ns/call");
    println!("   dyn              : {dyn_ns:>10.3} ns/call");
    println!("   difference       : {:>10.3} ns/call\n", dyn_ns - static_ns);

    // ---- shared fixture ---------------------------------------------------
    let dir = std::env::temp_dir().join("wingman-dispatch-cost");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("sample.rs");
    // ~8 KiB: an ordinary source file, which is what the tree-walking tools
    // actually read.
    let body = "fn main() { println!(\"x\"); }\n".repeat(280);
    std::fs::write(&file, &body).expect("write sample");
    let path: &Path = &file;

    let fs = OsFileSystem;
    let fs_dyn: &dyn FileSystem = &fs;

    // ---- 2. Blocking read, warm cache ------------------------------------
    // What grep / find_symbol / who_calls do thousands of times per call.
    // Warm cache is the *worst* case for relative overhead: the faster the
    // I/O, the more any dispatch cost would show.
    const READS: u64 = 20_000;
    let _ = black_box(std::fs::read(path).unwrap());

    let time_direct = || {
        let start = Instant::now();
        for _ in 0..READS {
            black_box(std::fs::read(black_box(path)).unwrap());
        }
        start.elapsed()
    };
    let direct_a = best_of(3, time_direct);
    let direct_b = best_of(3, time_direct); // noise floor
    let seam = best_of(3, || {
        let start = Instant::now();
        for _ in 0..READS {
            black_box(fs_dyn.read_blocking(black_box(path)).unwrap());
        }
        start.elapsed()
    });

    let read_ns = ns_per_op(direct_a, READS);
    println!("2. blocking read, 8 KiB, warm cache — {READS} reads");
    report(
        "read_blocking",
        read_ns,
        ns_per_op(seam, READS),
        (ns_per_op(direct_b, READS) - ns_per_op(direct_a, READS)).abs(),
    );

    // ---- 3. Async read, warm cache ---------------------------------------
    const AREADS: u64 = 5_000;
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let _ = black_box(tokio::fs::read(path).await.unwrap());

        // Timed inline: a nested `block_on` inside a runtime panics.
        async fn time_direct(path: &Path, n: u64) -> Duration {
            let start = Instant::now();
            for _ in 0..n {
                black_box(tokio::fs::read(black_box(path)).await.unwrap());
            }
            start.elapsed()
        }
        async fn time_seam(fs: &dyn FileSystem, path: &Path, n: u64) -> Duration {
            let start = Instant::now();
            for _ in 0..n {
                black_box(fs.read(black_box(path)).await.unwrap());
            }
            start.elapsed()
        }

        // More trials than the blocking case: tokio routes each read through
        // its blocking pool, and that handoff varies far more run-to-run than
        // the read does. Min-of-N converges downward, so a bigger N tightens
        // both the arms and the floor; with N=3 this section reported
        // anything from "below the noise floor" to "+30%" on identical code.
        const ATRIALS: usize = 9;
        let mut direct_a = Duration::MAX;
        let mut direct_b = Duration::MAX;
        let mut seam = Duration::MAX;
        for _ in 0..ATRIALS {
            direct_a = direct_a.min(time_direct(path, AREADS).await);
        }
        for _ in 0..ATRIALS {
            direct_b = direct_b.min(time_direct(path, AREADS).await);
        }
        for _ in 0..ATRIALS {
            seam = seam.min(time_seam(fs_dyn, path, AREADS).await);
        }

        println!("3. async read, same file, warm cache — {AREADS} reads");
        report(
            "async read",
            ns_per_op(direct_a, AREADS),
            ns_per_op(seam, AREADS),
            (ns_per_op(direct_b, AREADS) - ns_per_op(direct_a, AREADS)).abs(),
        );
    });

    // ---- 4. The async-dispatch floor -------------------------------------
    // Worth isolating, because `#[async_trait]` does more than a vtable
    // lookup: it returns `Pin<Box<dyn Future>>`, so every call heap-allocates.
    // That is a real mechanism rather than noise, and with no I/O to hide
    // behind it is the one part of this that does measure.
    const ACALLS: u64 = 2_000_000;
    rt.block_on(async {
        let t = TrivialAsyncImpl;
        let t_dyn: &dyn TrivialAsync = &t;

        let mut plain = Duration::MAX;
        let mut boxed = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            let mut acc = 0u64;
            for i in 0..ACALLS {
                acc = acc.wrapping_add(plain_async(black_box(i)).await);
            }
            black_box(acc);
            plain = plain.min(start.elapsed());
        }
        for _ in 0..5 {
            let start = Instant::now();
            let mut acc = 0u64;
            for i in 0..ACALLS {
                acc = acc.wrapping_add(t_dyn.value(black_box(i)).await);
            }
            black_box(acc);
            boxed = boxed.min(start.elapsed());
        }

        let plain_ns = ns_per_op(plain, ACALLS);
        let boxed_ns = ns_per_op(boxed, ACALLS);
        println!("4. async dispatch floor — trivial method, {ACALLS} calls");
        println!("   plain async fn   : {plain_ns:>10.3} ns/call");
        println!("   #[async_trait]   : {boxed_ns:>10.3} ns/call  (boxes the future)");
        println!("   difference       : {:>10.3} ns/call", boxed_ns - plain_ns);
        println!();
        // Scale it against the read measured above rather than a constant, so
        // the ratio stays honest on a machine whose I/O is faster or slower.
        println!("   For scale: the 8 KiB warm read above cost {read_ns:.0} ns, so that one");
        println!(
            "   allocation is about {:.4}% of a single file read.",
            (boxed_ns - plain_ns) / read_ns * 100.0
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

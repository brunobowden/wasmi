//! Regression test for the physical memory committed by linear memories.
//!
//! Pages of a linear memory that are never accessed should not consume physical memory.
//! Each case below runs a controlled workload, measures how much resident memory the
//! process gains and how long the workload takes, reports all measurements, and only then
//! checks them against the budgets defined below.
//!
//! # Cases
//!
//! | Case        | Workload                                                                  |
//! |-------------|---------------------------------------------------------------------------|
//! | `create`    | 8 memories created at 64 MiB; one byte written in each                    |
//! | `grow`      | 8 memories created with 1 page, grown to 64 MiB; first and last byte written |
//! | `instances` | 100 instances of a 17-page module (a Rust guest's default 1 MiB stack plus data); each touches its stack top and data, then grows by 16 pages and writes one byte there |
//! | `control`   | 1 memory of 64 MiB with every byte written                                |
//!
//! # Design
//!
//! - **Measure, then judge.** All cases are measured and reported before any budget is
//!   checked, so a failure always shows every number. Run with
//!   `cargo test --release --test memory_footprint -- --nocapture` to see the report.
//!   Set `WASMI_MEMORY_REPORT=<path>` to append the measurements as CSV, and
//!   `WASMI_MEMORY_REPORT_LABEL=<label>` to tag them (e.g. with a branch name).
//! - **Tight memory budgets.** Each case has a budget per memory or instance, sized to what
//!   the workload must commit (the pages it writes plus bookkeeping) with roughly 3x
//!   headroom over the measured value on Linux, plus 1 MiB of process noise per case.
//!   A regression that commits untouched pages exceeds a budget by two to three orders of
//!   magnitude. The budgets hold for 4 KiB (Linux) and 16 KiB (macOS/arm64) pages.
//! - **Timing is reported, never asserted.** Wall time depends on the hardware, so it is
//!   only reported. Compare timings on the same machine (see `WASMI_MEMORY_REPORT`).
//! - **Validated measurement.** The `control` case must register at least 90% of what it
//!   writes, so an unreliable measurement fails instead of passing vacuously.
//! - **Isolated process.** This is its own test binary with a single test, so no other test
//!   runs in the same process while resident memory is measured.
//! - **Platforms.** Resident memory is read from `/proc/self/status` on Linux and via `ps`
//!   on macOS. Other platforms, 32-bit hosts and Miri are skipped. On Linux hosts with
//!   transparent hugepages set to `always`, a single write commits a whole 2 MiB page, so
//!   the budgets are reported but not checked there.
//! - **Sizes.** The 64 MiB memories are above glibc's maximum dynamic mmap threshold
//!   (32 MiB), so they are always served with fresh, lazily zero-filled pages.
#![cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64",
    not(miri)
))]

use std::time::{Duration, Instant};
use wasmi::{Engine, Linker, Memory, MemoryType, Module, Store};

/// Returns `true` if transparent hugepages apply to all anonymous mappings, in which case a
/// single write can commit a whole 2 MiB huge page.
fn transparent_hugepages_always() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .map(|setting| setting.contains("[always]"))
        .unwrap_or(false)
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// The size of a Wasm page in bytes.
const WASM_PAGE: u64 = 64 * KIB;

/// The number of Wasm pages of the memories of the `create`, `grow` and `control` cases: 64 MiB.
const BIG_PAGES: u32 = 1024;
/// The number of memories of the `create` and `grow` cases.
const BIG_MEMORIES: u64 = 8;
/// The number of instances of the `instances` case.
const INSTANCES: u64 = 100;

/// Resident memory budget per memory of the `create` case (measured: ~5 KiB on Linux).
const BUDGET_CREATE: u64 = 64 * KIB;
/// Resident memory budget per memory of the `grow` case (measured: ~20 KiB on Linux).
const BUDGET_GROW: u64 = 128 * KIB;
/// Resident memory budget per instance of the `instances` case (measured: ~175 KiB on Linux).
const BUDGET_INSTANCE: u64 = 512 * KIB;
/// Process noise allowance added to every budget.
const NOISE: u64 = MIB;
/// Minimum resident growth, relative to the declared size, for the fully written `control` case.
const MIN_CONTROL_RATIO: f64 = 0.9;

/// A 17-page module like a Rust guest with the default 1 MiB shadow stack.
const GUEST: &str = r#"
(module
  (memory (export "memory") 17)
  ;; 64 bytes of data just above the 1 MiB shadow stack, like rustc's statics.
  (data (i32.const 1048576) "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
  ;; Writes the top 8 KiB of the stack (two 4 KiB pages) and the data region.
  (func (export "touch")
    (local $p i32)
    (local.set $p (i32.const 1040384))
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $p) (i32.const 1048576)))
        (i32.store8 (local.get $p) (i32.const 1))
        (local.set $p (i32.add (local.get $p) (i32.const 4096)))
        (br $next)))
    (i32.store8 (i32.const 1048640) (i32.const 1)))
  ;; Grows by 16 pages and writes one byte in the new range.
  (func (export "grow") (result i32)
    (local $old i32)
    (local.set $old (memory.grow (i32.const 16)))
    (i32.store8 (i32.const 1200000) (i32.const 1))
    (local.get $old)))
"#;

/// The measurement of a single case.
struct Measurement {
    /// The name of the case.
    case: &'static str,
    /// The number of memories or instances of the case.
    units: u64,
    /// The total size of all linear memories of the case in bytes.
    declared: u64,
    /// The growth of resident memory during the case in bytes.
    resident_growth: u64,
    /// The wall time of the workload.
    elapsed: Duration,
}

impl Measurement {
    /// Returns the resident growth relative to the declared size.
    fn ratio(&self) -> f64 {
        self.resident_growth as f64 / self.declared as f64
    }

    /// Asserts that the resident growth is within `budget_per_unit * units + NOISE`.
    fn assert_budget(&self, budget_per_unit: u64) {
        let budget = budget_per_unit * self.units + NOISE;
        assert!(
            self.resident_growth <= budget,
            "{}: resident memory grew by {} KiB ({} KiB per unit, {:.2}% of {} MiB declared), \
            exceeding the budget of {} KiB ({} KiB per unit + {} KiB noise): \
            untouched pages appear to be committed",
            self.case,
            self.resident_growth / KIB,
            self.resident_growth / self.units / KIB,
            self.ratio() * 100.0,
            self.declared / MIB,
            budget / KIB,
            budget_per_unit / KIB,
            NOISE / KIB,
        );
    }
}

/// Returns the resident memory of this process in bytes.
fn resident_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    let kib = {
        let status =
            std::fs::read_to_string("/proc/self/status").expect("failed to read /proc/self/status");
        let line = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .expect("missing VmRSS in /proc/self/status");
        line.split_whitespace()
            .nth(1)
            .and_then(|kib| kib.parse::<u64>().ok())
            .expect("invalid VmRSS line")
    };
    #[cfg(target_os = "macos")]
    let kib = {
        let pid = std::process::id().to_string();
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .expect("failed to run ps");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .expect("invalid ps output")
    };
    kib * KIB
}

/// Runs `workload` and measures resident growth and wall time.
///
/// The workload returns whatever it created and the total declared size of its memories.
/// What it created stays alive until all cases are measured, so its pages cannot be reused
/// by later cases.
fn measure<T: 'static>(
    case: &'static str,
    units: u64,
    keep: &mut Vec<Box<dyn std::any::Any>>,
    workload: impl FnOnce() -> (T, u64),
) -> Measurement {
    let before = resident_bytes();
    let start = Instant::now();
    let (created, declared) = workload();
    let elapsed = start.elapsed();
    let after = resident_bytes();
    keep.push(Box::new(created));
    Measurement {
        case,
        units,
        declared,
        resident_growth: after.saturating_sub(before),
        elapsed,
    }
}

/// Prints the measurements and optionally appends them as CSV to `WASMI_MEMORY_REPORT`.
fn report(measurements: &[Measurement]) {
    eprintln!(
        "{:<10} {:>6} {:>13} {:>14} {:>14} {:>9} {:>12}",
        "case", "units", "declared MiB", "resident KiB", "KiB per unit", "ratio", "µs per unit"
    );
    for m in measurements {
        eprintln!(
            "{:<10} {:>6} {:>13} {:>14} {:>14} {:>8.3}% {:>12.1}",
            m.case,
            m.units,
            m.declared / MIB,
            m.resident_growth / KIB,
            m.resident_growth / m.units / KIB,
            m.ratio() * 100.0,
            m.elapsed.as_secs_f64() * 1e6 / m.units as f64,
        );
    }
    if let Some(path) = std::env::var_os("WASMI_MEMORY_REPORT") {
        use std::io::Write as _;
        let label = std::env::var("WASMI_MEMORY_REPORT_LABEL").unwrap_or_default();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("failed to open WASMI_MEMORY_REPORT");
        for m in measurements {
            writeln!(
                file,
                "{label},{},{},{},{},{},{},{}",
                std::env::consts::OS,
                std::env::consts::ARCH,
                m.case,
                m.units,
                m.declared,
                m.resident_growth,
                m.elapsed.as_nanos(),
            )
            .expect("failed to write WASMI_MEMORY_REPORT");
        }
    }
}

#[test]
fn untouched_linear_memory_is_not_committed() {
    let engine = Engine::default();
    let mut keep = Vec::new();

    let create = measure("create", BIG_MEMORIES, &mut keep, || {
        let mut store = Store::new(&engine, ());
        for _ in 0..BIG_MEMORIES {
            let memory = Memory::new(&mut store, MemoryType::new(BIG_PAGES, None)).unwrap();
            memory.data_mut(&mut store)[0] = 1;
        }
        (store, BIG_MEMORIES * u64::from(BIG_PAGES) * WASM_PAGE)
    });

    let grow = measure("grow", BIG_MEMORIES, &mut keep, || {
        let mut store = Store::new(&engine, ());
        for _ in 0..BIG_MEMORIES {
            let memory = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
            memory.data_mut(&mut store)[0] = 1;
            memory
                .grow(&mut store, u64::from(BIG_PAGES - 1))
                .expect("failed to grow memory");
            let data = memory.data_mut(&mut store);
            assert_eq!(data[0], 1, "growth must preserve the contents");
            let last = data.len() - 1;
            data[last] = 1;
        }
        (store, BIG_MEMORIES * u64::from(BIG_PAGES) * WASM_PAGE)
    });

    let module = Module::new(&engine, GUEST).expect("invalid guest module");
    let linker = Linker::<()>::new(&engine);
    let instances = measure("instances", INSTANCES, &mut keep, || {
        let mut stores = Vec::new();
        let mut declared = 0;
        for _ in 0..INSTANCES {
            let mut store = Store::new(&engine, ());
            let instance = linker
                .instantiate_and_start(&mut store, &module)
                .expect("failed to instantiate guest");
            let touch = instance.get_typed_func::<(), ()>(&store, "touch").unwrap();
            touch.call(&mut store, ()).unwrap();
            let grow = instance.get_typed_func::<(), i32>(&store, "grow").unwrap();
            assert_eq!(grow.call(&mut store, ()).unwrap(), 17, "memory.grow failed");
            let memory = instance.get_memory(&store, "memory").unwrap();
            declared += memory.data(&store).len() as u64;
            stores.push(store);
        }
        (stores, declared)
    });

    let control = measure("control", 1, &mut keep, || {
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(BIG_PAGES, None)).unwrap();
        for (i, byte) in memory.data_mut(&mut store).iter_mut().enumerate() {
            // A non-uniform pattern so that the pages are not trivially compressible.
            *byte = (i % 251) as u8 + 1;
        }
        (store, u64::from(BIG_PAGES) * WASM_PAGE)
    });

    let measurements = [create, grow, instances, control];
    report(&measurements);

    let [create, grow, instances, control] = &measurements;
    assert!(
        control.ratio() >= MIN_CONTROL_RATIO,
        "invalid measurement: writing all {} MiB of a memory grew resident memory by only {} KiB \
        (expected at least {:.0}%), so resident memory cannot be measured reliably here",
        control.declared / MIB,
        control.resident_growth / KIB,
        MIN_CONTROL_RATIO * 100.0,
    );
    if transparent_hugepages_always() {
        eprintln!(
            "budgets not checked: transparent hugepages are set to `always`, \
            so a single write commits a whole 2 MiB page"
        );
        return;
    }
    create.assert_budget(BUDGET_CREATE);
    grow.assert_budget(BUDGET_GROW);
    instances.assert_budget(BUDGET_INSTANCE);
}

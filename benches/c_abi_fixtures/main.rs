//! R1: criterion over the parity fixtures through the `orbital_kernel_*` C ABI, plus the JSON row
//! `@almadar-io/kernel-bench-tool`'s `bench-report` merges. R1 minus R0 is the cost of the C-string and JSON boundary
//! (and of the client role: a server leg is returned in the wire, not run).
//!
//! `ALMADAR_PARITY_FIXTURES` names the fixtures folder (required). With `ALMADAR_BENCH_OUT` set the
//! bench writes the JSON row there (`ALMADAR_PARITY_REFERENCE` gates it against a bench result file,
//! `ALMADAR_PARITY_REFERENCE_LABEL` names it; `ALMADAR_BENCH_NOTE` is the machine note); without it,
//! criterion runs.

mod c_abi_session;
mod scenario_bench;

use c_abi_session::CAbiDriver;
use criterion::{BatchSize, Criterion};
use scenario_bench::*;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct CountingAlloc;
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("ALMADAR_PARITY_FIXTURES")
            .expect("ALMADAR_PARITY_FIXTURES must name the parity fixtures folder"),
    )
}

fn write_json(out: &str) {
    let fixtures = load_fixtures(&fixtures_dir()).expect("fixtures");
    let references = std::env::var("ALMADAR_PARITY_REFERENCE")
        .ok()
        .map(|p| reference_traces(std::path::Path::new(&p)).expect("reference"));
    let sha = command_output(
        "git",
        &["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"],
    );
    let reference_label = std::env::var("ALMADAR_PARITY_REFERENCE_LABEL").unwrap_or_else(|_| {
        if references.is_some() {
            "reference file"
        } else {
            "self (ungated)"
        }
        .to_string()
    });
    let sampler = || (LIVE_BYTES.load(Ordering::Relaxed), rss_bytes());
    let options = BenchOptions {
        driver: &CAbiDriver,
        row: "R1",
        row_name: "orbital-ffi C ABI (client-role EmbeddedKernel, JSON boundary)",
        reference_label: &reference_label,
        references: references.as_ref(),
        commits: json!({ "orbital-ffi": { "sha": sha } }),
        environment: machine_environment(),
        warmups: WARMUP_RUNS,
        measured: MEASURED_RUNS,
        memory_events: MEMORY_EVENTS,
        sampler: &sampler,
    };
    let result = run_bench(&options, &fixtures);
    std::fs::write(out, serde_json::to_string_pretty(&result).unwrap()).expect("write bench json");
}

fn criterion_run() {
    let fixtures = load_fixtures(&fixtures_dir()).expect("fixtures");
    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
        .sample_size(30)
        .configure_from_args();
    let mut group = c.benchmark_group("r1_scenario_steps");
    for fixture in &fixtures {
        for (index, scenario) in fixture.scenarios.iter().enumerate() {
            group.bench_function(scenario_key(fixture, index), |b| {
                b.iter_batched(
                    || CAbiDriver.open(fixture, scenario).expect("open"),
                    |session| {
                        for step in &scenario.steps {
                            session.dispatch(step).expect("dispatch");
                        }
                    },
                    BatchSize::SmallInput,
                )
            });
        }
    }
    group.finish();
    c.final_summary();
}

fn main() {
    match std::env::var("ALMADAR_BENCH_OUT") {
        Ok(out) => write_json(&out),
        Err(_) => criterion_run(),
    }
}

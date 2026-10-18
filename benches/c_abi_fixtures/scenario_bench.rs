//! The embedded-kernel benchmark's row-independent half: the parity fixtures (`tools/parity-cascade/fixtures`,
//! read from the folder `ALMADAR_PARITY_FIXTURES` names) and a mirror of `@almadar-io/kernel-bench-tool`'s
//! `scenario/bench.ts`, so the JSON a row writes merges into the same report: same trace shape per step
//! (`trait`, `event`, `states`, `effects`, `emits`), same statistics, same workload keys
//! (`<fixture>#<index>`). A row supplies a [`Driver`]. This file is the same in `orbital-core/benches`
//! (R0) and `orbital-ffi/benches` (R1): each crate must build from a standalone clone, so neither can
//! reach into the other's folder.

use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

pub const WARMUP_RUNS: usize = 5;
pub const MEASURED_RUNS: usize = 50;
pub const MEMORY_EVENTS: usize = 10_000;
const CLOCK_MARKER: &str = "<clock>";

pub struct Step {
    pub trait_name: String,
    pub event: String,
    pub payload: Option<Json>,
}

pub struct Scenario {
    pub name: Option<String>,
    pub orbital: String,
    pub steps: Vec<Step>,
}

pub struct Fixture {
    pub name: String,
    pub schema: Json,
    pub clock_fields: Vec<String>,
    pub scenarios: Vec<Scenario>,
}

fn string_field(raw: &Json, key: &str, where_: &str) -> Result<String, String> {
    raw[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{where_}: \"{key}\" must be a string"))
}

fn to_step(raw: &Json, where_: &str) -> Result<Step, String> {
    Ok(Step {
        trait_name: string_field(raw, "trait", where_)?,
        event: string_field(raw, "event", where_)?,
        payload: raw.get("payload").cloned(),
    })
}

fn to_scenario(raw: &Json, where_: &str) -> Result<Scenario, String> {
    let steps = match raw.get("steps").and_then(Json::as_array) {
        Some(steps) => steps
            .iter()
            .enumerate()
            .map(|(i, s)| to_step(s, &format!("{where_}.steps[{i}]")))
            .collect::<Result<Vec<_>, _>>()?,
        None => vec![to_step(raw, where_)?],
    };
    Ok(Scenario {
        name: raw.get("name").and_then(Json::as_str).map(str::to_string),
        orbital: string_field(raw, "orbital", where_)?,
        steps,
    })
}

/// Every `*.json` fixture in exactly the folder given, sorted by file name.
pub fn load_fixtures(dir: &Path) -> Result<Vec<Fixture>, String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            let file = path.display().to_string();
            let raw: Json = serde_json::from_str(
                &std::fs::read_to_string(path).map_err(|e| format!("read {file}: {e}"))?,
            )
            .map_err(|e| format!("parse {file}: {e}"))?;
            let scenarios = raw["scenarios"]
                .as_array()
                .ok_or_else(|| format!("{file}: \"scenarios\" must be an array"))?
                .iter()
                .enumerate()
                .map(|(i, s)| to_scenario(s, &format!("{file}: scenarios[{i}]")))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Fixture {
                name: string_field(&raw, "name", &file)?,
                schema: raw["schema"].clone(),
                clock_fields: raw["clockFields"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Json::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                scenarios,
            })
        })
        .collect()
}

pub fn scenario_key(fixture: &Fixture, index: usize) -> String {
    format!("{}#{index}", fixture.name)
}

/// One open scenario on a row's implementation; a dispatch returns the trace step.
pub trait Session {
    fn dispatch(&self, step: &Step) -> Result<Json, String>;
}

/// A row's implementation: how a scenario is opened on it.
pub trait Driver {
    type Open: Session;
    fn open(&self, fixture: &Fixture, scenario: &Scenario) -> Result<Self::Open, String>;
}

/// The steps of one scenario through a fresh session: `{ "steps": [...] }`.
#[allow(dead_code)]
pub fn run_scenario<D: Driver>(
    driver: &D,
    fixture: &Fixture,
    scenario: &Scenario,
) -> Result<Json, String> {
    let session = driver.open(fixture, scenario)?;
    let steps = scenario
        .steps
        .iter()
        .map(|s| session.dispatch(s))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "steps": steps }))
}

#[derive(Debug, PartialEq)]
pub struct TraceDifference {
    pub step_index: usize,
    pub field: String,
    pub expected: Json,
    pub actual: Json,
}

fn canonical(value: &Json) -> Json {
    match value {
        Json::Array(a) => Json::Array(a.iter().map(canonical).collect()),
        Json::Object(o) => {
            let sorted: BTreeMap<_, _> = o.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            Json::Object(sorted.into_iter().collect())
        }
        Json::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 && !n.is_i64() && !n.is_u64() => {
                json!(f as i64)
            }
            _ => value.clone(),
        },
        other => other.clone(),
    }
}

fn with_clock_fields(emits: &Json, clock_fields: &[String]) -> Json {
    let list = emits.as_array().cloned().unwrap_or_default();
    Json::Array(
        list.into_iter()
            .map(|emit| {
                let event = emit["event"].as_str().unwrap_or_default().to_string();
                let prefix = format!("{event}.");
                let fields: Vec<&str> = clock_fields
                    .iter()
                    .filter_map(|f| f.strip_prefix(&prefix))
                    .collect();
                let mut out = emit.clone();
                if let Some(payload) = out.get_mut("payload").and_then(Json::as_object_mut) {
                    for f in fields {
                        if payload.get(f).is_some_and(Json::is_number) {
                            payload.insert(f.to_string(), json!(CLOCK_MARKER));
                        }
                    }
                }
                out
            })
            .collect(),
    )
}

/// First departure of `actual` from `expected`, in the order `trace.ts` checks it.
pub fn compare_traces(
    clock_fields: &[String],
    expected: &Json,
    actual: &Json,
) -> Option<TraceDifference> {
    let (want, got) = (expected["steps"].as_array()?, actual["steps"].as_array()?);
    let same = |a: &Json, b: &Json| canonical(a) == canonical(b);
    for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
        for field in ["trait", "event", "states", "effects"] {
            if !same(&w[field], &g[field]) {
                return Some(TraceDifference {
                    step_index: i,
                    field: field.to_string(),
                    expected: w[field].clone(),
                    actual: g[field].clone(),
                });
            }
        }
        let (we, ge) = (
            with_clock_fields(&w["emits"], clock_fields),
            with_clock_fields(&g["emits"], clock_fields),
        );
        if !same(&we, &ge) {
            return Some(TraceDifference {
                step_index: i,
                field: "emits".into(),
                expected: we,
                actual: ge,
            });
        }
    }
    (want.len() != got.len()).then(|| TraceDifference {
        step_index: want.len().min(got.len()),
        field: "length".into(),
        expected: json!(want.len()),
        actual: json!(got.len()),
    })
}

/// The traces a `@almadar-io/kernel-bench-tool` bench result file recorded, by workload key.
pub fn reference_traces(path: &Path) -> Result<HashMap<String, Json>, String> {
    let result: Json = serde_json::from_str(
        &std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(result["workloads"]
        .as_array()
        .ok_or("reference has no workloads")?
        .iter()
        .filter(|w| !w["trace"].is_null())
        .map(|w| {
            (
                w["key"].as_str().unwrap_or_default().to_string(),
                w["trace"].clone(),
            )
        })
        .collect())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    percentile(&v, 50.0)
}

fn latency_stats(per_run: &[Vec<f64>]) -> Json {
    let mut pooled: Vec<f64> = per_run.iter().flatten().copied().collect();
    pooled.sort_by(f64::total_cmp);
    let run_medians: Vec<f64> = per_run.iter().map(|r| median(r)).collect();
    json!({
        "p50Ms": percentile(&pooled, 50.0), "p99Ms": percentile(&pooled, 99.0),
        "minMs": pooled[0], "maxMs": pooled[pooled.len() - 1], "samples": pooled.len(),
        "medianOfRunMediansMs": median(&run_medians),
        "runMedianSpreadMs": {
            "min": run_medians.iter().copied().fold(f64::INFINITY, f64::min),
            "max": run_medians.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        },
    })
}

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

pub struct RunOutcome {
    pub trace: Json,
    pub mount_ms: f64,
    pub latencies_ms: Vec<f64>,
}

pub fn run_once<D: Driver>(
    driver: &D,
    fixture: &Fixture,
    scenario: &Scenario,
) -> Result<RunOutcome, String> {
    let open_start = Instant::now();
    let session = driver.open(fixture, scenario)?;
    let mount_ms = ms_since(open_start);
    let mut latencies_ms = Vec::with_capacity(scenario.steps.len());
    let mut steps = Vec::with_capacity(scenario.steps.len());
    for step in &scenario.steps {
        let start = Instant::now();
        let traced = session.dispatch(step)?;
        latencies_ms.push(ms_since(start));
        steps.push(traced);
    }
    Ok(RunOutcome {
        trace: json!({ "steps": steps }),
        mount_ms,
        latencies_ms,
    })
}

/// Live heap bytes and resident set bytes at this instant.
pub type MemorySampler = dyn Fn() -> (u64, u64);

fn sample(sampler: &MemorySampler) -> Json {
    let (heap, rss) = sampler();
    json!({ "heapUsedBytes": heap, "rssBytes": rss })
}

fn measure_memory<D: Driver>(
    driver: &D,
    fixture: &Fixture,
    scenario: &Scenario,
    events: usize,
    sampler: &MemorySampler,
) -> Result<Json, String> {
    let session = driver.open(fixture, scenario)?;
    let after_mount = sample(sampler);
    let after_events = (0..events)
        .try_for_each(|i| {
            session
                .dispatch(&scenario.steps[i % scenario.steps.len()])
                .map(drop)
        })
        .ok()
        .map(|()| sample(sampler));
    Ok(json!({ "afterMount": after_mount, "afterEvents": after_events, "events": events }))
}

pub struct BenchOptions<'a, D: Driver> {
    pub driver: &'a D,
    pub row: &'a str,
    pub row_name: &'a str,
    pub reference_label: &'a str,
    pub references: Option<&'a HashMap<String, Json>>,
    pub commits: Json,
    pub environment: Json,
    pub warmups: usize,
    pub measured: usize,
    pub memory_events: usize,
    pub sampler: &'a MemorySampler,
}

fn failure(
    base: &Map<String, Json>,
    status: &str,
    trace: Option<Json>,
    diff: Option<TraceDifference>,
    error: Option<String>,
) -> Json {
    let mut out = base.clone();
    out.insert("status".into(), json!(status));
    out.insert("trace".into(), trace.unwrap_or(Json::Null));
    if let Some(d) = diff {
        out.insert("parityFailure".into(), json!({ "stepIndex": d.step_index, "field": d.field, "expected": d.expected, "actual": d.actual }));
    }
    if let Some(e) = error {
        out.insert("error".into(), json!(e));
    }
    Json::Object(out)
}

fn bench_workload<D: Driver>(
    o: &BenchOptions<D>,
    fixture: &Fixture,
    scenario: &Scenario,
    index: usize,
) -> Json {
    let key = scenario_key(fixture, index);
    let first = &scenario.steps[0];
    let mut base = Map::new();
    base.insert("key".into(), json!(key));
    base.insert("fixture".into(), json!(fixture.name));
    base.insert(
        "scenario".into(),
        json!(scenario
            .name
            .clone()
            .unwrap_or_else(|| format!("{}.{}", first.trait_name, first.event))),
    );
    let reference = match o.references {
        Some(refs) => match refs.get(&key) {
            Some(r) => Some(r),
            None => {
                return failure(
                    &base,
                    "error",
                    None,
                    None,
                    Some(format!(
                        "no reference trace for {key} from {}",
                        o.reference_label
                    )),
                )
            }
        },
        None => None,
    };
    let gate =
        |trace: &Json| reference.and_then(|r| compare_traces(&fixture.clock_fields, r, trace));
    let run = || -> Result<Json, Json> {
        let cold = Instant::now();
        let session = o
            .driver
            .open(fixture, scenario)
            .map_err(|e| failure(&base, "error", None, None, Some(e)))?;
        let init_ms = ms_since(cold);
        let mut steps = Vec::new();
        let mut first_dispatch_ms = 0.0;
        for (i, step) in scenario.steps.iter().enumerate() {
            let start = Instant::now();
            let traced = session
                .dispatch(step)
                .map_err(|e| failure(&base, "error", None, None, Some(e)))?;
            if i == 0 {
                first_dispatch_ms = ms_since(start);
            }
            steps.push(traced);
        }
        drop(session);
        let first_trace = json!({ "steps": steps });
        if let Some(d) = gate(&first_trace) {
            return Err(failure(
                &base,
                "parityFailure",
                Some(first_trace),
                Some(d),
                None,
            ));
        }
        let checked = |outcome: Result<RunOutcome, String>| -> Result<RunOutcome, Json> {
            let outcome = outcome.map_err(|e| failure(&base, "error", None, None, Some(e)))?;
            match gate(&outcome.trace) {
                Some(d) => Err(failure(
                    &base,
                    "parityFailure",
                    Some(outcome.trace),
                    Some(d),
                    None,
                )),
                None => Ok(outcome),
            }
        };
        for _ in 0..o.warmups {
            checked(run_once(o.driver, fixture, scenario))?;
        }
        let (mut per_run, mut mounts, mut dispatched_ms) = (Vec::new(), Vec::new(), 0.0);
        for _ in 0..o.measured {
            let outcome = checked(run_once(o.driver, fixture, scenario))?;
            dispatched_ms += outcome.latencies_ms.iter().sum::<f64>();
            mounts.push(outcome.mount_ms);
            per_run.push(outcome.latencies_ms);
        }
        let event_count: usize = per_run.iter().map(Vec::len).sum();
        let memory = measure_memory(o.driver, fixture, scenario, o.memory_events, o.sampler)
            .map_err(|e| failure(&base, "error", None, None, Some(e)))?;
        let mut out = base.clone();
        out.insert("status".into(), json!("timed"));
        out.insert("trace".into(), first_trace);
        out.insert("timing".into(), json!({
            "eventLatency": latency_stats(&per_run),
            "stepsPerSecond": event_count as f64 / (dispatched_ms / 1000.0),
            "mountMedianMs": median(&mounts),
            "coldStart": { "moduleLoadMs": 0.0, "initMs": init_ms, "firstDispatchMs": first_dispatch_ms, "totalMs": init_ms + first_dispatch_ms },
            "memory": memory,
        }));
        Ok(Json::Object(out))
    };
    run().unwrap_or_else(|failed| failed)
}

/// The whole bench result in `@almadar-io/kernel-bench-tool`'s `BenchResult` shape.
pub fn run_bench<D: Driver>(o: &BenchOptions<D>, fixtures: &[Fixture]) -> Json {
    let workloads: Vec<Json> = fixtures
        .iter()
        .flat_map(|f| f.scenarios.iter().enumerate().map(move |(i, s)| (f, s, i)))
        .map(|(f, s, i)| bench_workload(o, f, s, i))
        .collect();
    json!({
        "harness": o.row, "harnessName": o.row_name, "reference": o.reference_label,
        "commits": o.commits, "environment": o.environment,
        "warmupRuns": o.warmups, "measuredRuns": o.measured, "workloads": workloads,
    })
}

pub fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn rss_bytes() -> u64 {
    let pid = std::process::id().to_string();
    command_output("ps", &["-o", "rss=", "-p", &pid])
        .parse::<u64>()
        .unwrap_or(0)
        * 1024
}

fn cpu_model() -> String {
    let mac = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if !mac.is_empty() {
        return mac;
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn load_average() -> Vec<f64> {
    let raw = std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_else(|_| command_output("sysctl", &["-n", "vm.loadavg"]));
    raw.split_whitespace()
        .filter_map(|t| t.trim_matches(|c| c == '{' || c == '}').parse().ok())
        .take(3)
        .collect()
}

/// The `environment` block of a bench result; `node` carries the toolchain, since a Rust row has no Node.
pub fn machine_environment() -> Json {
    json!({
        "node": command_output("rustc", &["--version"]),
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpuModel": cpu_model(),
        "cpuCount": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "loadAverage": load_average(),
        "note": std::env::var("ALMADAR_BENCH_NOTE").unwrap_or_default(),
    })
}

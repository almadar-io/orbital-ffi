//! R1 of the embedded-kernel benchmark: the C ABI's trace must equal R0's (the `orbital-core`
//! `RuntimeKernel` trace) wherever the client role runs the whole circuit locally. A fixture whose
//! effects are server effects (fetch, persist, notify) leaves the client as server legs, so its C ABI
//! trace differs by design and `bench-report` shows it as a parity failure.
//! `ALMADAR_PARITY_FIXTURES` = the fixtures folder, `ALMADAR_PARITY_REFERENCE` = R0's bench result;
//! run with `-- --ignored`.

#[allow(dead_code)]
#[path = "../benches/c_abi_fixtures/c_abi_session.rs"]
mod c_abi_session;
#[allow(dead_code)]
#[path = "../benches/c_abi_fixtures/scenario_bench.rs"]
mod scenario_bench;

use c_abi_session::CAbiDriver;
use scenario_bench::{compare_traces, load_fixtures, reference_traces, run_scenario};
use std::path::PathBuf;

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set")))
}

fn r1_differences_from_r0(fixture_names: &[&str]) -> Vec<String> {
    let fixtures = load_fixtures(&env_path("ALMADAR_PARITY_FIXTURES")).unwrap();
    let reference = reference_traces(&env_path("ALMADAR_PARITY_REFERENCE")).unwrap();
    let mut checked = 0;
    let mut differences = Vec::new();
    for fixture in fixtures
        .iter()
        .filter(|f| fixture_names.contains(&f.name.as_str()))
    {
        for (index, scenario) in fixture.scenarios.iter().enumerate() {
            let key = format!("{}#{index}", fixture.name);
            let want = reference
                .get(&key)
                .unwrap_or_else(|| panic!("no R0 trace for {key}"));
            checked += 1;
            match run_scenario(&CAbiDriver, fixture, scenario) {
                Err(e) => differences.push(format!("{key}: R1 failed: {e}")),
                Ok(trace) => {
                    if let Some(d) = compare_traces(&fixture.clock_fields, want, &trace) {
                        differences.push(format!(
                            "{key}: step {} field {}\n  R0: {}\n  R1: {}",
                            d.step_index + 1,
                            d.field,
                            d.expected,
                            d.actual
                        ));
                    }
                }
            }
        }
    }
    assert!(
        checked >= fixture_names.len(),
        "only {checked} scenarios ran"
    );
    differences
}

#[test]
#[ignore = "needs ALMADAR_PARITY_FIXTURES and ALMADAR_PARITY_REFERENCE"]
fn r1_trace_equals_r0_on_audit_provenance_and_visit_key() {
    let differences = r1_differences_from_r0(&["audit-provenance", "visit-key"]);
    assert!(
        differences.is_empty(),
        "R1 differs from R0:\n{}",
        differences.join("\n")
    );
}

#[test]
#[ignore = "needs ALMADAR_PARITY_FIXTURES and ALMADAR_PARITY_REFERENCE"]
fn r1_trace_differs_from_r0_where_the_circuit_has_server_effects() {
    let differences = r1_differences_from_r0(&["fetch-payload"]);
    assert_eq!(differences.len(), 5, "{differences:?}");
}

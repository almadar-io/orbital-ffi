//! `orbital_kernel_frame`: the native tick clock driving a real client kernel, against the
//! `tick-counter` conformance fixture (interval tick `beat` + emit, `frameTick`, state-guarded
//! `guardedTick`, and a `Listener` that hears `Ticker.BEAT`).

use crate::kernel_c_api::{
    orbital_kernel_destroy, orbital_kernel_dispatch, orbital_kernel_frame,
    orbital_kernel_frame_pause, orbital_kernel_frame_resume, orbital_kernel_free_string,
    orbital_kernel_init, orbital_kernel_tick_count, OrbitalKernelHandle,
};
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::ptr;

fn fixture(slug: &str) -> String {
    crate::resolved_fixture(slug)
}

fn basic_entity() -> String {
    crate::resolved_fixture("basic-entity")
}

struct Kernel(*mut OrbitalKernelHandle);

// SAFETY: the handle serializes every call behind its own `call_lock`.
unsafe impl Send for Kernel {}
unsafe impl Sync for Kernel {}

impl Kernel {
    fn new(schema_json: &str) -> Self {
        let schema = CString::new(schema_json).unwrap();
        let client = CString::new("test-client").unwrap();
        let handle = unsafe { orbital_kernel_init(schema.as_ptr(), client.as_ptr()) };
        assert!(!handle.is_null(), "kernel init");
        Self(handle)
    }

    fn frame(&self, now_ms: u64) -> Value {
        take_json(unsafe { orbital_kernel_frame(self.0, now_ms) })
    }

    fn dispatch(&self, trait_name: &str, event: &str) -> Value {
        let t = CString::new(trait_name).unwrap();
        let e = CString::new(event).unwrap();
        take_json(unsafe {
            orbital_kernel_dispatch(self.0, t.as_ptr(), ptr::null(), e.as_ptr(), ptr::null())
        })
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        unsafe { orbital_kernel_destroy(self.0) };
    }
}

fn take_json(raw: *mut std::ffi::c_char) -> Value {
    if raw.is_null() {
        return Value::Null;
    }
    let value = serde_json::from_str(unsafe { CStr::from_ptr(raw) }.to_str().unwrap()).unwrap();
    unsafe { orbital_kernel_free_string(raw) };
    value
}

fn fired(frame: &Value) -> Vec<String> {
    frame["fired"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| f["tick"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn field(frame: &Value, trait_name: &str, name: &str) -> f64 {
    frame["response"]["entityByTrait"][trait_name][name]
        .as_f64()
        .unwrap_or_else(|| panic!("{trait_name}.{name} missing: {frame}"))
}

fn booted() -> Kernel {
    let kernel = Kernel::new(&fixture("tick-counter"));
    kernel.dispatch("Ticker", "INIT");
    kernel.dispatch("Listener", "INIT");
    kernel
}

#[test]
fn the_clock_counts_every_declared_tick_and_a_tickless_schema_has_none() {
    let kernel = booted();
    assert_eq!(unsafe { orbital_kernel_tick_count(kernel.0) }, 3);
    let tickless = Kernel::new(&basic_entity());
    assert_eq!(unsafe { orbital_kernel_tick_count(tickless.0) }, 0);
    assert!(
        tickless.frame(0).is_null(),
        "no declared tick: nothing ever fires"
    );
    assert!(tickless.frame(60_000).is_null());
}

#[test]
fn an_interval_tick_fires_once_per_elapsed_interval_and_never_twice_in_one_pass() {
    let kernel = booted();
    kernel.frame(0);
    let early = kernel.frame(999);
    assert!(
        !fired(&early).contains(&"beat".to_string()),
        "control: 999ms is not a beat: {early}"
    );
    let due = kernel.frame(1000);
    assert_eq!(
        fired(&due).iter().filter(|t| *t == "beat").count(),
        1,
        "{due}"
    );
    assert_eq!(field(&due, "Ticker", "beats"), 1.0);
    let same_pass = kernel.frame(1000);
    assert!(
        !fired(&same_pass).contains(&"beat".to_string()),
        "a second pass at the same instant is not a second beat: {same_pass}"
    );
}

#[test]
fn a_long_gap_fires_the_interval_tick_once_not_once_per_missed_beat() {
    let kernel = booted();
    kernel.frame(0);
    let gap = kernel.frame(10_500);
    assert_eq!(field(&gap, "Ticker", "beats"), 1.0, "{gap}");
}

#[test]
fn a_frame_tick_fires_on_every_pass_including_the_first() {
    let kernel = booted();
    for (pass, now) in [0u64, 16, 33].into_iter().enumerate() {
        let frame = kernel.frame(now);
        assert!(fired(&frame).contains(&"frameTick".to_string()), "{frame}");
        assert_eq!(
            field(&frame, "Ticker", "frames"),
            pass as f64 + 1.0,
            "{frame}"
        );
    }
}

#[test]
fn a_state_guarded_tick_stops_after_the_transition_out_and_resumes_on_the_way_back() {
    let kernel = booted();
    kernel.frame(0);
    let before = kernel.frame(1000);
    assert!(
        fired(&before).contains(&"guardedTick".to_string()),
        "control: runs in `running`: {before}"
    );

    kernel.dispatch("Ticker", "STOP");
    let stopped = kernel.frame(2000);
    assert!(
        !fired(&stopped).contains(&"guardedTick".to_string()),
        "stopped: the guard blocks the tick: {stopped}"
    );
    assert!(
        fired(&stopped).contains(&"beat".to_string()),
        "an unguarded tick keeps running: {stopped}"
    );

    kernel.dispatch("Ticker", "RESUME");
    let resumed = kernel.frame(3000);
    assert!(
        fired(&resumed).contains(&"guardedTick".to_string()),
        "{resumed}"
    );
}

#[test]
fn pause_stops_the_clock_and_resume_does_not_burst_the_paused_span() {
    let kernel = booted();
    kernel.frame(0);
    unsafe { orbital_kernel_frame_pause(kernel.0) };
    assert!(
        kernel.frame(5000).is_null(),
        "paused: even the frame tick is silent"
    );
    unsafe { orbital_kernel_frame_resume(kernel.0) };
    let first = kernel.frame(60_000);
    assert!(
        !fired(&first).contains(&"beat".to_string()),
        "resume re-captures the timestamp, the 55s pause is not a beat: {first}"
    );
    let next = kernel.frame(61_000);
    assert!(
        fired(&next).contains(&"beat".to_string()),
        "cadence resumes: {next}"
    );
}

#[test]
fn resume_without_a_pause_leaves_the_phase_alone() {
    let kernel = booted();
    kernel.frame(0);
    kernel.frame(400);
    unsafe { orbital_kernel_frame_resume(kernel.0) };
    let frame = kernel.frame(1000);
    assert!(fired(&frame).contains(&"beat".to_string()), "{frame}");
}

#[test]
fn one_frame_merges_every_due_tick_into_a_single_response() {
    let kernel = booted();
    kernel.frame(0);
    let frame = kernel.frame(1000);
    let mut ticks = fired(&frame);
    ticks.sort();
    assert_eq!(ticks, ["beat", "frameTick", "guardedTick"], "{frame}");
    assert_eq!(field(&frame, "Ticker", "beats"), 1.0);
    assert_eq!(field(&frame, "Ticker", "frames"), 2.0);
    assert_eq!(field(&frame, "Ticker", "guarded"), 1.0);
    let renders = frame["response"]["clientEffects"]
        .as_array()
        .expect("beat's render-ui is a client effect");
    assert_eq!(renders.len(), 1, "{frame}");
    assert_eq!(renders[0][0], "render-ui", "{frame}");
}

#[test]
fn a_tick_emit_cascades_to_its_listener_through_the_dispatch_path() {
    let kernel = booted();
    kernel.frame(0);
    let frame = kernel.frame(1000);
    let dispatches = frame["dispatches"].as_array().expect("dispatches");
    assert_eq!(dispatches.len(), 1, "one emit, one cascade: {frame}");
    assert_eq!(dispatches[0]["trait"], "Ticker");
    assert_eq!(dispatches[0]["orbital"], "TickCounterOrbital");
    assert_eq!(
        dispatches[0]["response"]["entityByTrait"]["Listener"]["heard"].as_f64(),
        Some(1.0),
        "the listener heard Ticker.BEAT: {frame}"
    );
    assert!(
        dispatches[0]["serverLegs"].as_array().unwrap().is_empty(),
        "local traits: no server leg"
    );
    assert!(
        dispatches[0]["response"]["guardFailed"].is_null(),
        "the emitter is not re-run as a seed for its own emit: {frame}"
    );
    assert!(
        dispatches[0]["response"]["states"]["Ticker"].is_null(),
        "the emitter is not an outcome of its own emit: {frame}"
    );
    assert!(
        dispatches[0]["response"]["clientEffects"].is_null(),
        "no repaint of the emitter's last render: {frame}"
    );

    let quiet = kernel.frame(1100);
    assert!(
        quiet["dispatches"].as_array().is_none_or(|d| d.is_empty()),
        "control: no beat, no emit, no cascade: {quiet}"
    );
}

#[test]
fn a_frame_waits_for_an_in_flight_kernel_call() {
    let kernel = std::sync::Arc::new(booted());
    kernel.frame(0);
    let lock = unsafe { &*kernel.0 }.call_lock.clone();
    let guard = lock.write();
    let worker = {
        let kernel = kernel.clone();
        std::thread::spawn(move || kernel.frame(1000))
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        !worker.is_finished(),
        "the frame must queue behind the held call lock"
    );
    drop(guard);
    let frame = worker.join().unwrap();
    assert!(fired(&frame).contains(&"beat".to_string()), "{frame}");
}

#[test]
fn control_a_frame_does_not_wait_when_no_call_is_in_flight() {
    let kernel = std::sync::Arc::new(booted());
    kernel.frame(0);
    let worker = {
        let kernel = kernel.clone();
        std::thread::spawn(move || kernel.frame(1000))
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(worker.is_finished());
    worker.join().unwrap();
}

fn persisting_listener_schema() -> String {
    serde_json::json!({
        "name": "App",
        "orbitals": [{
            "name": "Main",
            "entity": { "name": "Note", "fields": [
                { "name": "id", "type": "string" },
                { "name": "beats", "type": "number" }
            ] },
            "traits": [
                {
                    "name": "Ticker", "linkedEntity": "Note", "local": true,
                    "stateMachine": {
                        "states": [{ "name": "running", "isInitial": true }],
                        "transitions": [{ "from": "running", "to": "running", "event": "INIT", "effects": [] }]
                    },
                    "ticks": [
                        { "name": "beat", "interval": 100,
                          "effects": [["emit", "BEAT", { "n": 1 }]] },
                        { "name": "saver", "interval": 100,
                          "effects": [["persist", "create", "Note", { "beats": 3 }]] }
                    ]
                },
                {
                    "name": "Persistor", "linkedEntity": "Note",
                    "listens": [{ "event": "BEAT", "triggers": "DO_CREATE", "source": { "kind": "trait", "trait": "Ticker" } }],
                    "stateMachine": {
                        "states": [{ "name": "idle", "isInitial": true }],
                        "transitions": [{ "from": "idle", "to": "idle", "event": "DO_CREATE",
                            "effects": [["persist", "create", "Note", { "beats": "@payload.n" }]] }]
                    }
                }
            ]
        }]
    })
    .to_string()
}

#[test]
fn a_listeners_server_work_rides_a_tick_stamped_leg_and_the_ticks_own_is_counted_and_dropped() {
    let kernel = Kernel::new(&persisting_listener_schema());
    kernel.dispatch("Ticker", "INIT");
    kernel.frame(0);
    let frame = kernel.frame(100);
    let dispatches = frame["dispatches"].as_array().expect("dispatches");
    assert_eq!(dispatches.len(), 1, "{frame}");
    let legs = dispatches[0]["serverLegs"].as_array().unwrap();
    assert_eq!(legs.len(), 1, "{frame}");
    assert_eq!(legs[0]["orbital"], "Main", "the leg names the route it posts to: {frame}");
    let leg = &legs[0]["leg"];
    assert_eq!(leg["event"], "DO_CREATE", "{frame}");
    assert_eq!(leg["tick"], "beat", "the leg carries the tick stamp: {frame}");
    assert_eq!(leg["targetTrait"], "Persistor");
    assert_eq!(
        frame["droppedServerEffects"], 1,
        "the saver tick's own persist has no event to replay under: {frame}"
    );

    let next = kernel.dispatch("Ticker", "INIT");
    assert!(
        next["serverLegs"].as_array().unwrap().is_empty(),
        "control: the tick's own discarded effect did not leak onto the next dispatch: {next}"
    );
}

fn painted_by(frame: &Value, trait_name: &str) -> Vec<f64> {
    frame["response"]["clientEffectsByTrait"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["traitName"] == trait_name)
        .filter_map(|e| e["effect"][2]["value"].as_f64())
        .collect()
}

#[test]
fn a_frame_whose_tick_writes_a_shared_entity_repaints_the_bound_sibling_and_not_the_other() {
    let kernel = Kernel::new(&fixture("entity-rerender"));
    for t in ["Mover", "ScoreBoard", "Caption"] {
        kernel.dispatch(t, "INIT");
    }
    kernel.frame(0);
    let frame = kernel.frame(1000);
    assert!(fired(&frame).contains(&"step".to_string()), "{frame}");
    assert_eq!(painted_by(&frame, "ScoreBoard"), vec![1.0], "{frame}");
    assert!(
        frame["response"]["clientEffectsByTrait"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|e| e["traitName"] != "Caption"),
        "control: the sibling that reads no entity is not repainted: {frame}"
    );
}

fn two_tick_schema(writer_first: bool) -> String {
    let render = |content: &str| serde_json::json!(["render-ui", "hud", { "type": "typography", "content": content }]);
    let tick = |name: &str, effects: serde_json::Value| {
        serde_json::json!({ "name": name, "interval": 100, "effects": effects })
    };
    let emitter = serde_json::json!({
        "name": "Emitter", "linkedEntity": "Board", "local": true,
        "ticks": [tick("announce", serde_json::json!([["emit", "ANNOUNCED", {}]]))],
        "stateMachine": { "states": [{ "name": "idle", "isInitial": true }],
            "transitions": [{ "from": "idle", "to": "idle", "event": "INIT" }] }
    });
    let writer = serde_json::json!({
        "name": "Writer", "linkedEntity": "Board", "local": true,
        "ticks": [tick("bump", serde_json::json!([["set", "@entity.score", ["+", "@entity.score", 1]]]))],
        "stateMachine": { "states": [{ "name": "idle", "isInitial": true }],
            "transitions": [{ "from": "idle", "to": "idle", "event": "INIT" }] }
    });
    let hud = serde_json::json!({
        "name": "Hud", "linkedEntity": "Board", "local": true,
        "listens": [{ "event": "ANNOUNCED", "triggers": "SHOW", "source": { "kind": "trait", "trait": "Emitter" } }],
        "stateMachine": { "states": [{ "name": "idle", "isInitial": true }], "transitions": [
            { "from": "idle", "to": "idle", "event": "INIT" },
            { "from": "idle", "to": "idle", "event": "SHOW", "effects": [render("@entity.score")] }
        ] }
    });
    let traits = if writer_first { vec![writer, emitter, hud] } else { vec![emitter, writer, hud] };
    serde_json::json!({
        "name": "order",
        "orbitals": [{
            "name": "Main",
            "entity": { "name": "Board", "shared": true, "persistence": "runtime",
                "fields": [{ "name": "id", "type": "string" }, { "name": "score", "type": "number", "default": 0 }] },
            "traits": traits
        }]
    })
    .to_string()
}

/// What a host that folds `response` first and then each dispatch in order ends up showing
/// in `trait`'s `slot`.
fn host_shows(frame: &Value, trait_name: &str, slot: &str) -> Option<f64> {
    let mut shown = None;
    let mut fold = |response: &Value| {
        for e in response["clientEffectsByTrait"].as_array().into_iter().flatten() {
            if e["traitName"] == trait_name && e["effect"][1] == slot {
                shown = e["effect"][2]["content"].as_f64();
            }
        }
    };
    fold(&frame["response"]);
    for d in frame["dispatches"].as_array().into_iter().flatten() {
        fold(&d["response"]);
    }
    shown
}

fn two_tick_frame(writer_first: bool) -> Value {
    let kernel = Kernel::new(&two_tick_schema(writer_first));
    for t in ["Emitter", "Writer", "Hud"] {
        kernel.dispatch(t, "INIT");
    }
    kernel.frame(0);
    kernel.frame(100)
}

#[test]
fn a_later_ticks_repaint_wins_over_an_earlier_ticks_dispatch_render_of_the_same_slot() {
    let frame = two_tick_frame(false);
    assert_eq!(fired(&frame), vec!["announce", "bump"], "{frame}");
    assert_eq!(host_shows(&frame, "Hud", "hud"), Some(1.0), "{frame}");
}

#[test]
fn control_an_earlier_ticks_write_leaves_the_later_dispatch_render_standing() {
    let frame = two_tick_frame(true);
    assert_eq!(fired(&frame), vec!["bump", "announce"], "{frame}");
    assert_eq!(host_shows(&frame, "Hud", "hud"), Some(1.0), "{frame}");
    assert!(
        frame["dispatches"][0]["response"]["clientEffectsByTrait"]
            .as_array()
            .is_some_and(|e| e.iter().any(|e| e["traitName"] == "Hud")),
        "the newer dispatch render is not stripped: {frame}"
    );
}

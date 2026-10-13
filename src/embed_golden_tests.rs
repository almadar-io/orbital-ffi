//! The kernel C ABI's wire output, pinned. Each script under `tests/fixtures/embed/scripts` drives
//! every `orbital_kernel_*` operation and its outputs must equal `tests/fixtures/embed/golden`;
//! `orbital-core`'s `embed_session` test replays the same scripts through `embed::EmbeddedKernel`
//! against the same goldens. `ORB_UPDATE_EMBED_GOLDEN=1` rewrites them.

use crate::kernel_c_api::*;
use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

fn embed_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/embed")
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn opt_cstr(v: Option<&Value>) -> Option<CString> {
    v.and_then(Value::as_str).map(cstr)
}

fn opt_ptr(c: &Option<CString>) -> *const c_char {
    c.as_ref().map_or(ptr::null(), |c| c.as_ptr())
}

unsafe fn take(raw: *mut c_char) -> Value {
    if raw.is_null() {
        return Value::Null;
    }
    let text = CStr::from_ptr(raw).to_str().unwrap().to_string();
    orbital_kernel_free_string(raw);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

unsafe fn run_step(handle: *mut OrbitalKernelHandle, step: &Value) -> Value {
    let s = |key: &str| {
        cstr(
            step[key]
                .as_str()
                .unwrap_or_else(|| panic!("{key}: {step}")),
        )
    };
    let json_arg = |key: &str| cstr(&step[key].to_string());
    match step["op"].as_str().unwrap() {
        "dispatch" => {
            let t = s("trait");
            let e = s("event");
            let entity = opt_cstr(step.get("entityId"));
            let payload = step.get("payload").map(|p| cstr(&p.to_string()));
            take(orbital_kernel_dispatch(
                handle,
                t.as_ptr(),
                opt_ptr(&entity),
                e.as_ptr(),
                opt_ptr(&payload),
            ))
        }
        "apply" => {
            let response = json_arg("response");
            let already = json_arg("alreadyDelivered");
            take(orbital_kernel_apply_server_response(
                handle,
                response.as_ptr(),
                already.as_ptr(),
            ))
        }
        "restore" => {
            let t = s("trait");
            let snapshot = json_arg("snapshot");
            let entity = opt_cstr(step.get("entityId"));
            take(orbital_kernel_restore(
                handle,
                t.as_ptr(),
                opt_ptr(&entity),
                snapshot.as_ptr(),
            ))
        }
        "abandon" => {
            let t = s("trait");
            let e = opt_cstr(step.get("event"));
            let err = opt_cstr(step.get("error"));
            orbital_kernel_abandon_server_leg(handle, t.as_ptr(), opt_ptr(&e), opt_ptr(&err));
            Value::Null
        }
        "tick" => {
            let t = s("trait");
            let k = s("tick");
            let entity = opt_cstr(step.get("entityId"));
            take(orbital_kernel_tick(
                handle,
                t.as_ptr(),
                k.as_ptr(),
                opt_ptr(&entity),
            ))
        }
        "frame" => take(orbital_kernel_frame(
            handle,
            step["nowMs"].as_u64().unwrap(),
        )),
        "framePause" => {
            orbital_kernel_frame_pause(handle);
            Value::Null
        }
        "frameResume" => {
            orbital_kernel_frame_resume(handle);
            Value::Null
        }
        "tickCount" => json!(orbital_kernel_tick_count(handle)),
        "getState" => {
            let t = s("trait");
            let entity = opt_cstr(step.get("entityId"));
            take(orbital_kernel_get_state(
                handle,
                t.as_ptr(),
                opt_ptr(&entity),
            ))
        }
        "snapshot" => take(orbital_kernel_snapshot(handle)),
        "setThemeMode" => {
            let m = s("mode");
            orbital_kernel_set_theme_mode(handle, m.as_ptr());
            Value::Null
        }
        "setUser" => {
            let u = json_arg("user");
            orbital_kernel_set_current_user(handle, u.as_ptr());
            Value::Null
        }
        "seed" => {
            let t = s("entityType");
            let rows = json_arg("rows");
            orbital_kernel_seed(handle, t.as_ptr(), rows.as_ptr());
            Value::Null
        }
        "remount" => {
            let traits = json_arg("traits");
            orbital_kernel_remount(handle, traits.as_ptr());
            Value::Null
        }
        "mount" => {
            let traits = json_arg("traits");
            orbital_kernel_mount(handle, traits.as_ptr());
            Value::Null
        }
        "lastError" => take(orbital_kernel_last_error(handle)),
        other => panic!("unknown op {other}"),
    }
}

fn run_script(script: &Value) -> Vec<Value> {
    let schema = cstr(&crate::resolved_fixture(script["schema"].as_str().unwrap()));
    let client = cstr(script["clientId"].as_str().unwrap());
    unsafe {
        let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(!handle.is_null(), "init {}", script["schema"]);
        let mut outputs = Vec::new();
        for step in script["steps"].as_array().unwrap() {
            outputs.push(run_step(handle, step));
        }
        orbital_kernel_destroy(handle);
        outputs
    }
}

#[test]
fn every_script_matches_its_golden() {
    let update = std::env::var("ORB_UPDATE_EMBED_GOLDEN").is_ok();
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(embed_dir().join("scripts"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    scripts.sort();
    assert!(!scripts.is_empty());
    for path in scripts {
        let script: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let outputs = Value::Array(run_script(&script));
        let golden = embed_dir().join("golden").join(path.file_name().unwrap());
        if update {
            std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
            std::fs::write(
                &golden,
                serde_json::to_string_pretty(&outputs).unwrap() + "\n",
            )
            .unwrap();
            continue;
        }
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(&golden).unwrap_or_else(|e| {
                panic!(
                    "{}: {e} (record with ORB_UPDATE_EMBED_GOLDEN=1)",
                    golden.display()
                )
            }))
            .unwrap();
        for (i, (got, want)) in outputs
            .as_array()
            .unwrap()
            .iter()
            .zip(expected.as_array().unwrap())
            .enumerate()
        {
            assert_eq!(
                got,
                want,
                "{} step {i} {}",
                path.display(),
                script["steps"][i]
            );
        }
        assert_eq!(
            outputs.as_array().unwrap().len(),
            expected.as_array().unwrap().len(),
            "{}",
            path.display()
        );
    }
}

#[test]
fn control_a_changed_step_breaks_the_golden() {
    let script: Value = serde_json::from_str(
        &std::fs::read_to_string(embed_dir().join("scripts/rollback.json")).unwrap(),
    )
    .unwrap();
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(embed_dir().join("golden/rollback.json")).unwrap(),
    )
    .unwrap();
    let mut altered = script.clone();
    altered["steps"][2]["event"] = json!("INIT");
    assert_ne!(Value::Array(run_script(&altered)), golden);
    assert_eq!(Value::Array(run_script(&script)), golden);
}

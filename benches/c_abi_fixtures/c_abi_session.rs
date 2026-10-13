//! R1's driver: the `orbital_kernel_*` C ABI that iOS and Android link, called the way a host does:
//! every argument a C string, every result a JSON string to parse and free. The kernel behind it is
//! the client-role `EmbeddedKernel`, so a server leg is returned in the wire rather than run.
//! `states` is one `orbital_kernel_get_state` per trait of the scenario orbital (entity `singleton`),
//! omitting traits the kernel holds no state for: `orbital_kernel_snapshot` reads entity `None` and so
//! reports every state empty today.

use crate::scenario_bench::{Driver, Fixture, Scenario, Session, Step};
use orbital_ffi::kernel_c_api::*;
use serde_json::{json, Map, Value as Json};
use std::ffi::{c_char, CStr, CString};
use std::ptr;

const DEFAULT_VIEWER: &str =
    r#"{"id":"viewer-1","name":"Dev Viewer","email":"viewer@example.com","role":""}"#;

pub struct CAbiDriver;

pub struct HandleSession {
    handle: *mut OrbitalKernelHandle,
    orbital_traits: Vec<String>,
}

impl Drop for HandleSession {
    fn drop(&mut self) {
        unsafe { orbital_kernel_destroy(self.handle) }
    }
}

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|e| format!("interior nul: {e}"))
}

/// Takes a result string the library allocated: copies it out, frees it, parses it.
unsafe fn take_json(raw: *mut c_char, what: &str) -> Result<Json, String> {
    if raw.is_null() {
        return Err(format!("{what}: null result"));
    }
    let text = CStr::from_ptr(raw)
        .to_str()
        .map_err(|e| format!("{what}: {e}"))
        .map(str::to_string);
    orbital_kernel_free_string(raw);
    let value: Json = serde_json::from_str(&text?).map_err(|e| format!("{what}: {e}"))?;
    match value.get("error").and_then(Json::as_str) {
        Some(error) => Err(format!("{what}: {error}")),
        None => Ok(value),
    }
}

impl Driver for CAbiDriver {
    type Open = HandleSession;

    fn open(&self, fixture: &Fixture, scenario: &Scenario) -> Result<HandleSession, String> {
        let orbital_traits: Vec<String> = fixture.schema["orbitals"]
            .as_array()
            .and_then(|os| {
                os.iter()
                    .find(|o| o["name"].as_str() == Some(scenario.orbital.as_str()))
            })
            .ok_or_else(|| format!("{}: no orbital \"{}\"", fixture.name, scenario.orbital))?
            ["traits"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let schema = cstr(&fixture.schema.to_string())?;
        let client = cstr("bench")?;
        let user = cstr(DEFAULT_VIEWER)?;
        unsafe {
            let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
            if handle.is_null() {
                return Err(format!(
                    "{}: orbital_kernel_init returned null",
                    fixture.name
                ));
            }
            orbital_kernel_set_current_user(handle, user.as_ptr());
            Ok(HandleSession {
                handle,
                orbital_traits,
            })
        }
    }
}

impl Session for HandleSession {
    fn dispatch(&self, step: &Step) -> Result<Json, String> {
        let trait_name = cstr(&step.trait_name)?;
        let event = cstr(&step.event)?;
        let payload = cstr(
            &step
                .payload
                .clone()
                .unwrap_or_else(|| json!({}))
                .to_string(),
        )?;
        let wire = unsafe {
            take_json(
                orbital_kernel_dispatch(
                    self.handle,
                    trait_name.as_ptr(),
                    ptr::null(),
                    event.as_ptr(),
                    payload.as_ptr(),
                ),
                &format!("{}.{}", step.trait_name, step.event),
            )?
        };
        let mut states = Map::new();
        let singleton = cstr("singleton")?;
        for name in &self.orbital_traits {
            let trait_c = cstr(name)?;
            let state = unsafe {
                let raw =
                    orbital_kernel_get_state(self.handle, trait_c.as_ptr(), singleton.as_ptr());
                if raw.is_null() {
                    return Err(format!("get_state {name}: null result"));
                }
                let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
                orbital_kernel_free_string(raw);
                text
            };
            if !state.is_empty() {
                states.insert(name.clone(), Json::String(state));
            }
        }
        if !states.contains_key(&step.trait_name) {
            return Err(format!(
                "trait \"{}\" has no state after the dispatch",
                step.trait_name
            ));
        }
        let response = &wire["response"];
        let emits: Vec<Json> = response["emittedEvents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| json!({ "event": e["event"], "payload": e.get("payload").cloned().unwrap_or(Json::Null) }))
                    .collect()
            })
            .unwrap_or_default();
        let effects = response
            .get("clientEffects")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(json!({
            "trait": step.trait_name, "event": step.event,
            "states": Json::Object(states), "effects": effects, "emits": emits,
        }))
    }
}

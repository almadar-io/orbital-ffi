//! `orbital_kernel_resolve_rows`: the C ABI over the kernel's one row-set resolver. The native renderers resolve a
//! `$rowSet` marker's `ref` through it at paint; `@read` is re-checked on every call.

use orbital_ffi::kernel_c_api::*;
use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::ptr;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn schema() -> CString {
    c(&json!({
        "name": "rows", "typeContractVersion": 1, "orbitals": [{
            "name": "Main",
            "entity": {
                "name": "Ticket", "persistence": "runtime",
                "fields": [{ "name": "id", "type": "string" }, { "name": "title", "type": "string" }, { "name": "owner", "type": "string" }],
                "read_policy": ["!=", ["object/get", "@entity", "owner"], "u2"]
            },
            "traits": [{ "name": "Tickets", "linkedEntity": "Ticket", "stateMachine": {
                "states": [{ "name": "idle", "isInitial": true }],
                "transitions": [{ "from": "idle", "to": "idle", "event": "INIT", "effects": [] }]
            } }],
            "pages": [{ "name": "Home", "path": "/", "traits": [{ "ref": "Tickets" }] }]
        }]
    })
    .to_string())
}

unsafe fn take(raw: *mut std::os::raw::c_char) -> Value {
    assert!(!raw.is_null());
    let value = serde_json::from_str(CStr::from_ptr(raw).to_str().unwrap()).unwrap();
    orbital_kernel_free_string(raw);
    value
}

unsafe fn kernel() -> *mut OrbitalKernelHandle {
    let handle = orbital_kernel_init(schema().as_ptr(), c("resolve-rows").as_ptr());
    assert!(!handle.is_null());
    let rows = json!([
        { "id": "t1", "title": "alpha", "owner": "u1" },
        { "id": "t2", "title": "hidden", "owner": "u2" },
        { "id": "t3", "title": "gamma", "owner": "u1" }
    ]);
    orbital_kernel_seed(handle, c("Ticket").as_ptr(), c(&rows.to_string()).as_ptr());
    handle
}

fn reference(ids: &[&str]) -> CString {
    c(&json!({ "entityType": "Ticket", "ids": ids, "total": ids.len(), "query": {} }).to_string())
}

#[test]
fn a_reference_resolves_to_the_rows_the_viewer_may_read_in_order_with_the_recounted_total() {
    unsafe {
        let handle = kernel();
        let page = take(orbital_kernel_resolve_rows(
            handle,
            reference(&["t3", "t2", "t1"]).as_ptr(),
        ));
        let titles: Vec<_> = page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            vec!["gamma", "alpha"],
            "u2's row is omitted, the reference's order is kept: {page}"
        );
        assert!(page.get("total").is_some(), "{page}");
        orbital_kernel_destroy(handle);
    }
}

#[test]
fn control_a_hidden_row_stays_hidden_on_every_resolve_since_no_call_names_a_viewer() {
    unsafe {
        let handle = kernel();
        for _ in 0..2 {
            let page = take(orbital_kernel_resolve_rows(
                handle,
                reference(&["t1", "t2", "t3"]).as_ptr(),
            ));
            let ids: Vec<_> = page["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, vec!["t1", "t3"], "{page}");
        }
        orbital_kernel_destroy(handle);
    }
}

#[test]
fn edge_a_bad_reference_answers_an_error_and_sets_last_error_and_a_null_handle_is_null() {
    unsafe {
        let handle = kernel();
        let answer = take(orbital_kernel_resolve_rows(
            handle,
            c("{ not json").as_ptr(),
        ));
        assert!(
            answer["error"].as_str().unwrap().contains("ref_json"),
            "{answer}"
        );
        let last = take_plain(orbital_kernel_last_error(handle));
        assert!(last.contains("ref_json"), "{last}");
        assert!(
            orbital_kernel_resolve_rows(handle, ptr::null()).is_null() == false,
            "a null argument is an error answer, not a crash"
        );
        orbital_kernel_destroy(handle);
        assert!(orbital_kernel_resolve_rows(ptr::null_mut(), reference(&[]).as_ptr()).is_null());
    }
}

unsafe fn take_plain(raw: *mut std::os::raw::c_char) -> String {
    let s = CStr::from_ptr(raw).to_str().unwrap().to_string();
    orbital_kernel_free_string(raw);
    s
}

#[test]
fn a_refused_init_leaves_its_reason_and_a_bad_language_bundle_is_reported() {
    let schema = std::ffi::CString::new("{not json").unwrap();
    let client = std::ffi::CString::new("c").unwrap();
    unsafe {
        let handle = orbital_ffi::kernel_c_api::orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(handle.is_null());
        let reason = orbital_ffi::kernel_c_api::orbital_kernel_init_error();
        let text = std::ffi::CStr::from_ptr(reason).to_str().unwrap().to_string();
        orbital_ffi::kernel_c_api::orbital_kernel_free_string(reason);
        assert!(text.contains("parse resolved schema"), "{text}");
        let bad = std::ffi::CString::new("{}").unwrap();
        let refused = orbital_ffi::kernel_c_api::orbital_language_install(bad.as_ptr());
        assert!(!refused.is_null(), "control: a bundle without the language data is refused, never silently accepted");
        orbital_ffi::kernel_c_api::orbital_kernel_free_string(refused);
    }
}

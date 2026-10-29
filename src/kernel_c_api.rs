//! Kernel C ABI: `orbital_core::embed::EmbeddedKernel` (the in-app client-role kernel) exposed
//! for the Swift/Kotlin bridges (`OrbitalKernelBridge.swift`/`.kt`) via an opaque handle + JSON
//! in/out. This module only marshals C strings; every decision is the embedded kernel's.
//!
//! No compiler runs on device: `orbital_kernel_init` takes an
//! ALREADY-RESOLVED schema JSON (what `orb compile` embeds as
//! `resolved_schema.json`, B6). Every function null-checks its pointer
//! arguments, catches panics (`catch_unwind`) and reports failure through the
//! handle's own `last_error` + a null/empty return — never a process abort.
//! `last_error` is a per-handle `Mutex<String>`, NOT thread-local: a UI
//! thread and a background reconcile thread share one handle.

use orbital_core::embed::{parse_theme_mode, EmbeddedKernel};
use orbital_core::runtime::TraitSnapshot;
use orbital_core::{EventResponse, OrbitalSchema, Value};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Strings `to_c_string` handed to a host and `orbital_kernel_free_string` has not yet taken back.
static OUTSTANDING_STRINGS: AtomicI64 = AtomicI64::new(0);

/// Opaque handle wrapping the embedded kernel. `last_error` is per-handle so two threads sharing
/// this pointer (the UI thread + a background reconcile thread) never race on a thread-local.
pub struct OrbitalKernelHandle {
    pub(crate) embedded: EmbeddedKernel,
    last_error: Mutex<String>,
    /// Held for the whole of every call that runs the kernel or touches the
    /// collector: a dispatch drains ONE per-handle collector,
    /// so overlapping calls would steal each other's delegated effects.
    /// Writers (dispatch and every mutator) are exclusive; reads share.
    pub(crate) call_lock: Arc<RwLock<()>>,
}

fn set_error(handle: &OrbitalKernelHandle, msg: impl Into<String>) {
    *handle.last_error.lock() = msg.into();
}

/// Read a `*const c_char` as `&str`, `None` for a null pointer or invalid UTF-8.
///
/// # Safety
/// `s` must be a valid null-terminated UTF-8 C string, or null.
unsafe fn read_str<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        None
    } else {
        CStr::from_ptr(s).to_str().ok()
    }
}

fn to_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(cs) => {
            OUTSTANDING_STRINGS.fetch_add(1, Ordering::Relaxed);
            cs.into_raw()
        }
        Err(_) => ptr::null_mut(),
    }
}

fn json_error(handle: &OrbitalKernelHandle, msg: impl Into<String>) -> *mut c_char {
    let msg = msg.into();
    set_error(handle, msg.clone());
    to_c_string(serde_json::json!({ "error": msg }).to_string())
}

fn to_json<T: Serialize>(handle: &OrbitalKernelHandle, value: &T) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(s) => to_c_string(s),
        Err(e) => json_error(handle, format!("serialize: {e}")),
    }
}

/// Why `orbital_kernel_init` returns no handle; the C boundary reports every kind as null.
#[derive(Debug)]
enum InitError {
    Argument(String),
    Entry(orbital_core::runtime::kernel::EntryError),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argument(message) => f.write_str(message),
            Self::Entry(error) => error.fmt(f),
        }
    }
}

impl From<orbital_core::runtime::kernel::EntryError> for InitError {
    fn from(error: orbital_core::runtime::kernel::EntryError) -> Self {
        Self::Entry(error)
    }
}

/// Create a kernel handle from an ALREADY-RESOLVED schema JSON (`orb compile`'s
/// `resolved_schema.json`) and a client id. Every trait is indexed to its
/// owning orbital (see `TraitOwners`). Returns null (nothing to report through — no handle yet)
/// on a null/invalid argument or a schema that fails to parse/register.
///
/// # Safety
/// `resolved_schema_json` and `client_id` must be valid null-terminated UTF-8
/// C strings, or null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_init(
    resolved_schema_json: *const c_char,
    client_id: *const c_char,
) -> *mut OrbitalKernelHandle {
    let result = catch_unwind(AssertUnwindSafe(
        || -> Result<OrbitalKernelHandle, InitError> {
            let schema_json = read_str(resolved_schema_json).ok_or(InitError::Argument(
                "resolved_schema_json is null/invalid utf-8".to_string(),
            ))?;
            let client_id = read_str(client_id).ok_or(InitError::Argument(
                "client_id is null/invalid utf-8".to_string(),
            ))?;
            let schema: OrbitalSchema = serde_json::from_str(schema_json)
                .map_err(|e| InitError::Argument(format!("parse resolved schema: {e}")))?;

            let embedded = EmbeddedKernel::new(schema, client_id)?;

            Ok(OrbitalKernelHandle {
                embedded,
                last_error: Mutex::new(String::new()),
                call_lock: Arc::new(RwLock::new(())),
            })
        },
    ));

    match result {
        Ok(Ok(handle)) => Box::into_raw(Box::new(handle)),
        Ok(Err(error)) => {
            eprintln!("orbital_kernel_init: {error}");
            ptr::null_mut()
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Destroy a handle returned by `orbital_kernel_init`.
///
/// # Safety
/// `handle` must be a valid pointer from `orbital_kernel_init`, or null; must
/// not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_destroy(handle: *mut OrbitalKernelHandle) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _ = Box::from_raw(handle);
        }));
    }
}

/// Dispatch one event through the kernel's cross-trait cascade and report the
/// client-facing response, the server leg to post (if any), the resolved
/// `DispatchMode`, the pre-dispatch snapshot (`RuntimeOptimistic` only), and
/// the `(trait, event)` pairs already realized locally — hand the last one
/// straight to `orbital_kernel_apply_server_response` so a live-push echo of
/// this same dispatch doesn't run twice.
///
/// JSON shape: `{"orbital": "<owning orbital of trait_name>", "seed": {"trait","event"}, "response": EventResponse, "serverLegs": [{"orbital": "<owner of the leg's target trait>", "leg": EventRequest}],
/// "mode": "persistedAwaited"|"hybridClientOnly"|"runtimeOptimistic",
/// "snapshot": TraitSnapshot|null, "alreadyDelivered": [[trait,event],...] (sorted), "awaiting": [{"trait","event","from"}]}`.
///
/// # Safety
/// `handle` must be a valid pointer from `orbital_kernel_init`.
/// `trait_name`/`event` must be valid C strings; `entity_id`/`payload_json`
/// may be null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_dispatch(
    handle: *mut OrbitalKernelHandle,
    trait_name: *const c_char,
    entity_id: *const c_char,
    event: *const c_char,
    payload_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let trait_name = read_str(trait_name).ok_or("trait_name is null/invalid utf-8")?;
        let event = read_str(event).ok_or("event is null/invalid utf-8")?;
        let entity_id = read_str(entity_id);
        handle_ref.embedded.owning_orbital(trait_name)?;
        let payload: Value = match read_str(payload_json) {
            Some(s) if !s.is_empty() => {
                serde_json::from_str(s).map_err(|e| format!("parse payload_json: {e}"))?
            }
            _ => Value::Null,
        };

        handle_ref
            .embedded
            .dispatch(trait_name, entity_id, event, payload)
    }));

    match result {
        Ok(Ok(wire)) => to_json(handle_ref, &wire),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during dispatch"),
    }
}

/// Fold a server `EventResponse` into the kernel — the counterpart to the
/// `serverLeg` a `dispatch` call produced (or an unrelated live-push). Every
/// `(trait, event)` pair in `already_delivered_json` (as returned by
/// `orbital_kernel_dispatch`) is skipped so a client that already realized
/// its own dispatch locally doesn't re-run it on the server's echo.
///
/// JSON out: `{"applied": [[trait, newState], ...], "response": EventResponse}`
/// on success, `{"error": "..."}` on failure (also set as `last_error`).
/// `response` merges every applied outcome the way a dispatch does
/// (`EventResponse::from_process_results`), so its `clientEffects` /
/// `clientEffectsByTrait` fold through the host's dispatch fold.
///
/// # Safety
/// `handle` must be valid; `response_json` must be valid; `already_delivered_json`
/// may be null (treated as empty).
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_apply_server_response(
    handle: *mut OrbitalKernelHandle,
    response_json: *const c_char,
    already_delivered_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let response_json = read_str(response_json).ok_or("response_json is null/invalid utf-8")?;
        let response: EventResponse =
            serde_json::from_str(response_json).map_err(|e| format!("parse response_json: {e}"))?;

        let already_delivered: HashSet<(String, String)> = match read_str(already_delivered_json) {
            Some(s) if !s.is_empty() => {
                let pairs: Vec<(String, String)> = serde_json::from_str(s)
                    .map_err(|e| format!("parse already_delivered_json: {e}"))?;
                pairs.into_iter().collect()
            }
            _ => HashSet::new(),
        };
        handle_ref
            .embedded
            .apply_server_response(&response, &already_delivered)
    }));

    match result {
        Ok(Ok(wire)) => to_json(handle_ref, &wire),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during apply_server_response"),
    }
}

/// Roll a trait instance back to a prior `TraitSnapshot` (the
/// `RuntimeOptimistic` rollback path, `dispatch`'s own `snapshot` field
/// handed back verbatim). Returns the `EventResponse` JSON of the repaint the
/// rollback owes traits bound to the restored entity (no renders when nothing
/// changed); free it with `orbital_kernel_free_string`. Null for a null
/// handle; a failure is `{"error": ...}` and sets `last_error`.
///
/// # Safety
/// `handle` must be valid; `trait_name`/`snapshot_json` must be valid C
/// strings; `entity_id` may be null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_restore(
    handle: *mut OrbitalKernelHandle,
    trait_name: *const c_char,
    entity_id: *const c_char,
    snapshot_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<EventResponse, String> {
        let trait_name = read_str(trait_name).ok_or("trait_name is null/invalid utf-8")?;
        let entity_id = read_str(entity_id);
        let snapshot_json = read_str(snapshot_json).ok_or("snapshot_json is null/invalid utf-8")?;
        let snapshot: TraitSnapshot =
            serde_json::from_str(snapshot_json).map_err(|e| format!("parse snapshot_json: {e}"))?;
        Ok(handle_ref.embedded.restore(trait_name, entity_id, snapshot))
    }));

    match result {
        Ok(Ok(response)) => to_json(handle_ref, &response),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during restore"),
    }
}

/// Run one scheduled tick and return the resulting `EventResponse` JSON (the
/// same client-effect fold shape `dispatch`'s `response` field carries).
///
/// # Safety
/// `handle` must be valid; `trait_name`/`tick_name` must be valid C strings;
/// `entity_id` may be null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_tick(
    handle: *mut OrbitalKernelHandle,
    trait_name: *const c_char,
    tick_name: *const c_char,
    entity_id: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let trait_name = read_str(trait_name).ok_or("trait_name is null/invalid utf-8")?;
        let tick_name = read_str(tick_name).ok_or("tick_name is null/invalid utf-8")?;
        let entity_id = read_str(entity_id);
        handle_ref.embedded.tick(trait_name, tick_name, entity_id)
    }));

    match result {
        Ok(Ok(response)) => to_json(handle_ref, &response),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during tick"),
    }
}

/// The frame delta a painter at version `since` of this kernel's frames needs (`FrameDelta` JSON): empty when it is
/// current, else a snapshot of every frame (EXEC-DIST-7). Free it with `orbital_kernel_free_string`.
///
/// # Safety
/// `handle` must be valid.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_frames_since(handle: *mut OrbitalKernelHandle, since: u64) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.read();
    let result = catch_unwind(AssertUnwindSafe(|| handle_ref.embedded.frames_since(since)));
    match result {
        Ok(Ok(delta)) => to_json(handle_ref, &delta),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during frames_since"),
    }
}

/// Run one host frame of the native tick clock: every declared tick (across all orbitals) whose
/// interval has elapsed runs through the kernel's `process_tick`, at most once, and the results
/// come back as ONE envelope the host folds the way it folds a dispatch.
/// `now_ms` is the host's monotonic frame time in milliseconds (the clock only differences it).
///
/// JSON shape: `{"fired": [{"trait","tick"}], "response": EventResponse (merged tick effects),
/// "dispatches": [{"trait", ...dispatch envelope}] (one per tick `emit`: its listener cascade
/// through `RuntimeKernel::dispatch_tick`, the emitter not re-run as a seed, server leg stamped
/// with the tick), "droppedServerEffects": n, "errors": [...]}`. NULL when no tick fired
/// (nothing to fold); `{"error": ...}` on failure. The first of `errors` is also `last_error`.
///
/// A tick's OWN server effects have no event to replay under (web parity: `useClientTicks`
/// delegates them to a no-op): they are discarded, counted in `droppedServerEffects`, and never
/// ride the next dispatch's leg.
///
/// # Safety
/// `handle` must be valid.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_frame(
    handle: *mut OrbitalKernelHandle,
    now_ms: u64,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    match catch_unwind(AssertUnwindSafe(|| handle_ref.embedded.frame(now_ms))) {
        Ok(Ok(Some(wire))) => {
            if let Some(first) = wire.errors.first() {
                set_error(handle_ref, first.clone());
            }
            to_json(handle_ref, &wire)
        }
        Ok(Ok(None)) => ptr::null_mut(),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during frame"),
    }
}

/// Stop the native tick clock (host backgrounded). Declared ticks and their phase are kept.
///
/// # Safety
/// `handle` must be valid or null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_frame_pause(handle: *mut OrbitalKernelHandle) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    handle_ref.embedded.frame_pause();
}

/// Restart the native tick clock after `orbital_kernel_frame_pause`: the paused span is never
/// counted, so no tick bursts on resume.
///
/// # Safety
/// `handle` must be valid or null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_frame_resume(handle: *mut OrbitalKernelHandle) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    handle_ref.embedded.frame_resume();
}

/// How many `(trait, tick)` pairs the schema declares; a host with `0` never starts a frame loop.
///
/// # Safety
/// `handle` must be valid or null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_tick_count(handle: *mut OrbitalKernelHandle) -> i32 {
    if handle.is_null() {
        return 0;
    }
    i32::try_from((*handle).embedded.tick_count()).unwrap_or(i32::MAX)
}

/// Current state name for a trait instance, or `""` when unset. Plain string,
/// not JSON.
///
/// # Safety
/// `handle` must be valid; `trait_name` must be a valid C string; `entity_id`
/// may be null.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_get_state(
    handle: *mut OrbitalKernelHandle,
    trait_name: *const c_char,
    entity_id: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.read();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<String, String> {
        let trait_name = read_str(trait_name).ok_or("trait_name is null/invalid utf-8")?;
        let entity_id = read_str(entity_id);
        Ok(handle_ref.embedded.get_state(trait_name, entity_id))
    }));

    match result {
        Ok(Ok(state)) => to_c_string(state),
        Ok(Err(e)) => {
            set_error(handle_ref, e);
            to_c_string(String::new())
        }
        Err(_) => {
            set_error(handle_ref, "panic during get_state");
            to_c_string(String::new())
        }
    }
}

/// Every trait state across ALL orbitals (a trait name declared by more than
/// one orbital is listed under `ambiguousTraits` as `{trait, orbitals}`, never
/// in `states`) plus the resolved
/// `@currentTheme`: `{"states": {"<trait>": "<state>", ...}, "theme":
/// ResolvedTheme {key, base, mode, tokens}, "canvasTheme", "motion"}` (`entity_id = "singleton"` per
/// trait).
///
/// # Safety
/// `handle` must be valid.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_snapshot(handle: *mut OrbitalKernelHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.read();
    match catch_unwind(AssertUnwindSafe(|| handle_ref.embedded.snapshot())) {
        Ok(snapshot) => to_json(handle_ref, &snapshot),
        Err(_) => json_error(handle_ref, "panic during snapshot"),
    }
}

/// Resolve a row-set reference (`ref_json` = a `RowSetRef`, the `ref` of a `$rowSet` marker) through the
/// kernel's one resolver (`RuntimeKernel::resolve_rows`): the rows as they are now, re-checked against the
/// current viewer's `@read` policy, with the re-counted total, as `{"rows": [...], "total": N}`. A reference
/// the client cannot resolve answers `{"error": ...}` and sets `last_error`.
///
/// # Safety
/// `handle` must be valid; `ref_json` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_resolve_rows(
    handle: *mut OrbitalKernelHandle,
    ref_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.read();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ref_json = read_str(ref_json).ok_or("ref_json is null/invalid utf-8")?;
        let reference: orbital_core::runtime::row_query::RowSetRef =
            serde_json::from_str(ref_json).map_err(|e| format!("parse ref_json: {e}"))?;
        handle_ref.embedded.resolve_rows(&reference)
    }));
    match result {
        Ok(Ok(page)) => to_json(handle_ref, &page),
        Ok(Err(e)) => json_error(handle_ref, e),
        Err(_) => json_error(handle_ref, "panic during resolve_rows"),
    }
}

/// Set the kernel's tracked light/dark toggle (`"light"|"dark"`), overriding
/// the mode encoded in a declared theme's key suffix on every subsequent
/// `@currentTheme` resolution (`RuntimeKernel::set_theme_mode`). No return
/// value; an invalid `mode` string sets `last_error` and is a no-op.
///
/// # Safety
/// `handle` must be valid; `mode` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_set_theme_mode(
    handle: *mut OrbitalKernelHandle,
    mode: *const c_char,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| {
        parse_theme_mode(read_str(mode).ok_or("mode is null/invalid utf-8")?)
    }));

    match result {
        Ok(Ok(mode)) => handle_ref.embedded.set_theme_mode(mode),
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during set_theme_mode"),
    }
}

/// Report whether `trait_name`'s rendered output is on screen, so its `[visible]` ticks run only
/// then (EXEC-TICK-4). Hosts call it as a trait's view appears and disappears. An invalid
/// argument sets `last_error` and is a no-op.
///
/// # Safety
/// `handle` must be valid; `trait_name` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_set_trait_on_screen(
    handle: *mut OrbitalKernelHandle,
    trait_name: *const c_char,
    on_screen: bool,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| {
        read_str(trait_name).ok_or("trait_name is null/invalid utf-8")
    }));
    match result {
        Ok(Ok(name)) => handle_ref.embedded.set_trait_on_screen(name, on_screen),
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during set_trait_on_screen"),
    }
}

/// Bind `@user` for every subsequent dispatch on this handle. `user_json`
/// must decode to a `Value` (an object, typically).
///
/// # Safety
/// `handle` must be valid; `user_json` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_set_current_user(
    handle: *mut OrbitalKernelHandle,
    user_json: *const c_char,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Value, String> {
        let user_json = read_str(user_json).ok_or("user_json is null/invalid utf-8")?;
        serde_json::from_str(user_json).map_err(|e| format!("parse user_json: {e}"))
    }));

    match result {
        Ok(Ok(user)) => handle_ref.embedded.set_current_user(user),
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during set_current_user"),
    }
}

/// Seed `entity_type` rows (`rows_json` = a JSON array) into the handle's
/// in-memory entity cache, so a subsequent INIT fetch/list resolves real
/// data. Each row's own `id` is honored (defaults to `"singleton"`).
///
/// # Safety
/// `handle` must be valid; `entity_type`/`rows_json` must be valid C strings.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_seed(
    handle: *mut OrbitalKernelHandle,
    entity_type: *const c_char,
    rows_json: *const c_char,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        let entity_type = read_str(entity_type).ok_or("entity_type is null/invalid utf-8")?;
        let rows_json = read_str(rows_json).ok_or("rows_json is null/invalid utf-8")?;
        let rows: Vec<Value> =
            serde_json::from_str(rows_json).map_err(|e| format!("parse rows_json: {e}"))?;
        handle_ref.embedded.seed(entity_type, rows)
    }));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during seed"),
    }
}

/// Parse a JSON array of trait names for `remount`/`mount`.
unsafe fn read_traits(traits_json: *const c_char) -> Result<Vec<String>, String> {
    let traits_json = read_str(traits_json).ok_or("traits_json is null/invalid utf-8")?;
    serde_json::from_str(traits_json).map_err(|e| format!("parse traits_json: {e}"))
}

/// Declare a new mount (`RuntimeKernel::remount_traits`) before its lifecycle
/// events fire: a routed delivery reaches none of `traits_json` (a JSON array
/// of trait names) before that trait's own INIT. Pass exactly the traits about
/// to be INITed — a declared trait that never INITs holds its deliveries.
/// Mirrors `ClientKernel::remount`. Failures set `last_error` and leave the
/// current mount unchanged.
///
/// # Safety
/// `handle` must be valid; `traits_json` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_remount(
    handle: *mut OrbitalKernelHandle,
    traits_json: *const c_char,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    match catch_unwind(AssertUnwindSafe(|| read_traits(traits_json))) {
        Ok(Ok(traits)) => handle_ref.embedded.remount(traits),
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during remount"),
    }
}

/// Add traits to the current mount (`RuntimeKernel::mount_traits`) without
/// dropping the traits already mounted — a gated page's circuit joining after
/// the viewer authenticates. `traits_json` is a JSON array of trait names
/// about to be INITed. Failures set `last_error` and leave the mount unchanged.
///
/// # Safety
/// `handle` must be valid; `traits_json` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_mount(
    handle: *mut OrbitalKernelHandle,
    traits_json: *const c_char,
) {
    if handle.is_null() {
        return;
    }
    let handle_ref = &*handle;
    let _call = handle_ref.call_lock.write();
    match catch_unwind(AssertUnwindSafe(|| read_traits(traits_json))) {
        Ok(Ok(traits)) => handle_ref.embedded.mount(traits),
        Ok(Err(e)) => set_error(handle_ref, e),
        Err(_) => set_error(handle_ref, "panic during mount"),
    }
}

/// The handle's last recorded error message (`""` if none since the last
/// call that succeeded and cleared nothing — errors accumulate until the
/// next failing call overwrites them; callers read this right after a
/// null/`{"error":...}` return).
///
/// # Safety
/// `handle` must be valid.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_last_error(
    handle: *mut OrbitalKernelHandle,
) -> *mut c_char {
    if handle.is_null() {
        return to_c_string(String::new());
    }
    let handle_ref = &*handle;
    to_c_string(handle_ref.last_error.lock().clone())
}

/// Free a string returned by any `orbital_kernel_*` function.
///
/// # Safety
/// `s` must have been returned by an `orbital_kernel_*` function (or null),
/// and must not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn orbital_kernel_free_string(s: *mut c_char) {
    if !s.is_null() {
        let _ = CString::from_raw(s);
        OUTSTANDING_STRINGS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Process-wide count of strings an `orbital_kernel_*` function returned that
/// `orbital_kernel_free_string` has not freed yet. A host that frees every
/// result reads `0` once its calls are done; the device benchmark gates on it.
#[no_mangle]
pub extern "C" fn orbital_kernel_outstanding_strings() -> i64 {
    OUTSTANDING_STRINGS.load(Ordering::Relaxed)
}

/// Debug C ABI outside `KERNEL_C_SYMBOLS`: read by the device benchmark, never part of a generated bridge.
pub const DEBUG_C_SYMBOLS: &[&str] = &["orbital_kernel_outstanding_strings"];

/// The `orbital_kernel_*` C ABI this module defines, kept next to the
/// definitions (mirrors `jni_bridge::JNI_SYMBOLS`) so the iOS/Android bridge
/// generation tests can assert coverage without a hand-typed twin.
pub const KERNEL_C_SYMBOLS: &[&str] = &[
    "orbital_kernel_init",
    "orbital_kernel_destroy",
    "orbital_kernel_dispatch",
    "orbital_kernel_apply_server_response",
    "orbital_kernel_restore",
    "orbital_kernel_tick",
    "orbital_kernel_frame",
    "orbital_kernel_frame_pause",
    "orbital_kernel_frame_resume",
    "orbital_kernel_tick_count",
    "orbital_kernel_get_state",
    "orbital_kernel_snapshot",
    "orbital_kernel_resolve_rows",
    "orbital_kernel_set_theme_mode",
    "orbital_kernel_set_trait_on_screen",
    "orbital_kernel_set_current_user",
    "orbital_kernel_seed",
    "orbital_kernel_remount",
    "orbital_kernel_mount",
    "orbital_kernel_last_error",
    "orbital_kernel_free_string",
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::CString;

    /// Guards against `KERNEL_C_SYMBOLS` drifting from the `fn` definitions.
    #[test]
    fn kernel_c_symbols_match_declared_fns() {
        let src = include_str!("kernel_c_api.rs");
        for sym in KERNEL_C_SYMBOLS {
            let needle = format!("fn {sym}(");
            assert!(
                src.contains(&needle),
                "KERNEL_C_SYMBOLS lists {sym:?} but no `fn {sym}(` found"
            );
        }
    }

    // Must resolve with no `uses` peers: a crate's tests never depend on
    // sibling packages (`std-notes.orb` needs the `almadar-behaviors`
    // monorepo sibling and isn't available to a standalone checkout).
    fn resolved_schema_json() -> String {
        crate::resolved_fixture("basic-entity")
    }

    pub(crate) const FIRST_ORBITAL: &str = "Task Management";
    pub(crate) const SECOND_ORBITAL: &str = "Chore Management";

    /// `01-basic-entity.orb` plus a clone of its orbital under
    /// `SECOND_ORBITAL` (entity `Chore`, trait named `second_trait`). Passing
    /// the first orbital's own trait name (`TaskCRUD`) makes it ambiguous.
    pub(crate) fn two_orbital_schema_json(second_trait: &str) -> String {
        match second_trait {
            "ChoreCRUD" => crate::resolved_fixture("two-orbital-chore"),
            "TaskCRUD" => crate::resolved_fixture("two-orbital-task"),
            other => panic!("no resolved two-orbital fixture for trait {other}"),
        }
    }

    unsafe fn init_two_orbital_handle(second_trait: &str) -> *mut OrbitalKernelHandle {
        let schema_json = cstr(&two_orbital_schema_json(second_trait));
        let client_id = cstr("test-client");
        orbital_kernel_init(schema_json.as_ptr(), client_id.as_ptr())
    }

    unsafe fn dispatch_json(
        handle: *mut OrbitalKernelHandle,
        trait_name: &str,
        event: &str,
    ) -> serde_json::Value {
        let t = cstr(trait_name);
        let e = cstr(event);
        let out = orbital_kernel_dispatch(handle, t.as_ptr(), ptr::null(), e.as_ptr(), ptr::null());
        assert!(!out.is_null());
        let wire = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        wire
    }

    #[test]
    fn dispatch_wire_carries_the_owning_orbital_of_a_second_orbital_trait() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            assert!(!handle.is_null());
            let second = dispatch_json(handle, "ChoreCRUD", "INIT");
            assert_eq!(second["orbital"], SECOND_ORBITAL, "{second}");
            // Control: a first-orbital trait still reports the first orbital.
            let first = dispatch_json(handle, "TaskCRUD", "INIT");
            assert_eq!(first["orbital"], FIRST_ORBITAL, "{first}");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn unknown_trait_is_an_error_naming_the_trait() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            let wire = dispatch_json(handle, "NoSuchTrait", "INIT");
            assert_eq!(wire["error"], "unknown trait: NoSuchTrait", "{wire}");
            // Control: a declared trait on the same handle dispatches.
            assert!(dispatch_json(handle, "ChoreCRUD", "INIT")
                .get("error")
                .is_none());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn trait_declared_by_two_orbitals_is_an_error_listing_both_and_never_picked() {
        unsafe {
            let handle = init_two_orbital_handle("TaskCRUD");
            assert!(!handle.is_null());
            let wire = dispatch_json(handle, "TaskCRUD", "INIT");
            let err = wire["error"].as_str().unwrap_or_else(|| panic!("{wire}"));
            assert!(
                err.contains(FIRST_ORBITAL) && err.contains(SECOND_ORBITAL),
                "{err}"
            );
            assert!(err.contains("ambiguous"), "{err}");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn snapshot_lists_traits_of_every_orbital_and_reports_ambiguous_ones() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            let snap = orbital_kernel_snapshot(handle);
            let v: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(snap).to_str().unwrap()).unwrap();
            orbital_kernel_free_string(snap);
            assert!(v["states"].get("TaskCRUD").is_some(), "{v}");
            assert!(v["states"].get("ChoreCRUD").is_some(), "{v}");
            assert_eq!(v["ambiguousTraits"], serde_json::json!([]));
            orbital_kernel_destroy(handle);

            let handle = init_two_orbital_handle("TaskCRUD");
            let snap = orbital_kernel_snapshot(handle);
            let v: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(snap).to_str().unwrap()).unwrap();
            orbital_kernel_free_string(snap);
            assert!(
                v["states"].get("TaskCRUD").is_none(),
                "never overwritten: {v}"
            );
            assert_eq!(v["ambiguousTraits"][0]["trait"], "TaskCRUD");
            assert_eq!(
                v["ambiguousTraits"][0]["orbitals"],
                serde_json::json!([SECOND_ORBITAL, FIRST_ORBITAL])
            );
            orbital_kernel_destroy(handle);
        }
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn init_handle() -> *mut OrbitalKernelHandle {
        let schema_json = cstr(&resolved_schema_json());
        let client_id = cstr("test-client");
        orbital_kernel_init(schema_json.as_ptr(), client_id.as_ptr())
    }

    #[test]
    fn init_destroy_round_trip() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn init_null_args_returns_null() {
        unsafe {
            assert!(orbital_kernel_init(ptr::null(), ptr::null()).is_null());
        }
    }

    #[test]
    fn dispatch_null_handle_returns_null() {
        unsafe {
            let t = cstr("Trait");
            let e = cstr("EVENT");
            assert!(orbital_kernel_dispatch(
                ptr::null_mut(),
                t.as_ptr(),
                ptr::null(),
                e.as_ptr(),
                ptr::null()
            )
            .is_null());
        }
    }

    #[test]
    fn dispatch_unknown_trait_reports_error_json_and_last_error() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let t = cstr("NoSuchTrait");
            let e = cstr("EVENT");
            let out =
                orbital_kernel_dispatch(handle, t.as_ptr(), ptr::null(), e.as_ptr(), ptr::null());
            assert!(!out.is_null());
            let s = CStr::from_ptr(out).to_str().unwrap().to_string();
            assert!(s.contains("\"error\""), "expected error json, got {s}");
            orbital_kernel_free_string(out);

            let err = orbital_kernel_last_error(handle);
            let err_s = CStr::from_ptr(err).to_str().unwrap().to_string();
            assert!(!err_s.is_empty());
            orbital_kernel_free_string(err);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn seed_get_state_snapshot_round_trip() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());

            let entity_type = cstr("Task");
            let rows = cstr(r#"[{"id":"n1","title":"hello"}]"#);
            orbital_kernel_seed(handle, entity_type.as_ptr(), rows.as_ptr());

            let snap = orbital_kernel_snapshot(handle);
            assert!(!snap.is_null());
            let snap_s = CStr::from_ptr(snap).to_str().unwrap().to_string();
            assert!(snap_s.contains("\"states\""));
            orbital_kernel_free_string(snap);

            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn snapshot_carries_the_canvas_theme_of_the_resolved_theme_and_follows_the_mode() {
        unsafe {
            let handle = init_handle();
            let read = |h| {
                let snap = orbital_kernel_snapshot(h);
                let v: serde_json::Value =
                    serde_json::from_str(CStr::from_ptr(snap).to_str().unwrap()).unwrap();
                orbital_kernel_free_string(snap);
                v
            };
            let light = read(handle);
            let tokens: orbital_core::schema::ThemeTokens =
                serde_json::from_value(light["theme"]["tokens"].clone()).unwrap();
            let expected = orbital_core::runtime::canvas_theme::canvas_theme(&tokens);
            assert_eq!(
                light["canvasTheme"],
                serde_json::to_value(&expected).unwrap()
            );

            let dark = cstr("dark");
            orbital_kernel_set_theme_mode(handle, dark.as_ptr());
            let dark_snap = read(handle);
            assert_ne!(
                dark_snap["canvasTheme"]["ground"],
                light["canvasTheme"]["ground"]
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn snapshot_carries_the_resolved_motion_of_the_theme() {
        unsafe {
            let handle = init_handle();
            let snap = orbital_kernel_snapshot(handle);
            let v: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(snap).to_str().unwrap()).unwrap();
            orbital_kernel_free_string(snap);
            let tokens: orbital_core::schema::ThemeTokens =
                serde_json::from_value(v["theme"]["tokens"].clone()).unwrap();
            let expected = orbital_core::runtime::motion::resolve_motion(&tokens);
            assert_eq!(v["motion"], serde_json::to_value(&expected).unwrap());
            assert!(v["motion"]["shapes"]["modalEnter"]["scale"].is_number());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn set_current_user_then_get_state_does_not_error() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let user = cstr(r#"{"id":"u1","role":"member"}"#);
            orbital_kernel_set_current_user(handle, user.as_ptr());

            let trait_name = cstr("NoSuchTrait");
            let state = orbital_kernel_get_state(handle, trait_name.as_ptr(), ptr::null());
            let state_s = CStr::from_ptr(state).to_str().unwrap().to_string();
            assert_eq!(state_s, "");
            orbital_kernel_free_string(state);
            orbital_kernel_destroy(handle);
        }
    }

    unsafe fn init_rollback_handle() -> *mut OrbitalKernelHandle {
        let schema_json = cstr(&crate::resolved_fixture("rollback"));
        let client_id = cstr("test-client");
        orbital_kernel_init(schema_json.as_ptr(), client_id.as_ptr())
    }

    /// The `content` of every render the response paints for `trait_name`.
    fn painted(response: &serde_json::Value, trait_name: &str) -> Vec<f64> {
        response["clientEffectsByTrait"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["traitName"] == trait_name)
            .filter_map(|e| {
                e["effect"]
                    .as_array()
                    .and_then(|a| a.get(2))
                    .and_then(|p| p["content"].as_f64())
            })
            .collect()
    }

    #[test]
    fn restore_returns_the_repaint_it_owes_bound_siblings() {
        unsafe {
            let handle = init_rollback_handle();
            assert!(
                !handle.is_null(),
                "{}",
                CStr::from_ptr(orbital_kernel_last_error(ptr::null_mut())).to_string_lossy()
            );
            dispatch_json(handle, "Driver", "INIT");
            dispatch_json(handle, "Hud", "INIT");
            let before = (*handle).embedded.kernel().snapshot_trait("Driver", None);
            let bumped = dispatch_json(handle, "Driver", "BUMP");
            assert_eq!(painted(&bumped["response"], "Hud"), vec![1.0], "{bumped}");
            let t = cstr("Driver");
            let s = cstr(&serde_json::to_string(&before).unwrap());
            let out = orbital_kernel_restore(handle, t.as_ptr(), ptr::null(), s.as_ptr());
            assert!(!out.is_null());
            let restored: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
            orbital_kernel_free_string(out);
            assert_eq!(
                painted(&restored, "Hud"),
                vec![0.0],
                "the restored value: {restored}"
            );
            let again = orbital_kernel_restore(handle, t.as_ptr(), ptr::null(), s.as_ptr());
            let again_json: serde_json::Value =
                serde_json::from_str(CStr::from_ptr(again).to_str().unwrap()).unwrap();
            orbital_kernel_free_string(again);
            assert!(
                painted(&again_json, "Hud").is_empty(),
                "control: nothing changed, nothing repaints"
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn restore_and_tick_null_handle_are_safe_no_ops() {
        unsafe {
            let t = cstr("Trait");
            let s = cstr("{}");
            assert!(
                orbital_kernel_restore(ptr::null_mut(), t.as_ptr(), ptr::null(), s.as_ptr())
                    .is_null()
            );
            assert!(
                orbital_kernel_tick(ptr::null_mut(), t.as_ptr(), t.as_ptr(), ptr::null()).is_null()
            );
        }
    }

    #[test]
    fn snapshot_includes_resolved_theme() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let snap = orbital_kernel_snapshot(handle);
            assert!(!snap.is_null());
            let snap_s = CStr::from_ptr(snap).to_str().unwrap().to_string();
            assert!(
                snap_s.contains("\"theme\""),
                "expected theme in snapshot, got {snap_s}"
            );
            assert!(
                snap_s.contains("\"mode\":\"light\""),
                "expected default light mode, got {snap_s}"
            );
            orbital_kernel_free_string(snap);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn set_theme_mode_flips_snapshot_mode() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let dark = cstr("dark");
            orbital_kernel_set_theme_mode(handle, dark.as_ptr());
            let snap = orbital_kernel_snapshot(handle);
            let snap_s = CStr::from_ptr(snap).to_str().unwrap().to_string();
            assert!(
                snap_s.contains("\"mode\":\"dark\""),
                "expected dark mode after toggle, got {snap_s}"
            );
            orbital_kernel_free_string(snap);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn set_theme_mode_invalid_sets_last_error() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let bogus = cstr("sepia");
            orbital_kernel_set_theme_mode(handle, bogus.as_ptr());
            let err = orbital_kernel_last_error(handle);
            let err_s = CStr::from_ptr(err).to_str().unwrap().to_string();
            assert!(err_s.contains("unknown theme mode"), "got {err_s}");
            orbital_kernel_free_string(err);
            orbital_kernel_destroy(handle);
        }
    }

    /// The `_awaitingInit` sidecar on the server leg of `trait_name`'s `INIT`.
    unsafe fn awaiting_on_init_leg(
        handle: *mut OrbitalKernelHandle,
        trait_name: &str,
    ) -> Vec<String> {
        let t = cstr(trait_name);
        let e = cstr("INIT");
        let out = orbital_kernel_dispatch(handle, t.as_ptr(), ptr::null(), e.as_ptr(), ptr::null());
        assert!(!out.is_null());
        let wire: serde_json::Value =
            serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        let mut awaiting: Vec<String> = wire["serverLegs"][0]["leg"]["payload"]["_awaitingInit"]
            .as_array()
            .unwrap_or_else(|| panic!("INIT leg carries no _awaitingInit: {wire}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        awaiting.sort();
        awaiting
    }

    #[test]
    fn remount_declares_the_mount_the_server_leg_carries() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            let traits = cstr(r#"["TaskCRUD","ChoreCRUD"]"#);
            orbital_kernel_remount(handle, traits.as_ptr());
            // TaskCRUD's own INIT leaves the mount; ChoreCRUD (which handles INIT) still awaits.
            assert_eq!(
                awaiting_on_init_leg(handle, "TaskCRUD"),
                vec!["ChoreCRUD".to_string()]
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn control_without_remount_nothing_awaits() {
        unsafe {
            let handle = init_handle();
            assert!(awaiting_on_init_leg(handle, "TaskCRUD").is_empty());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn remount_replaces_the_previous_mount() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            let first = cstr(r#"["ChoreCRUD"]"#);
            orbital_kernel_remount(handle, first.as_ptr());
            let empty = cstr("[]");
            orbital_kernel_remount(handle, empty.as_ptr());
            assert!(awaiting_on_init_leg(handle, "TaskCRUD").is_empty());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn remount_rejects_non_string_array_and_keeps_the_mount() {
        unsafe {
            let handle = init_two_orbital_handle("ChoreCRUD");
            let good = cstr(r#"["ChoreCRUD"]"#);
            orbital_kernel_remount(handle, good.as_ptr());
            for bad in [r#"{"traits":[]}"#, r#"["ChoreCRUD", 3]"#, "not json"] {
                let bad = cstr(bad);
                orbital_kernel_remount(handle, bad.as_ptr());
                let err = orbital_kernel_last_error(handle);
                let err_s = CStr::from_ptr(err).to_str().unwrap().to_string();
                orbital_kernel_free_string(err);
                assert!(err_s.contains("traits_json"), "got {err_s}");
            }
            assert_eq!(
                awaiting_on_init_leg(handle, "TaskCRUD"),
                vec!["ChoreCRUD".to_string()]
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn remount_null_handle_and_null_traits_are_safe() {
        unsafe {
            let traits = cstr("[]");
            orbital_kernel_remount(ptr::null_mut(), traits.as_ptr());
            let handle = init_handle();
            orbital_kernel_remount(handle, ptr::null());
            let err = orbital_kernel_last_error(handle);
            assert!(!CStr::from_ptr(err).to_str().unwrap().is_empty());
            orbital_kernel_free_string(err);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn apply_server_response_parses_and_applies() {
        unsafe {
            let handle = init_handle();
            assert!(!handle.is_null());
            let response = cstr(
                r#"{"success":true,"transitioned":true,"states":{"NoSuchTrait":"done"},"emittedEvents":[]}"#,
            );
            let already = cstr("[]");
            let out =
                orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
            assert!(!out.is_null());
            let s = CStr::from_ptr(out).to_str().unwrap().to_string();
            assert!(s.contains("\"applied\""), "expected applied json, got {s}");
            orbital_kernel_free_string(out);
            orbital_kernel_destroy(handle);
        }
    }

    fn browse_schema_json(render_on_loaded: bool) -> String {
        let loaded_effects = if render_on_loaded {
            serde_json::json!([["render-ui", "main", { "type": "stack", "children": [] }]])
        } else {
            serde_json::json!([])
        };
        serde_json::json!({
            "name": "App",
            "typeContractVersion": 1,
            "orbitals": [{
                "name": "Main",
                "entity": { "name": "Item", "fields": [{ "name": "id", "type": "string" }] },
                "traits": [{
                    "name": "Browse",
                    "linkedEntity": "Item",
                    "stateMachine": {
                        "states": [
                            { "name": "loading", "isInitial": true },
                            { "name": "browsing" }
                        ],
                        "transitions": [
                            { "from": "loading", "to": "browsing", "event": "Loaded",
                              "effects": loaded_effects }
                        ]
                    }
                }]
            }]
        })
        .to_string()
    }

    unsafe fn apply_loaded(render_on_loaded: bool) -> serde_json::Value {
        let schema_json = cstr(&browse_schema_json(render_on_loaded));
        let client_id = cstr("test-client");
        let handle = orbital_kernel_init(schema_json.as_ptr(), client_id.as_ptr());
        assert!(!handle.is_null());
        let response = cstr(
            r#"{"success":true,"transitioned":true,"states":{"Browse":"loading"},"emittedEvents":[{"event":"Loaded","payload":{"data":[]},"source":{"trait":"Browse","orbital":"Main"}}]}"#,
        );
        let already = cstr("[]");
        let out = orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
        assert!(!out.is_null());
        let wire = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        orbital_kernel_destroy(handle);
        wire
    }

    #[test]
    fn apply_wire_carries_the_outcomes_render_ui_effect() {
        let wire = unsafe { apply_loaded(true) };
        assert_eq!(wire["applied"], serde_json::json!([["Browse", "browsing"]]));
        let effects = wire["response"]["clientEffectsByTrait"]
            .as_array()
            .unwrap_or_else(|| panic!("clientEffectsByTrait missing: {wire}"));
        assert_eq!(effects.len(), 1, "{wire}");
        assert_eq!(effects[0]["effect"][0], "render-ui");
        assert_eq!(effects[0]["effect"][1], "main");
        assert_eq!(
            wire["response"]["clientEffectsByTrait"][0]["traitName"],
            "Browse"
        );
        assert_eq!(wire["response"]["states"]["Browse"], "browsing");
    }

    #[test]
    fn apply_wire_of_an_effectless_outcome_carries_no_client_effects() {
        let wire = unsafe { apply_loaded(false) };
        assert_eq!(wire["applied"], serde_json::json!([["Browse", "browsing"]]));
        let effects = wire["response"].get("clientEffectsByTrait");
        assert!(
            effects.map_or(true, |e| e.is_null()
                || e.as_array().is_some_and(Vec::is_empty)),
            "{wire}"
        );
    }

    unsafe fn dispatch_with_payload(
        handle: *mut OrbitalKernelHandle,
        event: &str,
        payload: &str,
    ) -> serde_json::Value {
        let t = cstr("TaskCRUD");
        let e = cstr(event);
        let p = cstr(payload);
        let out = orbital_kernel_dispatch(handle, t.as_ptr(), ptr::null(), e.as_ptr(), p.as_ptr());
        assert!(!out.is_null());
        let wire = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        wire
    }

    fn two_orbital_listener_schema_json() -> String {
        serde_json::json!({
            "name": "App",
            "typeContractVersion": 1,
            "orbitals": [
                {
                    "name": "Browsing",
                    "entity": { "name": "Note", "fields": [{ "name": "id", "type": "string" }] },
                    "traits": [{
                        "name": "Browse", "linkedEntity": "Note", "dispatch": "client",
                        "stateMachine": {
                            "states": [{ "name": "ready", "isInitial": true }],
                            "transitions": [{ "from": "ready", "to": "ready", "event": "GO",
                                "effects": [["emit", "PING", {}]] }]
                        }
                    }]
                },
                {
                    "name": "Storing",
                    "entity": { "name": "Archive", "fields": [{ "name": "id", "type": "string" }] },
                    "traits": [{
                        "name": "Archiver", "linkedEntity": "Archive",
                        "listens": [{ "event": "PING", "triggers": "STORE", "source": { "kind": "orbital", "orbital": "Browsing", "trait": "Browse" } }],
                        "stateMachine": {
                            "states": [{ "name": "ready", "isInitial": true }],
                            "transitions": [{ "from": "ready", "to": "ready", "event": "STORE",
                                "effects": [["persist", "create", "Archive", {}]] }]
                        }
                    }]
                }
            ]
        })
        .to_string()
    }

    unsafe fn dispatch_on(
        handle: *mut OrbitalKernelHandle,
        trait_name: &str,
        event: &str,
    ) -> serde_json::Value {
        let t = cstr(trait_name);
        let e = cstr(event);
        let out = orbital_kernel_dispatch(handle, t.as_ptr(), ptr::null(), e.as_ptr(), ptr::null());
        assert!(!out.is_null());
        let wire = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        wire
    }

    #[test]
    fn a_leg_addressed_to_a_listener_names_the_orbital_that_owns_it() {
        unsafe {
            let schema = cstr(&two_orbital_listener_schema_json());
            let client = cstr("test-client");
            let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
            assert!(!handle.is_null());
            let wire = dispatch_on(handle, "Browse", "GO");
            assert_eq!(wire["orbital"], "Browsing", "{wire}");
            assert_eq!(
                (
                    wire["seed"]["trait"].as_str(),
                    wire["seed"]["event"].as_str()
                ),
                (Some("Browse"), Some("GO")),
                "a leg failure is reported against the dispatch event: {wire}"
            );
            let legs = wire["serverLegs"].as_array().unwrap();
            assert_eq!(legs.len(), 1, "{wire}");
            assert_eq!(
                legs[0]["orbital"], "Storing",
                "the leg posts to the listener's own route: {wire}"
            );
            assert_eq!(legs[0]["leg"]["targetTrait"], "Archiver");
            assert_eq!(legs[0]["leg"]["event"], "STORE");
            assert_eq!(
                wire["awaiting"],
                serde_json::json!([{ "trait": "Archiver", "event": "STORE", "from": "ready" }]),
                "the local seed never awaits; the listener it reached does: {wire}"
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn control_a_seed_that_needs_the_server_itself_posts_to_its_own_orbital() {
        unsafe {
            let handle = init_handle();
            let wire = dispatch_with_payload(handle, "INIT", "{}");
            assert_eq!(wire["serverLegs"][0]["orbital"], wire["orbital"], "{wire}");
            orbital_kernel_destroy(handle);
        }
    }

    const DELETE_PAYLOAD: &str = r#"{"row":{"id":"r1","title":"one"}}"#;
    const ITERATIONS: usize = 3000;

    fn leg_event(wire: &serde_json::Value) -> Option<String> {
        wire["serverLegs"][0]["leg"]["event"]
            .as_str()
            .map(str::to_string)
    }

    #[test]
    fn single_threaded_server_bound_dispatches_each_keep_their_own_leg() {
        unsafe {
            let handle = init_handle();
            for _ in 0..ITERATIONS {
                let init = dispatch_with_payload(handle, "INIT", "{}");
                assert_eq!(leg_event(&init).as_deref(), Some("INIT"), "{init}");
                let delete = dispatch_with_payload(handle, "DELETE", DELETE_PAYLOAD);
                assert_eq!(leg_event(&delete).as_deref(), Some("DELETE"), "{delete}");
            }
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn concurrent_server_bound_dispatches_on_one_handle_each_keep_their_own_leg() {
        unsafe {
            let handle = init_handle();
            let addr = handle as usize;
            let run = |event: &'static str, payload: &'static str| {
                std::thread::spawn(move || {
                    let handle = addr as *mut OrbitalKernelHandle;
                    let mut wrong: Vec<String> = Vec::new();
                    for _ in 0..ITERATIONS {
                        let wire = dispatch_with_payload(handle, event, payload);
                        if leg_event(&wire).as_deref() != Some(event) {
                            wrong.push(wire.to_string());
                        }
                    }
                    wrong
                })
            };
            let init = run("INIT", "{}");
            let delete = run("DELETE", DELETE_PAYLOAD);
            let wrong_init = init.join().unwrap();
            let wrong_delete = delete.join().unwrap();
            assert!(
                wrong_init.is_empty() && wrong_delete.is_empty(),
                "INIT lost its leg {} times, DELETE {} times",
                wrong_init.len(),
                wrong_delete.len()
            );
            orbital_kernel_destroy(handle);
        }
    }
}

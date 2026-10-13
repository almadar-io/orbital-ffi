//! JNI bridge: real marshaling from the `Java_generated_patterns_core_OrbitalKernelBridge_*`
//! name-mangled symbols the Kotlin `external fun` declarations in
//! `OrbitalKernelBridge.kt` expect, onto [`crate::kernel_c_api`]'s
//! `orbital_kernel_*` C ABI — the JNI twin of `jni_bridge.rs` (which does the
//! same for the renderer C ABI). No kernel logic lives here: every fn below
//! marshals its `JString` args into `CString`s via [`crate::jni_util`], calls
//! straight through to `kernel_c_api`, and marshals the C string/pointer
//! result back into a `jstring`/`jlong`.
//!
//! A `jstring` is an opaque JNI object reference, never a `*const c_char` —
//! `CStr::from_ptr` on one is undefined behavior. Every fn's body also runs
//! under [`crate::jni_util::catch_panic`]: a panic must never unwind across
//! the JNI boundary into the JVM.

use crate::jni_util::{catch_panic, optional_cstring, required_cstring, take_c_string_as_jstring};
use crate::kernel_c_api::{
    orbital_kernel_abandon_server_leg, orbital_kernel_apply_server_response,
    orbital_kernel_destroy, orbital_kernel_dispatch, orbital_kernel_frame,
    orbital_kernel_frame_pause, orbital_kernel_frame_resume, orbital_kernel_free_string,
    orbital_kernel_get_state, orbital_kernel_init, orbital_kernel_last_error, orbital_kernel_mount,
    orbital_kernel_remount, orbital_kernel_restore, orbital_kernel_seed,
    orbital_kernel_set_current_user, orbital_kernel_set_theme_mode, orbital_kernel_snapshot,
    orbital_kernel_tick, orbital_kernel_tick_count, orbital_verify_bridge_folded,
    orbital_verify_bridge_painted, orbital_verify_bridge_set_dispatch_callback,
    orbital_verify_bridge_set_ready, orbital_verify_bridge_set_route, orbital_verify_bridge_start,
    orbital_verify_bridge_stop, OrbitalKernelHandle,
};
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use std::ffi::{c_char, c_void, CStr};

/// The exported `Java_generated_patterns_core_OrbitalKernelBridge_*` symbols this module
/// defines, kept next to the definitions so the list cannot drift out of
/// sync (see `kernel_jni_symbols_match_declared_fns` below) and so the
/// iOS/Android bridge-emission tests can read it instead of hand-typing a
/// twin.
///
/// No `nativeFreeString`: every result here is copied into a `jstring` (a
/// JVM-owned, GC-managed object) and the C string freed internally via
/// `orbital_kernel_free_string` before returning — there is no raw C pointer
/// left for Kotlin to free.
pub const KERNEL_JNI_SYMBOLS: &[&str] = &[
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeInit",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeDestroy",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeDispatch",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeApplyServerResponse",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeRestore",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeAbandonServerLeg",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeTick",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeFrame",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeFramePause",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeFrameResume",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeTickCount",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeGetState",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeSnapshot",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeSetThemeMode",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeSetCurrentUser",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeSeed",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeRemount",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeMount",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeLastError",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeStart",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeStop",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetDispatchCallback",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetRoute",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetReady",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeFolded",
    "Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgePainted",
];

fn handle_of(handle: jlong) -> *mut OrbitalKernelHandle {
    handle as *mut OrbitalKernelHandle
}

/// JNI entry: `OrbitalKernelBridge.nativeInit(resolvedSchemaJson, clientId)`.
/// Returns `0` (Kotlin reads this as `null`) on a null/invalid arg, a schema
/// that fails to parse/register, or a panic — there is no handle yet to
/// report `lastError` through.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeInit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    resolved_schema_json: JString<'local>,
    client_id: JString<'local>,
) -> jlong {
    let outcome = catch_panic(|| {
        let schema = required_cstring(&mut env, &resolved_schema_json)?;
        let client = required_cstring(&mut env, &client_id)?;
        let handle = unsafe { orbital_kernel_init(schema.as_ptr(), client.as_ptr()) };
        Some(handle as jlong)
    });
    outcome.ok().flatten().unwrap_or(0)
}

/// JNI entry: `OrbitalKernelBridge.nativeDestroy(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeDestroy<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let _ = catch_panic(|| unsafe { orbital_kernel_destroy(handle_of(handle)) });
}

/// JNI entry: `OrbitalKernelBridge.nativeDispatch(handle, trait, entityId, event, payloadJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeDispatch<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    trait_name: JString<'local>,
    entity_id: JString<'local>,
    event: JString<'local>,
    payload_json: JString<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let trait_name = required_cstring(&mut env, &trait_name)?;
        let event = required_cstring(&mut env, &event)?;
        let entity_id = optional_cstring(&mut env, &entity_id);
        let payload_json = optional_cstring(&mut env, &payload_json);
        let raw = unsafe {
            orbital_kernel_dispatch(
                handle_of(handle),
                trait_name.as_ptr(),
                entity_id
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                event.as_ptr(),
                payload_json
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeApplyServerResponse(handle, responseJson, alreadyDeliveredJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeApplyServerResponse<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    response_json: JString<'local>,
    already_delivered_json: JString<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let response_json = required_cstring(&mut env, &response_json)?;
        let already_delivered_json = optional_cstring(&mut env, &already_delivered_json);
        let raw = unsafe {
            orbital_kernel_apply_server_response(
                handle_of(handle),
                response_json.as_ptr(),
                already_delivered_json
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeRestore(handle, trait, entityId, snapshotJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeRestore<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    trait_name: JString<'local>,
    entity_id: JString<'local>,
    snapshot_json: JString<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let trait_name = required_cstring(&mut env, &trait_name)?;
        let entity_id = optional_cstring(&mut env, &entity_id);
        let snapshot_json = required_cstring(&mut env, &snapshot_json)?;
        let raw = unsafe {
            orbital_kernel_restore(
                handle_of(handle),
                trait_name.as_ptr(),
                entity_id
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                snapshot_json.as_ptr(),
            )
        };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeAbandonServerLeg(handle, trait, event, error)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeAbandonServerLeg<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    trait_name: JString<'local>,
    event: JString<'local>,
    error: JString<'local>,
) {
    let _ = catch_panic(|| {
        let trait_name = optional_cstring(&mut env, &trait_name);
        let event = optional_cstring(&mut env, &event);
        let error = optional_cstring(&mut env, &error);
        unsafe {
            orbital_kernel_abandon_server_leg(
                handle_of(handle),
                trait_name
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                event.as_deref().map_or(std::ptr::null(), |c| c.as_ptr()),
                error.as_deref().map_or(std::ptr::null(), |c| c.as_ptr()),
            );
        }
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeTick(handle, trait, tick, entityId)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeTick<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    trait_name: JString<'local>,
    tick_name: JString<'local>,
    entity_id: JString<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let trait_name = required_cstring(&mut env, &trait_name)?;
        let tick_name = required_cstring(&mut env, &tick_name)?;
        let entity_id = optional_cstring(&mut env, &entity_id);
        let raw = unsafe {
            orbital_kernel_tick(
                handle_of(handle),
                trait_name.as_ptr(),
                tick_name.as_ptr(),
                entity_id
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeFrame(handle, nowMs)`. Null when no tick fired.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeFrame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    now_ms: jlong,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = unsafe { orbital_kernel_frame(handle_of(handle), now_ms.max(0) as u64) };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeFramePause(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeFramePause<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let _ = catch_panic(|| unsafe { orbital_kernel_frame_pause(handle_of(handle)) });
}

/// JNI entry: `OrbitalKernelBridge.nativeFrameResume(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeFrameResume<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let _ = catch_panic(|| unsafe { orbital_kernel_frame_resume(handle_of(handle)) });
}

/// JNI entry: `OrbitalKernelBridge.nativeTickCount(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeTickCount<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    catch_panic(|| unsafe { orbital_kernel_tick_count(handle_of(handle)) }).unwrap_or(0)
}

/// JNI entry: `OrbitalKernelBridge.nativeGetState(handle, trait, entityId)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeGetState<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    trait_name: JString<'local>,
    entity_id: JString<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let trait_name = required_cstring(&mut env, &trait_name)?;
        let entity_id = optional_cstring(&mut env, &entity_id);
        let raw = unsafe {
            orbital_kernel_get_state(
                handle_of(handle),
                trait_name.as_ptr(),
                entity_id
                    .as_deref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        Some(unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) })
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeSnapshot(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeSnapshot<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = unsafe { orbital_kernel_snapshot(handle_of(handle)) };
        unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) }
    });
    outcome.unwrap_or(std::ptr::null_mut())
}

/// JNI entry: `OrbitalKernelBridge.nativeSetThemeMode(handle, mode)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeSetThemeMode<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    mode: JString<'local>,
) {
    let _ = catch_panic(|| {
        let mode = required_cstring(&mut env, &mode)?;
        unsafe { orbital_kernel_set_theme_mode(handle_of(handle), mode.as_ptr()) };
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeSetCurrentUser(handle, userJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeSetCurrentUser<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    user_json: JString<'local>,
) {
    let _ = catch_panic(|| {
        let user_json = required_cstring(&mut env, &user_json)?;
        unsafe { orbital_kernel_set_current_user(handle_of(handle), user_json.as_ptr()) };
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeSeed(handle, entityType, rowsJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeSeed<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    entity_type: JString<'local>,
    rows_json: JString<'local>,
) {
    let _ = catch_panic(|| {
        let entity_type = required_cstring(&mut env, &entity_type)?;
        let rows_json = required_cstring(&mut env, &rows_json)?;
        unsafe { orbital_kernel_seed(handle_of(handle), entity_type.as_ptr(), rows_json.as_ptr()) };
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeRemount(handle, traitsJson)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeMount<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    traits_json: JString<'local>,
) {
    let _ = catch_panic(|| {
        let traits_json = required_cstring(&mut env, &traits_json)?;
        unsafe { orbital_kernel_mount(handle_of(handle), traits_json.as_ptr()) };
        Some(())
    });
}

#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeRemount<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    traits_json: JString<'local>,
) {
    let _ = catch_panic(|| {
        let traits_json = required_cstring(&mut env, &traits_json)?;
        unsafe { orbital_kernel_remount(handle_of(handle), traits_json.as_ptr()) };
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeLastError(handle)`. Always returns a
/// non-null `jstring` (`""` when nothing has failed) — `orbital_kernel_last_error`
/// never returns a null C string, matching Kotlin's non-optional `String`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeLastError<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = unsafe { orbital_kernel_last_error(handle_of(handle)) };
        unsafe { take_c_string_as_jstring(&mut env, raw, orbital_kernel_free_string) }
    });
    outcome.unwrap_or(std::ptr::null_mut())
}

/// What the C callback's `ctx` points at for a JNI-registered listener: the
/// VM to attach the bridge thread to and a global ref to the Kotlin
/// `OrbitalVerifyDispatchListener`. One is allocated per registration and
/// lives until process exit (a debug-only, once-per-launch registration).
struct JniDispatchListener {
    vm: JavaVM,
    target: GlobalRef,
}

fn optional_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }
            .to_str()
            .ok()
            .map(str::to_string)
    }
}

/// Bridge-thread trampoline: calls `OrbitalVerifyDispatchListener.onVerifyDispatch(traitScope, event, payloadJson)`.
extern "C" fn jni_verify_dispatch_trampoline(
    ctx: *mut c_void,
    trait_scope: *const c_char,
    event: *const c_char,
    payload_json: *const c_char,
) {
    let _ = catch_panic(|| {
        let listener = unsafe { &*(ctx as *const JniDispatchListener) };
        let mut env = listener.vm.attach_current_thread().ok()?;
        let scope = optional_str(trait_scope)
            .map(|s| env.new_string(s))
            .transpose()
            .ok()?;
        let event = env.new_string(optional_str(event)?).ok()?;
        let payload = optional_str(payload_json)
            .map(|s| env.new_string(s))
            .transpose()
            .ok()?;
        let scope = scope.map(JObject::from).unwrap_or_else(JObject::null);
        let payload = payload.map(JObject::from).unwrap_or_else(JObject::null);
        let call = env.call_method(
            &listener.target,
            "onVerifyDispatch",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                JValue::Object(&scope),
                JValue::Object(&event),
                JValue::Object(&payload),
            ],
        );
        if call.is_err() {
            let _ = env.exception_clear();
        }
        Some(())
    });
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeStart(handle, port)`. Returns the bound port, `0` on failure.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeStart<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    port: jint,
) -> jint {
    let Ok(port) = u16::try_from(port) else {
        return 0;
    };
    catch_panic(|| unsafe { orbital_verify_bridge_start(handle_of(handle), port) }).unwrap_or(0)
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeStop(handle)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeStop<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let _ = catch_panic(|| unsafe { orbital_verify_bridge_stop(handle_of(handle)) });
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeSetDispatchCallback(handle, listener)`.
/// A null `listener` clears the callback. Returns whether the native side stored it.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetDispatchCallback<
    'local,
>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    listener: JObject<'local>,
) -> bool {
    let stored = catch_panic(|| {
        if listener.is_null() {
            return Some(unsafe {
                orbital_verify_bridge_set_dispatch_callback(
                    handle_of(handle),
                    None,
                    std::ptr::null_mut(),
                )
            });
        }
        let vm = env.get_java_vm().ok()?;
        let target = env.new_global_ref(&listener).ok()?;
        let ctx = Box::into_raw(Box::new(JniDispatchListener { vm, target }));
        let stored = unsafe {
            orbital_verify_bridge_set_dispatch_callback(
                handle_of(handle),
                Some(jni_verify_dispatch_trampoline),
                ctx as *mut c_void,
            )
        };
        if stored == 0 {
            drop(unsafe { Box::from_raw(ctx) });
        }
        Some(stored)
    });
    matches!(stored, Ok(Some(1)))
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeSetRoute(handle, route)`.
/// A null `route` clears it. Returns whether the native side stored it.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetRoute<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    route: JString<'local>,
) -> bool {
    let stored = catch_panic(|| {
        let route = optional_cstring(&mut env, &route);
        Some(unsafe {
            orbital_verify_bridge_set_route(
                handle_of(handle),
                route.as_deref().map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        })
    });
    matches!(stored, Ok(Some(1)))
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeSetReady(handle, ready)`.
/// Returns whether the native side stored it.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeSetReady<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    ready: jboolean,
) -> bool {
    let stored = catch_panic(|| unsafe {
        orbital_verify_bridge_set_ready(handle_of(handle), i32::from(ready != 0))
    });
    matches!(stored, Ok(1))
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgeFolded(handle, generation)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgeFolded<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    generation: jlong,
) -> bool {
    let stored = catch_panic(|| unsafe {
        orbital_verify_bridge_folded(handle_of(handle), generation.max(0) as u64)
    });
    matches!(stored, Ok(1))
}

/// JNI entry: `OrbitalKernelBridge.nativeVerifyBridgePainted(handle, generation)`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalKernelBridge_nativeVerifyBridgePainted<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    generation: jlong,
) -> bool {
    let stored = catch_panic(|| unsafe {
        orbital_verify_bridge_painted(handle_of(handle), generation.max(0) as u64)
    });
    matches!(stored, Ok(1))
}

#[cfg(test)]
mod tests {
    use super::KERNEL_JNI_SYMBOLS;

    /// Guards against `KERNEL_JNI_SYMBOLS` drifting from the `fn` definitions
    /// above: every declared symbol must appear as an `fn <symbol>` in this
    /// file's own source.
    #[test]
    fn jni_symbols_match_declared_fns() {
        let src = include_str!("jni_kernel_bridge.rs");
        for sym in KERNEL_JNI_SYMBOLS {
            let needle = format!("fn {sym}<");
            assert!(
                src.contains(&needle),
                "KERNEL_JNI_SYMBOLS lists {sym:?} but no `fn {sym}<` found in jni_kernel_bridge.rs"
            );
        }
    }

    /// Every `native*` Kotlin call this file forwards must have a matching
    /// C ABI function in `kernel_c_api::KERNEL_C_SYMBOLS` (name after the
    /// `orbital_kernel_` prefix, camelCase → the JNI `native<Camel>` suffix).
    /// `orbital_kernel_free_string` is the one exempt C symbol: this bridge
    /// frees every C string internally (`take_c_string_as_jstring`) right
    /// after copying it into a `jstring`, so there is no JNI-exposed
    /// `nativeFreeString` for Kotlin to call.
    #[test]
    fn every_jni_symbol_forwards_a_c_abi_symbol() {
        let c_symbols = crate::kernel_c_api::KERNEL_C_SYMBOLS;
        // orbital_kernel_apply_server_response -> nativeApplyServerResponse
        fn native_suffix(c_symbol: &str) -> String {
            let rest = c_symbol.strip_prefix("orbital_kernel_").unwrap();
            let mut out = String::from("native");
            for part in rest.split('_') {
                let mut chars = part.chars();
                if let Some(first) = chars.next() {
                    out.push(first.to_ascii_uppercase());
                    out.extend(chars);
                }
            }
            out
        }
        for c_symbol in c_symbols {
            if *c_symbol == "orbital_kernel_free_string" {
                continue;
            }
            let expected = format!(
                "Java_generated_patterns_core_OrbitalKernelBridge_{}",
                native_suffix(c_symbol)
            );
            assert!(
                KERNEL_JNI_SYMBOLS.contains(&expected.as_str()),
                "no JNI forwarder for C ABI symbol {c_symbol:?} (expected {expected:?})"
            );
        }
    }

    /// Every verify-bridge C ABI symbol has a JNI forwarder (`orbital_verify_bridge_start` ->
    /// `nativeVerifyBridgeStart`).
    #[test]
    fn every_verify_bridge_c_symbol_has_a_jni_forwarder() {
        for c_symbol in crate::kernel_c_api::VERIFY_BRIDGE_C_SYMBOLS {
            let camel: String = c_symbol
                .strip_prefix("orbital_")
                .unwrap()
                .split('_')
                .map(|part| {
                    let mut chars = part.chars();
                    chars
                        .next()
                        .map(|f| f.to_ascii_uppercase().to_string() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect();
            let expected =
                format!("Java_generated_patterns_core_OrbitalKernelBridge_native{camel}");
            assert!(
                KERNEL_JNI_SYMBOLS.contains(&expected.as_str()),
                "no JNI forwarder {expected}"
            );
        }
    }
}

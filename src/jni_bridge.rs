//! JNI bridge: real marshaling from the `Java_generated_patterns_core_OrbitalGameBridge_*`
//! name-mangled symbols the Kotlin `external fun` declarations in
//! `OrbitalGameBridge.kt` expect, onto the `orbital-canvas` C ABI —
//! the JNI twin of `jni_kernel_bridge.rs`. No renderer logic lives here: a
//! JNI fn marshals its `JString` args into `CString`s via [`crate::jni_util`],
//! forwards to `orbital_canvas::c_api`, and marshals the result back
//! into a `jstring`/`jbyteArray`.
//!
//! A `jstring`/`jobject` is an opaque JNI reference, never a `*const c_char`
//! or a raw `ANativeWindow`-backing pointer — reading one as either is
//! undefined behavior. Every fn's body also runs under
//! [`crate::jni_util::catch_panic`]: a panic must never unwind across the
//! JNI boundary into the JVM.

use crate::jni_util::{catch_panic, optional_cstring, required_cstring, take_c_string_as_jstring};
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jbyteArray, jdouble, jfloat, jint, jlong, jstring};
use jni::JNIEnv;
use orbital_canvas::c_api::{
    orbital_canvas_add_font, orbital_canvas_asset_generation,
    orbital_canvas_destroy, orbital_canvas_free_rgba,
    orbital_canvas_free_string, orbital_canvas_init, orbital_canvas_input,
    orbital_canvas_last_error, orbital_canvas_render,
    orbital_canvas_render_scene_rgba, orbital_canvas_render_to_rgba,
    orbital_canvas_resize, orbital_canvas_scene_size,
    orbital_canvas_set_clock, orbital_canvas_set_motion,
    orbital_canvas_set_scale, orbital_canvas_set_scene, orbital_canvas_submit,
    orbital_canvas_take_events, orbital_canvas_take_missing_scripts,
    orbital_canvas_view_command, orbital_canvas_wants_frames,
    orbital_canvas_zoom,
};
use orbital_canvas::Canvas;
use std::ffi::c_void;

// `ANativeWindow_fromSurface` acquires an NDK reference to the window
// backing an `android.view.Surface`; `ANativeWindow_release` drops it. Only
// linked/declared on Android — the host build never calls these. `env` is
// the real `JNIEnv*` the NDK header declares (`jni::sys::JNIEnv`), not the
// crate's own `jni::JNIEnv` wrapper.
#[cfg(target_os = "android")]
#[link(name = "android")]
extern "C" {
    fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
}

/// Renderer pointer (as `usize`) → the `ANativeWindow*` acquired for it in
/// `nativeInit`, released in `nativeDestroy`. `platform::android::init_from_native_window`
/// only borrows the pointer to build the wgpu Vulkan surface — it does not
/// take or track an NDK reference of its own — so the bridge must hold the
/// acquired reference alive for the renderer's lifetime and release it
/// exactly once, after the renderer (and its surface) is torn down.
#[cfg(target_os = "android")]
fn native_windows() -> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
    static WINDOWS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    WINDOWS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The exported `Java_generated_patterns_core_OrbitalGameBridge_*` symbols this module
/// defines, kept next to the definitions so the list cannot drift out of
/// sync (see the `jni_symbols_match_declared_fns` test below).
///
/// No `nativeFreeString`/`nativeFreeRgba`: `nativeLastError` copies into a
/// `jstring` and `nativeRenderToRgba` copies into a `jbyteArray` (both
/// JVM-owned, GC-managed), freeing the C-side buffer internally before
/// returning — there is no raw C pointer left for Kotlin to free.
pub const JNI_SYMBOLS: &[&str] = &[
    "Java_generated_patterns_core_OrbitalGameBridge_nativeInit",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeDestroy",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeSubmit",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeSetScene",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeRender",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeResize",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeSetScale",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeInput",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeLastError",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeTakeEvents",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeTakeMissingScripts",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeAddFont",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeAssetGeneration",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeZoom",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeViewCommand",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeSceneAspect",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeSceneWidth",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeRenderSceneToRgba",
    "Java_generated_patterns_core_OrbitalGameBridge_nativeRenderToRgba",
];

fn renderer_of(ptr: jlong) -> *mut Canvas {
    ptr as *mut Canvas
}

/// JNI entry: create a renderer bound to `surface` (an `android.view.Surface`).
///
/// Called by Kotlin `OrbitalGameBridge.nativeInit(surface, w, h, url)`.
///
/// [`android.view.Surface`] has no public `ANativeWindow*` accessor, so on
/// Android this resolves `surface` via `ANativeWindow_fromSurface` (which
/// acquires an NDK reference) before handing the window pointer to
/// `orbital_canvas_init`; the acquired reference is tracked in
/// `native_windows` and released in `nativeDestroy`. On non-Android hosts
/// `orbital_canvas_init` ignores the window pointer entirely and
/// builds a headless renderer (mirrors the existing host-fallback branch in
/// `orbital-canvas::c_api::orbital_canvas_init`), so the host
/// build passes null rather than treating the JNI object reference as a raw
/// pointer.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeInit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    surface: JObject<'local>,
    width: jint,
    height: jint,
    url: JString<'local>,
) -> jlong {
    let outcome = catch_panic(|| {
        let url = optional_cstring(&mut env, &url);
        let url_ptr = url.as_deref().map_or(std::ptr::null(), |c| c.as_ptr());
        // Only read on Android (`ANativeWindow_fromSurface`); the host build
        // never touches the surface reference, so name it unconditionally to
        // avoid an unused-variable warning there.
        let _ = &surface;

        #[cfg(target_os = "android")]
        let window_ptr: *mut c_void = if surface.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { ANativeWindow_fromSurface(env.get_raw(), surface.as_raw()) }
        };
        #[cfg(not(target_os = "android"))]
        let window_ptr: *mut c_void = std::ptr::null_mut();

        let renderer = unsafe {
            orbital_canvas_init(
                window_ptr,
                width.max(0) as u32,
                height.max(0) as u32,
                url_ptr,
            )
        };

        #[cfg(target_os = "android")]
        {
            if window_ptr.is_null() {
                // nothing acquired, nothing to track
            } else if renderer.is_null() {
                // init failed after we acquired the reference — release it now,
                // there is no renderer to track it against.
                unsafe { ANativeWindow_release(window_ptr) };
            } else {
                native_windows()
                    .lock()
                    .unwrap()
                    .insert(renderer as usize, window_ptr as usize);
            }
        }

        renderer as jlong
    });
    outcome.unwrap_or(0)
}

/// JNI entry: destroy the renderer returned by `nativeInit`.
///
/// On Android this also releases the `ANativeWindow` reference `nativeInit`
/// acquired for this renderer, once the renderer (and its wgpu surface) has
/// been fully torn down.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeDestroy<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) {
    let _ = catch_panic(|| {
        let renderer = renderer_of(renderer_ptr);
        #[cfg(target_os = "android")]
        let window_ptr = native_windows()
            .lock()
            .unwrap()
            .remove(&(renderer as usize));

        unsafe { orbital_canvas_destroy(renderer) };

        #[cfg(target_os = "android")]
        if let Some(window_ptr) = window_ptr {
            unsafe { ANativeWindow_release(window_ptr as *mut c_void) };
        }
    });
}

/// JNI entry: submit a JSON `GameRenderPacket` to the renderer.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSubmit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    json_packet: JString<'local>,
) {
    let _ = catch_panic(|| {
        let json_packet = required_cstring(&mut env, &json_packet)?;
        unsafe { orbital_canvas_submit(renderer_of(renderer_ptr), json_packet.as_ptr()) };
        Some(())
    });
}

/// JNI entry: hand the renderer a `canvas` render-ui node (JSON); the Rust
/// mapper turns it into the packet. Returns 1 when accepted, 0 when refused
/// (reason via `nativeLastError`).
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSetScene<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    scene_json: JString<'local>,
) -> jint {
    let outcome = catch_panic(|| {
        let scene_json = required_cstring(&mut env, &scene_json)?;
        Some(unsafe {
            orbital_canvas_set_scene(renderer_of(renderer_ptr), scene_json.as_ptr())
        })
    });
    outcome.ok().flatten().unwrap_or(0)
}

/// JNI entry: render one frame to the bound surface.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeRender<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) {
    let _ = catch_panic(|| unsafe { orbital_canvas_render(renderer_of(renderer_ptr)) });
}

/// JNI entry: notify the renderer that the surface dimensions changed.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeResize<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    width: jint,
    height: jint,
) {
    let _ = catch_panic(|| unsafe {
        orbital_canvas_resize(
            renderer_of(renderer_ptr),
            width.max(0) as u32,
            height.max(0) as u32,
        )
    });
}

/// JNI entry: set the display density (`resources.displayMetrics.density`). Returns 1
/// when applied, 0 on a rejected value.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSetScale<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    scale: jfloat,
) -> jint {
    catch_panic(|| unsafe { orbital_canvas_set_scale(renderer_of(renderer_ptr), scale) })
        .unwrap_or(0)
}

/// JNI entry: hand the renderer the host frame clock in milliseconds (`withFrameNanos`).
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSetClock<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    ms: jdouble,
) {
    let _ =
        catch_panic(|| unsafe { orbital_canvas_set_clock(renderer_of(renderer_ptr), ms) });
}

/// JNI entry: allow (non-zero) or forbid (0) motion; the host passes the OS reduced-motion setting.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSetMotion<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    enabled: jint,
) {
    let _ = catch_panic(|| unsafe {
        orbital_canvas_set_motion(renderer_of(renderer_ptr), enabled)
    });
}

/// JNI entry: 1 while the host should keep drawing frames, 0 once a static scene has settled.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeWantsFrames<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) -> jint {
    catch_panic(|| unsafe { orbital_canvas_wants_frames(renderer_of(renderer_ptr)) })
        .unwrap_or(0)
}

/// JNI entry: forward a touch/key input event. Returns 1 if consumed.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeInput<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    event_type: JString<'local>,
    x: jfloat,
    y: jfloat,
    button: jint,
    key: JString<'local>,
) -> jint {
    let outcome = catch_panic(|| {
        let event_type = required_cstring(&mut env, &event_type)?;
        let key = optional_cstring(&mut env, &key);
        let consumed = unsafe {
            orbital_canvas_input(
                renderer_of(renderer_ptr),
                event_type.as_ptr(),
                x,
                y,
                button,
                key.as_deref().map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        Some(consumed)
    });
    outcome.ok().flatten().unwrap_or(0)
}

/// JNI entry: return the last renderer error as a `jstring` (`""` when
/// nothing has failed — `orbital_canvas_last_error` never returns a
/// null C string).
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeLastError<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = unsafe { orbital_canvas_last_error(renderer_of(renderer_ptr)) };
        unsafe { take_c_string_as_jstring(&mut env, raw, orbital_canvas_free_string) }
    });
    outcome.unwrap_or(std::ptr::null_mut())
}

/// JNI entry: the scene's width / height (logical units), or 0 when the scene cannot be
/// mapped (reason via the thread's last renderer error). Needs no renderer.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSceneAspect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    scene_json: JString<'local>,
) -> jfloat {
    let outcome = catch_panic(|| {
        let scene_json = required_cstring(&mut env, &scene_json)?;
        let (mut w, mut h) = (0.0f32, 0.0f32);
        let ok = unsafe { orbital_canvas_scene_size(scene_json.as_ptr(), &mut w, &mut h) };
        Some(if ok == 1 && h > 0.0 { w / h } else { 0.0 })
    });
    outcome.ok().flatten().unwrap_or(0.0)
}

/// JNI entry: the scene's own width in logical units, or 0 when the scene cannot be mapped.
/// Hosts cap a fixed-pixel pattern's surface at this instead of filling the column.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeSceneWidth<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    scene_json: JString<'local>,
) -> jfloat {
    let outcome = catch_panic(|| {
        let scene_json = required_cstring(&mut env, &scene_json)?;
        let (mut w, mut h) = (0.0f32, 0.0f32);
        let ok = unsafe { orbital_canvas_scene_size(scene_json.as_ptr(), &mut w, &mut h) };
        Some(if ok == 1 && h > 0.0 { w } else { 0.0 })
    });
    outcome.ok().flatten().unwrap_or(0.0)
}

/// JNI entry: map and render a scene once, headless, into an RGBA `jbyteArray` of
/// `width` x `height` physical pixels at density `scale`, or null when it cannot be drawn.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeRenderSceneToRgba<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    scene_json: JString<'local>,
    width: jint,
    height: jint,
    scale: jfloat,
) -> jbyteArray {
    let outcome = catch_panic(|| {
        let scene_json = required_cstring(&mut env, &scene_json)?;
        let mut out_pixels: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let ok = unsafe {
            orbital_canvas_render_scene_rgba(
                scene_json.as_ptr(),
                width.max(1) as u32,
                height.max(1) as u32,
                scale,
                &mut out_pixels,
                &mut out_len,
            )
        };
        if ok == 0 || out_pixels.is_null() {
            return None;
        }
        let copied = env
            .byte_array_from_slice(unsafe { std::slice::from_raw_parts(out_pixels, out_len) })
            .ok()
            .map(|array| array.into_raw());
        unsafe { orbital_canvas_free_rgba(out_pixels, out_len) };
        copied
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

/// JNI entry: take the scene events that taps queued since the last call, as a
/// JSON array string (`[]` when none) of `{"event", "payload"}`.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeTakeEvents<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = unsafe { orbital_canvas_take_events(renderer_of(renderer_ptr)) };
        unsafe { take_c_string_as_jstring(&mut env, raw, orbital_canvas_free_string) }
    });
    outcome.unwrap_or(std::ptr::null_mut())
}

/// JNI entry: the scripts painted text needed and no face covers, as a JSON array of
/// `{"script", "sample"}` (`[]` when none). The host answers with [`nativeAddFont`].
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeTakeMissingScripts<
    'local,
>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let outcome = catch_panic(|| {
        let raw = orbital_canvas_take_missing_scripts();
        unsafe { take_c_string_as_jstring(&mut env, raw, orbital_canvas_free_string) }
    });
    outcome.unwrap_or(std::ptr::null_mut())
}

/// JNI entry: register a system font file's bytes (face `index` of a collection) for `script`
/// (ISO 15924 short name).
/// Returns 1, or 0 when refused (reason via `nativeLastError`).
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeAddFont<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    script: JString<'local>,
    bytes: JByteArray<'local>,
    index: jint,
) -> jint {
    let outcome = catch_panic(|| {
        let script = required_cstring(&mut env, &script)?;
        let data = env.convert_byte_array(&bytes).ok()?;
        Some(unsafe {
            orbital_canvas_add_font(
                script.as_ptr(),
                data.as_ptr(),
                data.len(),
                index.max(0) as u32,
            )
        })
    });
    outcome.ok().flatten().unwrap_or(0)
}

/// JNI entry: the asset cache counter; a host re-measures canvas aspects when it moves.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeAssetGeneration<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jlong {
    orbital_canvas_asset_generation() as jlong
}

/// JNI entry: a pinch (`ScaleGestureDetector`) zooming the scene about its focus (logical points).
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeZoom<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    factor: jfloat,
    x: jfloat,
    y: jfloat,
) {
    let _ = catch_panic(|| unsafe {
        orbital_canvas_zoom(renderer_of(renderer_ptr), factor, x, y)
    });
}

/// JNI entry: a view button (`zoom-in` | `zoom-out` | `reset`); 1, or 0 with `nativeLastError` set.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeViewCommand<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
    command: JString<'local>,
) -> jint {
    let outcome = catch_panic(|| {
        let command = required_cstring(&mut env, &command)?;
        Some(unsafe {
            orbital_canvas_view_command(renderer_of(renderer_ptr), command.as_ptr())
        })
    });
    outcome.ok().flatten().unwrap_or(0)
}

/// JNI entry: headless RGBA render — used by the verifier / CI screenshot
/// path. Returns a `jbyteArray` copy of the current frame, or `null` when no
/// GPU adapter is available or no packet has been submitted.
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalGameBridge_nativeRenderToRgba<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    renderer_ptr: jlong,
) -> jbyteArray {
    let outcome = catch_panic(|| {
        let mut out_pixels: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let ok = unsafe {
            orbital_canvas_render_to_rgba(
                renderer_of(renderer_ptr),
                &mut out_pixels,
                &mut out_len,
            )
        };
        if ok == 0 || out_pixels.is_null() {
            return None;
        }
        let copied = env
            .byte_array_from_slice(unsafe { std::slice::from_raw_parts(out_pixels, out_len) })
            .ok()
            .map(|array| array.into_raw());
        unsafe { orbital_canvas_free_rgba(out_pixels, out_len) };
        copied
    });
    outcome.ok().flatten().unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::JNI_SYMBOLS;

    /// Guards against `JNI_SYMBOLS` drifting from the `fn` definitions above:
    /// every declared symbol must appear as an `fn <symbol>` in this file's
    /// own source.
    #[test]
    fn jni_symbols_match_declared_fns() {
        let src = include_str!("jni_bridge.rs");
        for sym in JNI_SYMBOLS {
            let needle = format!("fn {sym}<");
            assert!(
                src.contains(&needle),
                "JNI_SYMBOLS lists {sym:?} but no `fn {sym}<` found in jni_bridge.rs"
            );
        }
    }
}

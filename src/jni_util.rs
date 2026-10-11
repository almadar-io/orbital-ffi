//! Shared JNI marshaling for `jni_kernel_bridge.rs` and `jni_bridge.rs`: real
//! `JString <-> String` conversion (a `jstring` is an opaque JNI reference,
//! never a `*const c_char`) and the panic-safe call wrapper every exported
//! `Java_*` symbol wraps its body in — a panic must never unwind across the
//! JNI boundary into the JVM. One place so neither bridge hand-rolls either.
//!
//! Both bridges keep forwarding onto the crate's existing `*const c_char` C
//! ABI (`kernel_c_api`, `orbital_game_renderer::c_api`) instead of duplicating
//! kernel/renderer logic against `jni`'s object API: a JNI fn marshals its
//! `JString` args into owned `CString`s, calls the C ABI function, marshals
//! the `*mut c_char` result back into a `jstring` via [`take_c_string_as_jstring`],
//! which also frees the C string through the caller's own `free_string` — no
//! Kotlin-side `nativeFreeString` call needed for these results.

use jni::objects::JString;
use jni::sys::jstring;
use jni::JNIEnv;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Read a required JNI `JString` argument into an owned `CString`. `None` for
/// a null Java reference, a `get_string` failure (invalid modified-UTF-8), or
/// content carrying an embedded NUL (a `CString` cannot hold one).
pub fn required_cstring(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Option<CString> {
    if s.is_null() {
        return None;
    }
    let s: String = env.get_string(s).ok()?.into();
    CString::new(s).ok()
}

/// Read an optional JNI `JString` argument (Kotlin `String?`) into
/// `Option<CString>` — `None` for a null Java reference, distinct from `Some`
/// carrying an empty string.
pub fn optional_cstring(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Option<CString> {
    if s.is_null() {
        None
    } else {
        required_cstring(env, s)
    }
}

/// Build a Java `String` from a Rust `&str`; `null` (0) on allocation
/// failure, the same failure convention `CString::new`/`to_c_string` use on
/// the C ABI side.
pub fn string_to_jstring(env: &mut JNIEnv<'_>, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Read a `*mut c_char` result from a `*_free_string`-owning C ABI, copy it
/// into a Java `String`, and free the C-side allocation via `free_string` —
/// the one place that turns an owned C string into a `jstring` without ever
/// leaking it or handing the raw pointer to Kotlin. `ptr` null → Java `null`.
///
/// # Safety
/// `ptr` must be null or a pointer previously returned by the same C ABI
/// family `free_string` releases (`CString::into_raw`), and must not be read
/// or freed again after this call.
pub unsafe fn take_c_string_as_jstring(
    env: &mut JNIEnv<'_>,
    ptr: *mut c_char,
    free_string: unsafe extern "C" fn(*mut c_char),
) -> jstring {
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    let owned = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    free_string(ptr);
    string_to_jstring(env, &owned)
}

/// Run `f`, catching a panic and converting it to `Err(message)` — the JNI
/// boundary's twin of `kernel_c_api`'s `catch_unwind(AssertUnwindSafe(...))`.
/// Every exported `Java_*` fn wraps its whole body in this: an unhandled
/// panic must never unwind into the JVM's stack.
pub fn catch_panic<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    // `&payload` (a `&Box<dyn Any + Send>`) would coerce to `&dyn Any` by
    // treating the `Box` itself as the `Any` value (a blanket impl every
    // `Sized + 'static` type gets) instead of deref'ing to the payload
    // inside — every downcast then misses and falls through to "unknown
    // panic". `&*payload` derefs first, so the downcast sees the real
    // panic payload type (`&str`/`String`).
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| panic_message(&*payload))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `JString <-> String` conversion needs a live `JNIEnv` (a JVM invocation
    // is not available in `cargo test`), so only the pure helpers below are
    // unit-tested here; the symbol-drift tests in `jni_kernel_bridge.rs` /
    // `jni_bridge.rs` cover the marshaled fns end to end structurally, and
    // `make android-run`'s on-device proof covers them for real.

    #[test]
    fn catch_panic_returns_ok_value() {
        assert_eq!(catch_panic(|| 1 + 1), Ok(2));
    }

    #[test]
    fn catch_panic_captures_str_literal_panic_message() {
        let result = catch_panic(|| -> i32 { panic!("boom") });
        assert_eq!(result, Err("boom".to_string()));
    }

    #[test]
    fn catch_panic_captures_formatted_string_panic_message() {
        let result = catch_panic(|| -> i32 { panic!("boom {}", 42) });
        assert_eq!(result, Err("boom 42".to_string()));
    }

    #[test]
    fn panic_message_falls_back_for_unknown_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(payload.as_ref()), "unknown panic");
    }
}

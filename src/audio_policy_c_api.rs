use orbital_core::audio_policy::{plan_audio, plan_audio_unlock, AudioFrame};
use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn answer(previous: Option<&str>, next: &str, unlock: bool) -> String {
    let result = (|| -> Result<Value, String> {
        let next: AudioFrame = serde_json::from_str(next).map_err(|error| error.to_string())?;
        let actions = if unlock {
            plan_audio_unlock(&next)
        } else {
            let prior: Option<AudioFrame> = previous
                .map(|source| serde_json::from_str(source).map_err(|error| error.to_string()))
                .transpose()?;
            plan_audio(prior.as_ref(), &next)
        };
        Ok(json!({ "actions": actions }))
    })();
    match result {
        Ok(value) => value.to_string(),
        Err(error) => json!({ "error": error }).to_string(),
    }
}

unsafe fn read(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    CStr::from_ptr(pointer).to_str().ok().map(str::to_string)
}

fn c_answer(previous: *const c_char, next: *const c_char, unlock: bool) -> *mut c_char {
    let response = catch_unwind(AssertUnwindSafe(|| unsafe {
        let Some(next) = read(next) else {
            return json!({ "error": "next audio frame is required" }).to_string();
        };
        let prior = read(previous);
        answer(prior.as_deref(), &next, unlock)
    }))
    .unwrap_or_else(|_| json!({ "error": "audio policy panicked" }).to_string());
    CString::new(response)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "C" fn orbital_audio_plan(previous: *const c_char, next: *const c_char) -> *mut c_char {
    c_answer(previous, next, false)
}

#[no_mangle]
pub extern "C" fn orbital_audio_unlock(next: *const c_char) -> *mut c_char {
    c_answer(std::ptr::null(), next, true)
}

#[no_mangle]
pub unsafe extern "C" fn orbital_audio_free_string(pointer: *mut c_char) {
    if !pointer.is_null() {
        drop(CString::from_raw(pointer));
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalAudioPolicy_plan(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    previous: jni::objects::JString,
    next: jni::objects::JString,
) -> jni::sys::jstring {
    let next: String = match env.get_string(&next) {
        Ok(value) => value.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    let previous: Option<String> = if previous.is_null() {
        None
    } else {
        match env.get_string(&previous) {
            Ok(value) => Some(value.into()),
            Err(_) => return std::ptr::null_mut(),
        }
    };
    env.new_string(answer(previous.as_deref(), &next, false))
        .map(jni::objects::JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_generated_patterns_core_OrbitalAudioPolicy_unlock(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    next: jni::objects::JString,
) -> jni::sys::jstring {
    let next: String = match env.get_string(&next) {
        Ok(value) => value.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    env.new_string(answer(None, &next, true))
        .map(jni::objects::JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn c_boundary_returns_a_typed_audio_plan_and_frees_its_string() {
        let next =
            CString::new(r#"{"cue":"beep","cueSeq":0,"manifest":{"beep":{"path":"beep.wav"}}}"#)
                .unwrap();
        let response = orbital_audio_plan(std::ptr::null(), next.as_ptr());
        let value: Value =
            serde_json::from_str(unsafe { CStr::from_ptr(response).to_str().unwrap() }).unwrap();
        assert!(value["actions"].is_array());
        unsafe {
            orbital_audio_free_string(response);
        }
    }
}

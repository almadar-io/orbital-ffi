//! The one native library both mobile shells link: renderer C ABI + kernel
//! C ABI + JNI forwarders. Built as staticlib (iOS) and cdylib (Android).

pub use orbital_canvas::c_api;
pub mod jni_bridge;
pub mod jni_kernel_bridge;
pub mod jni_util;
pub mod kernel_c_api;
#[cfg(feature = "verify-bridge")]
pub mod verify_bridge;

#[cfg(test)]
mod frame_tests;

/// Resolved schemas the kernel tests boot from; `orbital-rust`'s
/// `ffi_resolved_fixtures` test regenerates and checks them.
#[cfg(test)]
pub(crate) fn resolved_fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/resolved")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

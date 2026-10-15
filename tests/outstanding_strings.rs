//! `orbital_kernel_outstanding_strings` counts every string a kernel call hands out until it is freed.
//! One test per binary: the counter is process-wide, so a parallel test would move it.

use orbital_ffi::kernel_c_api::*;
use std::ffi::CString;
use std::ptr;

#[test]
fn the_count_rises_with_each_result_and_returns_to_zero_when_freed() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/resolved/basic-entity.json");
    let schema = CString::new(std::fs::read_to_string(path).unwrap()).unwrap();
    let client = CString::new("outstanding").unwrap();
    unsafe {
        assert_eq!(
            orbital_kernel_outstanding_strings(),
            0,
            "control: nothing handed out yet"
        );
        let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(
            orbital_kernel_outstanding_strings(),
            0,
            "init returns a handle, not a string"
        );

        let snapshot = orbital_kernel_snapshot(handle);
        let missing_trait = CString::new("NoSuchTrait").unwrap();
        let event = CString::new("INIT").unwrap();
        let error = orbital_kernel_dispatch(
            handle,
            missing_trait.as_ptr(),
            ptr::null(),
            event.as_ptr(),
            ptr::null(),
        );
        assert!(!snapshot.is_null() && !error.is_null());
        assert_eq!(orbital_kernel_outstanding_strings(), 2);

        orbital_kernel_free_string(snapshot);
        assert_eq!(orbital_kernel_outstanding_strings(), 1);
        orbital_kernel_free_string(ptr::null_mut());
        assert_eq!(
            orbital_kernel_outstanding_strings(),
            1,
            "freeing null changes nothing"
        );
        orbital_kernel_free_string(error);
        assert_eq!(orbital_kernel_outstanding_strings(), 0);
        orbital_kernel_destroy(handle);
    }
}

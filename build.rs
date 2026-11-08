fn main() {
    println!("cargo:rerun-if-changed=native/apple_log.c");
    if std::env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple") {
        cc::Build::new()
            .file("native/apple_log.c")
            .compile("orbital_ffi_apple_log");
    }
}

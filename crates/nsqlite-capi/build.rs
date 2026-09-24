//! Builds the C test program and makes sure the cdylib the test loads is the one
//! that was just built.
//!
//! There is no C source in the crate's own build, so the only work here is
//! telling Cargo that the `.dll` under test is an output of this package. That
//! matters because `cargo test` builds the test binary without building the
//! cdylib, which would leave the test loading a stale copy from a previous run
//! and reporting a pass or a failure about code that is no longer there.
//!
//! The C test program itself is built by `tests/capi_test.rs`, which knows where
//! the compiler is and what to link against.

fn main() {
    // Re-run when the shim's own source changes, so a test run always loads a
    // library built from the code being tested.
    for file in ["src/lib.rs", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={file}");
    }
    println!("cargo:rerun-if-env-changed=NSQLITE_CAPI_SKIP_CDYLIB");
}

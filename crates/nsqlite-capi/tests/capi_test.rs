//! The C-side test: a real C program, compiled by the compiler on PATH, linked
//! against the shim's import library, and run.
//!
//! The C program is the only test here that can catch a signature the shim got
//! wrong. Rust would compile against whatever this file declares, so a Rust-only
//! test cannot: the point of `capi_test.c` is that it is compiled against the
//! real `sqlite3.h` prototypes, where a mismatch is a hard error.
//!
//! The build is skipped, with a printed note rather than a failure, when no
//! C compiler is on PATH -- a missing toolchain is not a defect in the shim.
//! Everything else in the crate runs regardless.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `sqlite3.h` lives, if it does. The shim is header-free by design, so
/// the test supplies the header; this is the MSYS2 ucrt64 tree the project's
/// tools come from.
fn sqlite3_include() -> Option<PathBuf> {
    let candidates = [
        "C:/Users/zyq/scoop/apps/msys2/current/ucrt64/include",
        "C:/msys64/ucrt64/include",
        "C:/msys64/mingw64/include",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.join("sqlite3.h").exists())
}

fn output_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("capi-ctest");
    std::fs::create_dir_all(&dir).expect("create the C test output directory");
    dir
}

/// The shim's library and its import library, in the profile cargo is building.
fn shim_artifacts() -> Option<(PathBuf, PathBuf)> {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join(profile);
    let dll = target.join("nsqlite_capi.dll");
    // MSVC puts the import library beside the DLL; a MinGW link would use
    // libnsqlite_capi.a, which this build does not produce.
    let import = target.join("nsqlite_capi.dll.lib");
    if dll.exists() && import.exists() {
        Some((dll, import))
    } else {
        None
    }
}

#[test]
fn the_c_program_compiles_links_and_passes() {
    let Some(include) = sqlite3_include() else {
        println!("skipping: no sqlite3.h found; the C test needs the SQLite headers");
        return;
    };
    let Some((dll, import)) = shim_artifacts() else {
        panic!(
            "the shim's cdylib is missing. Build it first: \
             `cargo build -p nsqlite-capi`. `cargo test` builds only the rlib, \
             and a C caller loads the cdylib, so running the tests alone would \
             link against whatever a previous build left behind."
        );
    };

    let out = output_dir();
    let exe = out.join(if cfg!(windows) {
        "capi_test.exe"
    } else {
        "capi_test"
    });
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("capi_test.c");

    // `cl` writes its diagnostics to stdout, not stderr, and the exit code alone
    // says nothing about what went wrong. Both are reported, because a build
    // that fails with an empty message is the hardest kind to act on.
    let compile = Command::new("cl")
        .current_dir(&out)
        .arg("/nologo")
        .arg(format!("/I{}", include.display()))
        .arg(&source)
        .arg(&import)
        .arg(format!("/Fe:{}", exe.display()))
        .output()
        .expect("cl is on PATH, or this test would have been skipped");

    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    assert!(
        compile.status.success(),
        "cl failed to build the C test:\n{diagnostics}"
    );

    // The DLL has to be findable at run time, so its directory goes on PATH.
    let dll_dir = dll.parent().expect("a DLL has a parent").to_owned();
    let run = Command::new(&exe)
        .current_dir(&out)
        .env(
            "PATH",
            format!(
                "{};{}",
                dll_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("the just-built executable runs");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "the C test failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("0 failures"),
        "the C test did not report a clean run:\n{stdout}"
    );
    println!("{stdout}");
}

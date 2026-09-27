//! A differential test over the second corpus, run through `tools/difftest2.sh`
//! against the real `sqlite3` and this engine, reporting every disagreement.
//!
//! # What this is for
//!
//! `differential.rs` and `tools/difftest.sh` already exist and work. This is not
//! a wider version of the same thing; it covers ground the first one cannot
//! reach, and the three gaps are structural rather than incidental.
//!
//! **A database state that is itself under test.** The brief asks what a
//! database looks like after being written and then read by the *other* engine.
//! `difftest.sh` drives one statement per shell invocation, so no file is ever
//! held across both engines. `difftest2.sh` has both engines write their own
//! copy of a body and then runs each read-back statement against the OTHER
//! engine's file. That is the only shape in which a file that is wrong only to
//! an outsider -- a schema that was never flushed, a declared type stored with
//! its quotes still attached -- is visible at all. Each engine agrees with
//! itself about such a file, so a self-comparison passes cleanly.
//!
//! **An error part way through a multi-row statement.** A runner that reads
//! only the final state sees the state both engines agree on *after* a
//! correctly rolled-back statement. A multi-row INSERT that applied the rows it
//! should have refused is therefore invisible to it, because the interesting
//! state is the one the statement did not leave. Each multi-row statement here
//! is one argument, and the row read-back on the next line is what sees it.
//!
//! **A repeated session.** Some behaviour only appears on the second
//! connection to a file, because the first connection is what wrote it. Each
//! section runs as a session and the read-back statements in it re-open the
//! same file.
//!
//! # What is compared
//!
//! Values, never printed text. The two shells format differently, and comparing
//! their renderings reports differences that are only about printing while
//! missing the ones that matter: NULL against the empty string, and a real
//! against an integer with the same digits.
//!
//! Where a statement can be rewritten, it is rewritten into a typed projection
//! the engine evaluates itself:
//!
//! ```text
//! hex(typeof(<expr>) || '~' || quote(<expr>))
//! ```
//!
//! `typeof` carries the storage class, `quote` separates NULL from `''` and
//! makes a real's rendering explicit, and the hex framing means no byte of a
//! value can be mistaken for a field or row boundary. `quote` is used rather
//! than the raw value because it is the engine's own deterministic rendering --
//! it was checked against the real engine across the awkward floats (1.0/3,
//! 0.1+0.2, 1e300, 1e-300, -0.0, 1.5e-8, 1e20, 1.0e+17) and agrees digit for
//! digit, so a disagreement there cannot be blamed on the comparison.
//!
//! Where a projection cannot express the query -- a derived table, which this
//! engine's parser refuses -- the statement is compared as each shell renders
//! it, and the run's summary says how many agreements were of that weaker kind.
//! A weak agreement cannot tell a blob from a text value of the same bytes, and
//! the summary never counts it as a strict pass.
//!
//! # The defect class
//!
//! The first corpus found four defects of one kind: the engine returned a WRONG
//! ANSWER rather than an error. DISTINCT was parsed and then ignored, a declared
//! type with a length was mis-parsed, an index was never built, a column name
//! was silently dropped. Nothing about those fails loudly, so that is what this
//! corpus is aimed at: precedence and associativity, storage-class boundaries,
//! the per-aggregate empty-set results, float rendering, and the state a
//! multi-row statement leaves behind.
//!
//! # Running it
//!
//! Both the `nsqlited` CLI and the real `sqlite3` are needed, so the test skips
//! rather than fails when either is missing -- a machine without them cannot
//! run a differential test, and that is not a defect in the engine.
//!
//! ```text
//! cargo test -p nsqlite --test differential2
//! NSQLITED=target/diff/debug/nsqlited.exe cargo test -p nsqlite --test differential2
//! DIFFTEST2_BIN=/path/to/difftest2.sh cargo test -p nsqlite --test differential2 -- --nocapture
//! DIFFTEST2_CASES=/path/to/other-cases.sql cargo test -p nsqlite --test differential2
//! DIFFTEST2_SECTION=12 cargo test -p nsqlite --test differential2 -- --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

// --- the same shell discovery the first differential test uses ---------------
//
// This is duplicated rather than shared because it is a test file and the two
// are separate integration targets: a shared module would have to live under
// src/ or be included by path, and this file is not allowed to reach outside
// its own two. The duplication is confined to shell discovery and path
// conversion, which are stable, and it is checked by the same probe the first
// test uses.

/// The runner script, overridable so a build outside `target/debug` can be
/// tested without editing this file.
fn difftest2_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST2_BIN") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest2.sh")
        .to_path_buf()
}

/// The corpus, overridable so the same runner can be pointed at another one.
fn case_file() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST2_CASES") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest2-cases.sql")
        .to_path_buf()
}

/// The shell that runs the runner script.
///
/// `bash` on the Windows PATH is often the WSL launcher, which cannot see a
/// `D:\` path and exits before the script starts. So no candidate is trusted:
/// each is *probed* with the runner's own question -- can it run a script from
/// a Windows drive path and print what the script prints -- and the first that
/// can wins. The probe sits beside the runner, on the repository's own drive,
/// because a `C:\` probe cannot see the difference between the MSYS families
/// over where `C:` is mounted, and cannot see the launcher failing at all.
fn difftest2_bash() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST_BASH") {
        return PathBuf::from(p);
    }
    let probe = probe_script_path();
    if std::fs::write(&probe, "printf 'nsqlite-bash-probe\\n'\n").is_err() {
        return PathBuf::from("bash");
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for c in bash_candidates() {
        if c.is_absolute() && !c.is_file() {
            continue;
        }
        let key = std::fs::canonicalize(&c).unwrap_or_else(|_| c.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if bash_runs_the_script(&c, &probe) {
            let _ = std::fs::remove_file(&probe);
            return c;
        }
    }
    let _ = std::fs::remove_file(&probe);
    PathBuf::from("bash")
}

fn bash_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["bash.exe", "bash"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    out.push(cand);
                }
            }
        }
    }
    out.push(PathBuf::from("bash"));
    if cfg!(windows) {
        for p in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\msys64\usr\bin\bash.exe",
            r"C:\cygwin64\bin\bin\bash.exe",
        ] {
            out.push(PathBuf::from(p));
        }
    }
    out
}

fn bash_runs_the_script(bash: &Path, probe: &Path) -> bool {
    let arg = windows_path_to_shell(probe);
    let Ok(out) = Command::new(bash).arg(&arg).output() else {
        return false;
    };
    out.status.success() && String::from_utf8_lossy(&out.stdout).contains("nsqlite-bash-probe")
}

fn probe_script_path() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("nsqlite-difftest2-bash-probe.sh")
}

/// `C:\dir\file` as `/c/dir/file`, for a shell that cannot take a drive path.
///
/// Not done with a bare `cygpath` from PATH: each MSYS family mounts a drive at
/// its own point, so one family's answer is not necessarily usable by the shell
/// that was selected. Every one of them agrees that `C:` is `/c/` and `D:` is
/// `/d/`, which is the form used here.
fn windows_path_to_shell(p: &Path) -> String {
    let raw = p.to_string_lossy().to_string();
    if cfg!(windows) {
        if raw.starts_with("\\\\?\\") {
            return raw;
        }
        let bytes = raw.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = raw[3..].replace('\\', "/");
            return format!("/{drive}/{rest}");
        }
    }
    raw
}

/// The nsqlite CLI to test. The default is the workspace debug build.
fn nsqlited() -> PathBuf {
    if let Ok(p) = std::env::var("NSQLITED") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/nsqlited.exe")
        .to_path_buf()
}

/// The real sqlite3, if it is on PATH.
fn find_sqlite3() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SQLITE3") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let dir = Path::new(&dir);
        for name in ["sqlite3.exe", "sqlite3"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// One section of the corpus, or the whole of it.
///
/// `DIFFTEST2_SECTION` narrows a run to a single section by number, which is
/// how a single reported disagreement is re-checked without paying for the
/// other sixty-odd. The numbering comes from the runner's own `--list`, so the
/// two cannot drift apart.
fn section_filter() -> Option<String> {
    std::env::var("DIFFTEST2_SECTION")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Runs the runner with `--self-test`, which needs neither engine.
///
/// The self-test is asked for explicitly rather than being inferred from
/// whether sqlite3 is present. Inferring it -- run the corpus when a real engine
/// is on PATH and the self-test otherwise -- means `the_runner_compares_strictly`
/// silently becomes a second copy of the whole corpus run whenever the machine
/// has sqlite3 installed, which is to say on every machine this is for. It then
/// takes minutes, it fails for reasons that have nothing to do with the runner's
/// text handling, and the failure message names a corpus disagreement where the
/// reader expected a self-test result.
fn run_self_test() -> (bool, String, String) {
    let bin = difftest2_bin();
    let bash = difftest2_bash();
    let out = Command::new(bash)
        .arg(windows_path_to_shell(&bin))
        .arg("--self-test")
        .output()
        .expect("running tools/difftest2.sh --self-test");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the corpus, or one section of it.
///
/// `DIFFTEST2_SECTION` narrows a run to a single section by number, which is how
/// one reported disagreement is re-checked without paying for the other sixty.
/// The numbering comes from the runner's own `--list`, so the two cannot drift
/// apart. The full run is slow by nature: it is a differential test across two
/// engines, and each statement is a process on each side.
fn run_corpus(extra: &[&str]) -> (bool, String, String) {
    let bin = difftest2_bin();
    let cases = case_file();
    let bash = difftest2_bash();
    let mut cmd = Command::new(bash);
    cmd.arg(windows_path_to_shell(&bin));
    cmd.arg(windows_path_to_shell(&cases));
    for a in extra {
        cmd.arg(a);
    }
    if let Ok(cli) = std::env::var("NSQLITED") {
        cmd.env("NSQLITED", windows_path_to_shell(Path::new(&cli)));
    } else {
        cmd.env("NSQLITED", windows_path_to_shell(&nsqlited()));
    }
    if let Some(real) = find_sqlite3() {
        cmd.env("SQLITE3", windows_path_to_shell(&real));
    }
    // Kept for this run, so the two databases can be opened afterwards to
    // attribute a disagreement to a file rather than to a rendering.
    let work = std::env::temp_dir().join(format!("nsqlite-difftest2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    cmd.env("WORKDIR", windows_path_to_shell(&work));
    let out = cmd.output().expect("running tools/difftest2.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Everything the runner found, summarised for a reader of the test output.
///
/// The list is a summary and not the deliverable: the deliverable is the
/// run's own report, which names the section, the reason, both engines' answers
/// and the statement for every disagreement. This exists so a `cargo test` line
/// says how bad it is without having to scroll the whole report.
fn summarise(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if t.starts_with("section:") || t.starts_with("reason:") || t.starts_with("statement:") {
            out.push(t.to_string());
        }
    }
    out
}

/// The corpus agrees with the real engine.
///
/// This is the test that fails when the two engines disagree, and it is
/// expected to fail right now: the corpus was written by running candidate
/// cases against the real engine and keeping the ones that exposed a
/// difference, so a passing run means those differences have been fixed.
#[test]
fn corpus_agrees_with_real_sqlite() {
    let bin = difftest2_bin();
    let cases = case_file();
    if find_sqlite3().is_none() {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return;
    }
    if !nsqlited().exists() {
        eprintln!(
            "skipping: {} does not exist; run `cargo build -p nsqlited` first",
            nsqlited().display()
        );
        return;
    }
    if !bin.exists() {
        eprintln!("skipping: {} does not exist", bin.display());
        return;
    }
    if !cases.exists() {
        panic!("the corpus {} does not exist", cases.display());
    }

    let mut extra: Vec<&str> = Vec::new();
    if let Some(s) = section_filter() {
        extra.push("--section");
        extra.push(Box::leak(s.into_boxed_str()));
    }
    let (ok, stdout, stderr) = run_corpus(&extra);
    if !stderr.is_empty() {
        eprintln!("difftest2 stderr:\n{stderr}");
    }
    println!("{stdout}");

    // A runner that printed no summary did not compare anything, and a test
    // that passes because it silently did nothing is worse than no test. This
    // catches a broken runner, a missing script, a case file the parser read as
    // zero sections, and a `bash` that is not bash.
    //
    // The line endings are normalised first, for the reason the self-test's
    // assertion does the same thing: Git Bash writes CRLF, and the summary line
    // is searched with a phrase that does not span a line ending, so it is
    // found either way -- but the `total` count below IS parsed out of the same
    // line and would carry a `\r` into the integer parse.
    let normalised: String = stdout.replace("\r\n", "\n");
    if !normalised.contains("statements agreed") {
        panic!(
            "the differential runner produced no summary, so nothing was compared.\n\
             cases: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            cases.display()
        );
    }
    // A run that compared fewer than a handful of statements has almost
    // certainly been narrowed to nothing, which looks identical to a pass.
    // The total is read out of the summary line, and the line is
    //     PASS: 5/5 statements agreed, 0 disagreed
    // so the count is `5/5` -- a numerator and a denominator -- not a bare
    // integer. Taking the last space-separated token before the comma yields the
    // EMPTY string, because the space before `statements` is itself a token
    // boundary, and the test then asserted `total > 0` against an empty string
    // and failed on a run that had passed. The numerator is taken explicitly.
    let total: usize = normalised
        .rsplit_once("statements agreed,")
        .and_then(|(head, _)| head.rsplit('/').next_back())
        .and_then(|s| {
            s.rsplit(|c: char| !c.is_ascii_digit())
                .find(|t| !t.is_empty())
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        total > 0,
        "the runner reported no statements compared; the corpus parsed as nothing.\n\
         cases: {}\nstdout:\n{stdout}",
        cases.display()
    );

    if !ok {
        let listed = summarise(&stdout);
        let mut msg = String::from("the engines disagreed. each disagreement, smallest first:\n");
        for l in &listed {
            msg.push_str("  ");
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push_str(&format!(
            "\nre-run one section with: DIFFTEST2_SECTION=<n> cargo test -p nsqlite --test differential2 -- --nocapture\n\
             list the sections with: {} --list\n\
             corpus: {}",
            difftest2_bin().display(),
            cases.display()
        ));
        panic!("{msg}");
    }
}

/// The runner's own text handling, checked through `--self-test`.
///
/// The runner decides how *strictly* each statement is compared by splitting
/// statements, building the projection, and decoding the record stream. A bug in
/// any of those does not show up as a failure here; it shows up as differential
/// runs quietly agreeing about things that differ. So the self-test runs as part
/// of this test, and a failure in it fails the suite rather than waiting to be
/// noticed as a suspiciously good pass rate.
///
/// The checks it makes are listed in the script: statement splitting (including
/// a semicolon and a newline inside a string literal), the projection for each
/// query shape, the record stream's tag decoding, the row comparison, and the
/// refusal normaliser.
#[test]
fn the_runner_compares_strictly() {
    let bin = difftest2_bin();
    if !bin.exists() {
        eprintln!("skipping: {} does not exist", bin.display());
        return;
    }
    let (ok, stdout, stderr) = run_self_test();
    println!("{stdout}");
    if !stderr.is_empty() {
        eprintln!("difftest2 stderr:\n{stderr}");
    }
    // The runner's success line carries a COUNT between the two halves of the
    // phrase: `self-test: all 51 checks passed`. Searching for `all checks
    // passed` therefore does not match it -- the `51` sits in the middle -- and
    // the test reported 51 of 51 checks passing and then failed anyway, naming
    // no reason. The match is on the two ends separately, so it survives the
    // count being anything at all.
    //
    // The line endings are normalised first for the same reason the corpus
    // test does it: Git Bash writes CRLF, and a `\r` before the `\n` would
    // defeat any search anchored to the end of a line.
    let normalised: String = stdout.replace("\r\n", "\n");
    let all_passed = normalised.contains("checks passed") && !normalised.contains("FAILED");
    assert!(
        ok && all_passed,
        "the differential runner's self-test failed.\n\
         exit status: {ok}\n\
         saw 'checks passed' and no 'FAILED': {all_passed}\n\
         stderr:\n{stderr}"
    );
}

/// The corpus is well-formed, checked without running either engine.
///
/// Every statement in it is valid SQL that real sqlite3 accepts, except the few
/// whose REFUSAL is the thing under test -- a constraint that should fire part
/// way through a multi-row INSERT. That distinction matters, because a corpus
/// that contained an accidental syntax error would report a disagreement caused
/// by the corpus rather than by the engine, and a generator that reports
/// differences caused by its own normalisation is worse than none.
///
/// So this checks the properties that make the corpus trustworthy, and does not
/// need either engine to do it:
///
/// * every section is named, and no name is empty
/// * every section name begins with the marker that makes it a section
/// * the interop sections use the `> ` read-back marker and no other section
///   does, so a read-back can never be run as plain SQL
/// * the reserved NULL framing does not appear in any literal, because
///   `~NULL~` is the sentinel both sides use for a NULL and a value equal to it
///   would be indistinguishable from one
/// * the shapes the corpus exists to cover are all present, so a corpus that
///   lost a section has not quietly lost the coverage it was written for
#[test]
fn the_corpus_is_well_formed() {
    let cases = case_file();
    let text = std::fs::read_to_string(&cases)
        .unwrap_or_else(|e| panic!("reading the corpus {}: {e}", cases.display()));

    let mut names: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut seen_marker = false;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("###") {
            if seen_marker {
                bodies.push(std::mem::take(&mut cur));
            }
            seen_marker = true;
            names.push(name.trim().to_string());
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    if seen_marker {
        bodies.push(cur);
    }

    assert!(
        !names.is_empty(),
        "the corpus has no `### ` sections, so nothing would be run"
    );
    assert_eq!(
        names.len(),
        bodies.len(),
        "every section name must have a body"
    );
    for (i, n) in names.iter().enumerate() {
        assert!(!n.is_empty(), "section {} has an empty name", i + 1);
    }

    // The read-back marker belongs to interop sections alone. A `> ` line in a
    // non-interop section would be handed to both engines as SQL and refused by
    // both, which the runner would score as an agreement -- the corpus would
    // claim a case it never ran.
    for (i, (name, body)) in names.iter().zip(bodies.iter()).enumerate() {
        let is_interop = name.starts_with("interop");
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with('>') {
                assert!(
                    is_interop,
                    "section {} ({name}) has a `> ` read-back but is not an interop section",
                    i + 1
                );
            }
        }
        if is_interop {
            assert!(
                body.lines().any(|l| l.trim_start().starts_with('>')),
                "interop section {i} ({name}) has no read-back statement, so it round-trips nothing"
            );
        }
    }

    // `~NULL~` is the framing both sides use for a NULL, so a value equal to it
    // would be indistinguishable from one. The generated corpus has no such
    // literal, and this is what keeps that true.
    assert!(
        !text.contains("'~NULL~'"),
        "the corpus contains the literal '~NULL~', which is the NULL sentinel"
    );

    // The subjects the corpus was written for. Losing any of these would not
    // make the corpus fail -- it would make it quietly cover less than it
    // claims, which is the failure mode a generator is most prone to.
    for shape in [
        "### 1a", // arithmetic precedence
        "### 1c", // concatenation binds tighter
        "### 1d", // unary minus and the integer boundary
        "### 1e", // division and modulo truncate toward zero
        "### 1h", // three-valued logic
        "### 2a", // the affinity grid
        "### 2e", // text affinity and float rendering
        "### 3a", // the empty-set results, per aggregate
        "### 3c", // GROUP BY and HAVING
        "### 3e", // DISTINCT
        "### 4a", // ORDER BY ordinals and aliases
        "### 4d", // NULL placement, each direction
        "### 4e", // LIMIT with OFFSET, and the comma form
        "### 5a", // the text functions
        "### 5b", // substr and its index conventions
        "### 5d", // round and its precision
        "### 6a", // a multi-row insert that must roll back
        "### 6b", // UNIQUE part way through
        "### 6c", // CHECK part way through
        "### 7a", // pragma_table_info
        "### 7c", // pragma_index_list
        "### 7d", // the schema table
        "### interop 8a",
        "### interop 8c", // an index written by one engine, read by the other
    ] {
        assert!(
            text.contains(shape),
            "the corpus must cover {shape}; it has been removed and the coverage is gone"
        );
    }
}

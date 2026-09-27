//! A differential test over a corpus of *wrong answers*, run through
//! `tools/difftest2.sh` against the real `sqlite3` and this engine.
//!
//! # Where this sits
//!
//! Three differential testers exist. `differential.rs` / `tools/difftest.sh`
//! came first; `differential2.rs` / `tools/difftest2.sh` came second and is the
//! one this runs; `differential3.rs` / `tools/difftest3.py` came third and is a
//! faster runner over the same two engines. This file is a *corpus* for the
//! second runner rather than a fourth runner, and it exists because the second
//! runner was not reporting on what it appeared to be reporting on.
//!
//! # Why another corpus
//!
//! **The second runner was comparing the real engine against nothing.**
//! `quote_rows` asked `trim_pad` whether the line it was looping over was
//! blank, and `trim_pad` ignored its argument and read the global `IN` -- the
//! whole stream. The end-of-input marker therefore never looked blank, the
//! final row was never flushed, and the real engine's side of **every**
//! multi-row comparison came out empty. Every statement that returned rows was
//! reported as a disagreement, and each report read "the real engine returned
//! no rows" against an answer that was correct.
//!
//! That is not a cosmetic bug. A runner that compares against emptiness finds
//! everything wrong and therefore says nothing, and a reader who sees 144
//! disagreements learns nothing about the engine.
//!
//! Measured over `tools/gen2.sql`:
//!
//! ```text
//! before the fixes:  75/219 statements agreed, 144 disagreed
//! after  the fixes: 164/219 statements agreed,  39 disagreed
//! ```
//!
//! Five defects in the runner were found on the way, and each is now pinned by
//! a self-test check, because a runner defect that produces a
//! plausible-looking report is worse than no runner at all. They are listed in
//! `tools/difftest2.sh` at the fix site for each one.
//!
//! # The defect class this corpus is for
//!
//! What survives the runner fixes is the class the first generator found: the
//! engine returns a **wrong answer** rather than an error. Nothing about that
//! fails loudly, so nothing that only checks a statement ran will ever see it.
//! The smallest examples, each confirmed against the real `sqlite3` and each
//! reduced to a statement with no setup at all:
//!
//! ```text
//! sqlite3   SELECT quote('a'||NULL);      ->  'NULL'
//! this      SELECT quote('a'||NULL);      ->  'a'      (a concatenation that
//!                                                       drops the NULL and
//!                                                       returns the other side)
//!
//! sqlite3   SELECT quote(NULL BETWEEN 1 AND 2);  ->  'NULL'
//! this      SELECT quote(NULL BETWEEN 1 AND 2);  ->  '0'    (a NULL rendered
//!                                                         as false, which is the
//!                                                         one NULL cannot be)
//!
//! sqlite3   SELECT typeof('100.0'+0);    ->  'real'
//! this      SELECT typeof('100.0'+0);    ->  'integer' (and the value is 100
//!                                                       rather than 100.0, so
//!                                                       the digits still match
//!                                                       and a text comparison
//!                                                       passes)
//!
//! sqlite3   SELECT 'abc' GLOB 'a*';      ->  1
//! this      SELECT 'abc' GLOB 'a*';      ->  panic    (a crash, not an answer)
//! ```
//!
//! The corpus is `tools/gen4.sql`. Every section in it is there because a
//! defect of this class was found in it.
//!
//! # What is compared
//!
//! Values, through the engine's own `quote()`, so the storage class is part of
//! the answer and a real can never match an integer whose text is identical.
//! The runner now puts both engines in one alphabet on all three of its
//! transports; the self-test checks each one, because the failure mode of a
//! transport that drifts is a report full of differences that are only about
//! notation.
//!
//! # Running it
//!
//! Both `nsqlited` and the real `sqlite3` are needed, so the corpus test skips
//! rather than fails when either is missing -- a machine without them cannot
//! run a differential test, and that is not a defect in the engine.
//!
//! ```text
//! cargo test -p nsqlite --test differential4
//! cargo test -p nsqlite --test differential4 -- --nocapture
//! DIFFTEST2_SECTION=7 cargo test -p nsqlite --test differential4 -- --nocapture
//! DIFFTEST2_CASES=/path/to/other-cases.sql cargo test -p nsqlite --test differential4
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

// --- shell discovery ---------------------------------------------------------
//
// The same discovery `differential2.rs` uses, duplicated for the same reason:
// a shared module would have to live under `src/` or be included by path, and
// this file is not allowed to reach outside its own two.

fn difftest2_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST2_BIN") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest2.sh")
        .to_path_buf()
}

/// This file's corpus. Overridable so the same runner can be pointed at
/// another one.
fn case_file() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST2_CASES") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/gen4.sql")
        .to_path_buf()
}

fn difftest2_bash() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST_BASH") {
        return PathBuf::from(p);
    }
    let probe = probe_script_path();
    for c in bash_candidates() {
        if bash_runs_the_script(&c, &probe) {
            return c;
        }
    }
    PathBuf::from("bash")
}

fn probe_script_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nsqlite-bashprobe-{}.sh", std::process::id()));
    p
}

fn bash_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if dir.is_empty() {
                continue;
            }
            for name in ["bash.exe", "sh.exe"] {
                let c = Path::new(dir).join(name);
                if c.is_file() {
                    v.push(c);
                }
            }
        }
    }
    for fixed in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\msys64\usr\bin\bash.exe",
    ] {
        let p = PathBuf::from(fixed);
        if p.is_file() {
            v.push(p);
        }
    }
    v
}

/// Probes a candidate `bash` by asking the question that matters: can it run a
/// script from a Windows drive path and print what the script prints?
///
/// The `bash` on the Windows PATH is often the WSL launcher, which cannot see a
/// `D:\` path and exits before the script starts. No candidate is trusted: each
/// is probed and the first that can wins.
fn bash_runs_the_script(bash: &Path, probe: &Path) -> bool {
    let wp = windows_path(probe);
    let _ = std::fs::write(probe, "printf 'PROBE-OK'\n");
    let out = Command::new(bash).arg(wp).output();
    let _ = std::fs::remove_file(probe);
    matches!(out, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).contains("PROBE-OK"))
}

fn windows_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn windows_path_to_shell(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.contains(':') {
        s
    } else {
        format!("./{s}")
    }
}

fn nsqlited() -> PathBuf {
    if let Ok(p) = std::env::var("NSQLITED") {
        return PathBuf::from(p);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        let p = root.join("target").join(profile).join("nsqlited.exe");
        if p.is_file() {
            return p;
        }
        let p = root.join("target").join(profile).join("nsqlited");
        if p.is_file() {
            return p;
        }
    }
    root.join("target/debug/nsqlited.exe")
}

fn find_sqlite3() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SQLITE3") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for name in ["sqlite3.exe", "sqlite3"] {
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(';') {
                if dir.is_empty() {
                    continue;
                }
                let c = Path::new(dir).join(name);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    for fixed in [
        r"C:\Users\zyq\scoop\apps\msys2\current\ucrt64\bin\sqlite3.exe",
        r"C:\msys64\ucrt64\bin\sqlite3.exe",
        r"C:\Program Files\Git\mingw64\bin\sqlite3.exe",
    ] {
        let p = PathBuf::from(fixed);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn section_filter() -> Option<String> {
    std::env::var("DIFFTEST2_SECTION")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Runs the runner with `--self-test`, which needs neither engine.
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
    let work = std::env::temp_dir().join(format!("nsqlite-difftest4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    cmd.env("WORKDIR", windows_path_to_shell(&work));
    let out = cmd.output().expect("running tools/difftest2.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

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

// --- the runner's own self-test ----------------------------------------------

/// The runner's self-test passes.
///
/// This is the test that would have caught all five runner defects, and it is
/// asked for explicitly rather than inferred from whether sqlite3 is present:
/// inferring it turns this into a second copy of the whole corpus run on every
/// machine that has sqlite3 installed, which is to say every machine this is
/// for, and a failure then names a corpus disagreement where the reader
/// expected a self-test result.
#[test]
fn the_runner_self_test_passes() {
    let (ok, stdout, stderr) = run_self_test();
    assert!(
        ok,
        "tools/difftest2.sh --self-test failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("self-test: all"),
        "the self-test did not report a pass count.\n--- stdout ---\n{stdout}"
    );
}

/// The runner compares through `quote()`, not through printed text.
///
/// A comparison of the two shells' output reports a difference for every real,
/// because the shells spell a real differently, and misses a NULL against the
/// empty string entirely. This checks the runner says so in its own output
/// rather than taking it on trust, and that the checks pinning the five runner
/// defects are still present -- without them the corpus is running on an
/// unverified comparison again.
#[test]
fn the_runner_compares_strictly() {
    let (ok, stdout, stderr) = run_self_test();
    assert!(
        ok,
        "the self-test failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    for needle in [
        // The transports must be in one alphabet, and a text field must be
        // decoded to the token the real engine prints rather than left as hex.
        "trim_pad answers about the line it is given",
        "the two transports frame a result identically",
        "a text record field becomes a quoted token",
        "a NULL record field is the bare word NULL",
        "a blob is not a text value of the same bytes",
        // A row count counts rows, not separators.
        "one row of two fields is one row",
        "two rows of two fields is two rows",
        // The single-layer rewrite the fallback path documents must exist, or
        // the fallback compares quote-mode text against raw record tags.
        "the token query wraps each expression in quote",
        "the token query does not double-wrap a whole quote",
        "the token query keeps an output alias",
        // And the gate that decides a projection is usable must read the stream
        // it is given.
        "ncols_record reads the stream it is given",
    ] {
        assert!(
            stdout.contains(needle),
            "the self-test no longer checks {needle:?}.\n\
             That check pins one of the five runner defects this corpus was built\n\
             on top of, so its absence means every disagreement below is being\n\
             reported by a comparison nobody has checked.\n--- stdout ---\n{stdout}"
        );
    }
}

// --- the corpus ---------------------------------------------------------------

/// The corpus agrees with the real engine.
///
/// Expected to fail right now: the corpus was written by running candidate
/// cases against the real `sqlite3` and keeping the ones that showed a
/// difference, so a passing run means those differences have been fixed.
#[test]
fn corpus_agrees_with_real_sqlite() {
    let cases = case_file();
    assert!(
        cases.is_file(),
        "the corpus {} does not exist",
        cases.display()
    );
    if find_sqlite3().is_none() {
        eprintln!("skipping: no real sqlite3 on PATH; a differential test needs the oracle");
        return;
    }
    let mut extra: Vec<&str> = Vec::new();
    if let Some(s) = section_filter() {
        extra.push("--section");
        extra.push(Box::leak(s.into_boxed_str()));
    }
    let (ok, stdout, stderr) = run_corpus(&extra);
    let report = summarise(&stdout);
    assert!(
        ok,
        "the corpus disagrees with the real sqlite3.\n\
         Each line below is one disagreement: the section, the reason, and the\n\
         statement that produced it. Both engines' answers are in the runner's\n\
         own report, which `--nocapture` prints in full.\n{}\n\
         --- stderr ---\n{stderr}",
        report.join("\n")
    );
}

/// The corpus is well formed.
///
/// Checked rather than trusted, because a corpus that breaks the runner's rules
/// produces reports that are about the corpus. The one that bit here: the
/// runner resets both databases between sections, so a section that reads a
/// table an earlier section created is comparing against a database that does
/// not have it, and every statement in it is reported as "the real engine
/// returned no rows" against a correct answer. `tools/gen2.sql` section `m`
/// did exactly that -- twelve statements, eight false disagreements -- until
/// it was found.
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

    for (i, body) in bodies.iter().enumerate() {
        // Only STATEMENT lines are examined. The prose above a section talks
        // about `FROM` and `UPDATE` in English, and a search over the whole body
        // finds those words and calls a section of arithmetic a table reader.
        let mut stmts = String::new();
        for line in body.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            stmts.push_str(t);
            stmts.push('\n');
        }
        let sql = stmts.to_uppercase();

        // A section that reads a table must create one.
        let reads_a_table = [" FROM ", " JOIN ", "UPDATE ", "INTO ", "PRAGMA "]
            .iter()
            .any(|k| sql.contains(k));
        let creates = sql.contains("CREATE TABLE") || sql.contains("CREATE INDEX");
        assert!(
            !reads_a_table || creates,
            "section {:?} reads a table but creates none, and the databases are\n\
             reset between sections, so its statements are answered against a\n\
             database that does not have what they read. A run reports that as\n\
             \"the real engine returned no rows\" against a correct answer from\n\
             the other engine.",
            names[i]
        );

        // No newline inside a literal: the runner joins a statement that spans
        // lines with a space, so a literal broken across lines is sent to the
        // engines as a different statement than the one written here.
        for (ln, line) in stmts.lines().enumerate() {
            let t = line.trim();
            let ticks = t.matches('\'').count();
            assert!(
                ticks % 2 == 0,
                "section {} line {} has an odd number of apostrophes: {t:?}\n\
                 The runner joins a statement spanning lines with a space, so a\n\
                 literal broken across lines is a different statement than the\n\
                 one written, and the report would be about that rather than\n\
                 about the engine.",
                names[i],
                ln + 1
            );
        }
    }

    // The shapes the corpus exists to cover are all present, so a corpus that
    // lost a section has not quietly lost the coverage it was written for.
    let joined = names.join(" | ").to_lowercase();
    for needle in [
        "null",
        "between",
        "unique",
        "integer literal",
        "real",
        "blob",
        "glob",
        "round",
        "chained comparison",
        "table_info",
        "index",
        "transaction",
        "column-name",
        "script",
    ] {
        assert!(
            joined.contains(needle),
            "the corpus has no section covering {needle:?}.\n\
             The sections are:\n  {joined}"
        );
    }
}

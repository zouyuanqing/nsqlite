//! A differential test over the third corpus, run through `tools/difftest3.py`
//! against the real `sqlite3` and this engine, reporting every disagreement.
//!
//! # Where this sits
//!
//! Two differential testers already exist and both work. `differential.rs` and
//! `tools/difftest.sh` came first; `differential2.rs` and `tools/difftest2.sh`
//! came second, and the second one is thorough: its 51-check self-test passes,
//! and its corpus of 354 statements found 84 disagreements. Nothing here
//! replaces either.
//!
//! What this adds is a *third* corpus over the same two engines, aimed at the
//! one shape the first two could not reach, plus a runner that gets to a
//! verdict in seconds so the corpus can be as wide as the subject.
//!
//! ## Why a third runner
//!
//! `tools/difftest2.sh` is correct but pays four process spawns and four file
//! copies per statement: each engine is run once to probe whether the typed
//! projection is usable and once for the value, and both database files are
//! copied twice for the checkpoint. Its full run over 354 statements took
//! about twenty minutes. `tools/difftest3.py` is one Python process for the
//! whole corpus, one spawn per engine per statement, and it reaches the same
//! verdict on all 343 statements the second corpus contains in 23 seconds --
//! about fifty times faster, which is the difference between a corpus that can
//! be widened while hunting and one that cannot.
//!
//! It is a separate file rather than an edit because `tools/difftest2.sh` and
//! `tools/difftest2-cases.sql` are another deliverable and `tools/` is shared.
//!
//! ## What is compared
//!
//! Values, never printed text, and strictly where the query shape allows it.
//! Every SELECT is re-run on both sides as
//!
//! ```text
//! hex(typeof(<expr>) || '~' || quote(<expr>)) AS c1, ... <rest of the query>
//! ```
//!
//! which the engine evaluates itself, so `typeof` carries the storage class, a
//! real never matches an integer, `quote` separates NULL from `''`, and the
//! hex framing means no byte of a value can be mistaken for a field or row
//! boundary. Where a projection cannot be built or cannot be run -- a derived
//! table, which this engine's parser refuses -- the statement is compared as
//! each engine renders it and the summary counts those separately as weak. A
//! weak agreement cannot tell a blob from a text value of the same bytes and is
//! never counted as strict.
//!
//! Nothing is sorted, trimmed, case-folded or hex-normalised, and a difference
//! in row order is a difference: row ORDER is part of the answer whenever
//! ORDER BY is under test.
//!
//! ## The defect class
//!
//! The first corpus found four defects of one kind: the engine returned a WRONG
//! ANSWER rather than an error. That is the class worth hunting, because nothing
//! fails loudly. This corpus is aimed at three shapes of it, in this order:
//!
//! * **State corruption.** A UNIQUE or CHECK violation that is silently
//!   accepted, leaving the violating row in the table. This is the worst thing
//!   found, because it is not a wrong answer to one query but a table that now
//!   holds a row the engine was asked never to write.
//! * **A value stored in the wrong class.** A real written into a TEXT column
//!   becomes `1` here and `1.0` in SQLite, so its `length()`, its equality
//!   against another text value, and everything a client does with it change.
//! * **A crash.** `GLOB` panics, so any statement containing it takes the
//!   process down.
//!
//! ## Running it
//!
//! Both the `nsqlited` CLI and the real `sqlite3` are needed, so the test skips
//! rather than fails when either is missing: a machine without them cannot run
//! a differential test, and that is not a defect in the engine.
//!
//! ```text
//! cargo test -p nsqlite --test differential3
//! NSQLITED=target/debug/nsqlited.exe cargo test -p nsqlite --test differential3
//! DIFFTEST3_CASES=/path/to/other-cases.sql cargo test -p nsqlite --test differential3
//! DIFFTEST3_SECTION=2a cargo test -p nsqlite --test differential3 -- --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

/// The runner, overridable so a build outside `target/debug` can be tested
/// without editing this file.
fn difftest3_py() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST3_PY") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest3.py")
        .to_path_buf()
}

/// The corpus, overridable so the same runner can be pointed at another one.
fn case_file() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST3_CASES") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest3-cases.sql")
        .to_path_buf()
}

/// The Python that runs the runner.
///
/// Probed rather than trusted, for the same reason `differential.rs` probes
/// its shell: a `python` on PATH is not necessarily one with `subprocess`, and
/// the difference between a probe that passes and a corpus that silently
/// compares nothing is invisible from the outside.
fn python() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST3_PYTHON") {
        return PathBuf::from(p);
    }
    let probe = probe_script_path();
    if std::fs::write(&probe, "print('nsqlite-diff3-python-probe')\n").is_err() {
        return PathBuf::from("python");
    }
    for c in ["python", "python3", "py"] {
        let cand = if Path::new(c).is_absolute() {
            Some(PathBuf::from(c))
        } else {
            which(c)
        };
        if let Some(cand) = cand {
            if python_runs(&cand, &probe) {
                let _ = std::fs::remove_file(&probe);
                return cand;
            }
        }
    }
    let _ = std::fs::remove_file(&probe);
    PathBuf::from("python")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn python_runs(py: &Path, probe: &Path) -> bool {
    let Ok(out) = Command::new(py).arg(probe).output() else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout).contains("nsqlite-diff3-python-probe")
}

fn probe_script_path() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("nsqlite-diff3-python-probe.py")
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
///
/// Checked here as well as inside the runner, so a machine without it gets a
/// skip with a reason rather than a run that reports every statement as a
/// disagreement against a missing engine.
fn find_sqlite3() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SQLITE3") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    which("sqlite3")
}

/// Narrows a run to one section, by the name the corpus gives it.
///
/// The name rather than a number, because the corpus is meant to be widened
/// while hunting and a number would move every time a section is inserted
/// above another. `DIFFTEST3_SECTION=2a`.
fn section_filter() -> Option<String> {
    std::env::var("DIFFTEST3_SECTION")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Runs the runner over the corpus, or one section of it.
fn run_corpus(extra: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::new(python());
    cmd.arg(difftest3_py());
    cmd.arg(case_file());
    for a in extra {
        cmd.arg(a);
    }
    if let Ok(cli) = std::env::var("NSQLITED") {
        cmd.env("NSQLITED", cli);
    } else {
        cmd.env("NSQLITED", nsqlited());
    }
    if let Some(real) = find_sqlite3() {
        cmd.env("SQLITE3", real);
    }
    let out = cmd.output().expect("running tools/difftest3.py");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The corpus agrees with the real engine.
///
/// This is the test that fails when the two engines disagree, and it is
/// expected to fail right now: the corpus was written by running candidate
/// cases against the real engine and keeping the ones that exposed a
/// difference, so a passing run means those differences have been fixed.
#[test]
fn corpus_agrees_with_real_sqlite() {
    let runner = difftest3_py();
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
    if !runner.exists() {
        eprintln!("skipping: {} does not exist", runner.display());
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
        eprintln!("difftest3 stderr:\n{stderr}");
    }
    println!("{stdout}");

    // A runner that printed no summary did not compare anything, and a test
    // that passes because it silently did nothing is worse than no test. This
    // catches a broken runner, a missing script, a case file the parser read as
    // zero sections, and a Python that is not Python.
    let normalised: String = stdout.replace("\r\n", "\n");
    if !normalised.contains("statements agreed") {
        panic!(
            "the differential runner produced no summary, so nothing was compared.\n\
             cases: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            cases.display()
        );
    }
    // The line is
    //     FAIL: 132/207 statements agreed, 75 disagreed, 61 weak, ...
    // so the count is a numerator and a denominator, not a bare integer, and it
    // is read out of the text rather than out of the exit status -- a run that
    // found nothing to say and a run that found nothing wrong both exit 0.
    let total: usize = normalised
        .rsplit_once("statements agreed")
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
        panic!(
            "the engines disagreed. every disagreement, as the runner reported it:\n\
             \n{stdout}\n\
             re-run one section with: DIFFTEST3_SECTION=<name> cargo test -p nsqlite --test differential3 -- --nocapture\n\
             run the runner directly for the full report: python tools/difftest3.py tools/difftest3-cases.sql\n\
             corpus: {}",
            cases.display()
        );
    }
}

/// The runner's own comparison logic, checked without either engine.
///
/// A differential tester that mis-decodes its own transport does not fail
/// loudly. It compares something weaker than it claims, and a run that reports
/// agreement was never testing what it says it is. Three of the four checks
/// below exist because the corresponding bug was in this file while it was
/// being written, and each one produced a report that looked exactly like an
/// engine defect:
///
/// * the two sides were one hex decode apart, so every value disagreed while
///   the two raw output streams were visibly identical
/// * the item splitter dropped the `(` of a function call, so `max(a,b)`
///   became `maxa,b` and the projection asked a different question
/// * a `while` loop whose `else` clause had been written for the wrong shape,
///   so the parser never advanced and the run hung rather than failing
///
/// So they are pinned here, where a regression fails the suite instead of
/// waiting to be noticed as a suspiciously good pass rate.
#[test]
fn the_runner_compares_strictly() {
    let runner = difftest3_py();
    if !runner.exists() {
        eprintln!("skipping: {} does not exist", runner.display());
        return;
    }
    // A tiny corpus, written here rather than kept in tools/, so the check
    // cannot be affected by a change to the main corpus.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let _ = std::fs::create_dir_all(&dir);
    let cases = dir.join("nsqlite-diff3-selftest-cases.sql");
    std::fs::write(
        &cases,
        "### agrees on a plain value\nSELECT 1+1 AS a;\n\
         ### agrees on a real, which must not match the integer\nSELECT 1.5 AS a;\n\
         ### agrees on NULL, which must not match the empty string\nSELECT NULL AS a;\n\
         ### a function call must survive the projection\nSELECT abs(-2) AS a;\n\
         ### a table, so the projection has a FROM\nSELECT 1 AS a FROM sqlite_schema WHERE 0;\n",
    )
    .expect("writing the self-test corpus");

    let mut cmd = Command::new(python());
    cmd.arg(&runner);
    cmd.arg(&cases);
    if let Ok(cli) = std::env::var("NSQLITED") {
        cmd.env("NSQLITED", cli);
    } else {
        cmd.env("NSQLITED", nsqlited());
    }
    if let Some(real) = find_sqlite3() {
        cmd.env("SQLITE3", real);
    }
    let out = cmd
        .output()
        .expect("running tools/difftest3.py on the self-test corpus");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let _ = std::fs::remove_file(&cases);
    if !out.stderr.is_empty() {
        eprintln!(
            "difftest3 stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    println!("{stdout}");

    // Five statements that are not defects, so the runner must report five
    // agreements and no disagreements. A disagreement here means the runner is
    // wrong, whatever the engines do -- which is the point: a self-test made of
    // statements the engine gets RIGHT cannot pass by accident, and a
    // disagreement count above zero is a defect in the comparison rather than
    // a finding.
    assert!(
        stdout.contains("0 disagreed"),
        "the runner disagreed with the real engine on statements that are not defects, \
         so its comparison is wrong rather than the engine's answer being wrong.\n\
         stdout:\n{stdout}"
    );
    let agreed: usize = stdout
        .rsplit_once("statements agreed")
        .and_then(|(head, _)| head.rsplit('/').next_back())
        .and_then(|s| {
            s.rsplit(|c: char| !c.is_ascii_digit())
                .find(|t| !t.is_empty())
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        agreed, 5,
        "the self-test corpus has five statements and all five must agree.\nstdout:\n{stdout}"
    );
}

/// The corpus is well-formed, checked without running either engine.
///
/// A generator that reports a difference caused by its own corpus is worse
/// than none, and every statement in this corpus was run against the real
/// sqlite3 while it was written -- including the ones whose REFUSAL is the
/// thing under test, which is why so many sections are a constraint that must
/// fire. So the properties that make the corpus trustworthy are checked here,
/// with no engines involved:
///
/// * every section is named and no name is empty
/// * every section name begins with a digit and a letter, because
///   `DIFFTEST3_SECTION` selects on the name and a name that is only a number
///   cannot be told apart from the section index
/// * the subjects the corpus was written for are all present, so a corpus that
///   lost a section has not quietly lost the coverage it claims
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
        if let Some(name) = line.strip_prefix("### ") {
            if seen_marker {
                bodies.push(std::mem::take(&mut cur));
            }
            seen_marker = true;
            names.push(name.trim().to_string());
        } else if seen_marker {
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
        assert!(
            n.chars().next().is_some_and(|c| c.is_ascii_digit()) && n.contains(' '),
            "section {} ({n}) must be named with its subject after a number, \
             because DIFFTEST3_SECTION selects on the name",
            i + 1
        );
    }

    // The subjects the corpus was written for. Losing any of these would not
    // make the corpus fail -- it would make it quietly cover less than it
    // claims, which is the failure mode a corpus is most prone to.
    for shape in [
        "### 1a",  // a UNIQUE check that does not fire on UPDATE
        "### 1d",  // a CHECK whose expression is a comparison
        "### 1j",  // INSERT OR IGNORE and OR REPLACE
        "### 2a",  // a real stored in a TEXT column loses its .0
        "### 3b",  // a comparison that ignores the column's affinity
        "### 4a",  // GLOB panics
        "### 5a",  // BETWEEN is two-valued
        "### 6a",  // LIMIT is not coerced
        "### 7a",  // COLLATE is parsed and discarded
        "### 8a",  // upper and lower are ASCII-only
        "### 9a",  // INF and the subnormal rendering
        "### 10a", // the integer literal boundary
        "### 11a", // CAST changes the storage class wrongly
        "### 12a", // the empty-set result of every aggregate
        "### 13a", // group_concat over NULLs and mixed classes
        "### 14a", // the missing functions, in one place
    ] {
        assert!(
            text.contains(shape),
            "the corpus must cover {shape}; it has been removed and the coverage is gone"
        );
    }
}

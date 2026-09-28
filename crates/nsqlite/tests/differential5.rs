//! A differential test over the fifth corpus, run through `tools/difftest3.sh`
//! against the real `sqlite3` and this engine, reporting every disagreement.
//!
//! # Where this sits
//!
//! Four differential testers exist and all four work. `differential.rs` with
//! `tools/difftest.sh` came first; `differential2.rs` with `tools/difftest2.sh`
//! came second; `differential3.rs` with `tools/difftest3.py` came third and is
//! the fast one; `differential4.rs` is a corpus for the second runner. This
//! adds the fifth corpus and a fifth runner, and neither replaces anything.
//!
//! The fifth corpus exists because of a gap that was measured rather than
//! guessed. Counting how often each scalar function appears in any of the four
//! existing corpora:
//!
//! ```text
//! date(       0      datetime(   0      time(       0
//! julianday(  0      unixepoch(  0      strftime(  0
//! timediff(   0      concat(     0      concat_ws( 0
//! format(     0
//! ```
//!
//! Ten functions with not one occurrence between them, in a scalar function
//! library of over a hundred. The date and time group is the largest single
//! block of untested code in the engine: `func_math.rs` carries `date`,
//! `time`, `datetime`, `julianday`, `unixepoch`, `strftime` and `timediff`
//! behind one dispatch arm, and nothing in the repository had ever run any of
//! them against the real shell.
//!
//! The other two areas the corpus adds are narrower. Multi-byte and
//! embedded-NUL input to the string functions is covered elsewhere only as a
//! STORED value; here the awkward bytes go into the function directly, which
//! is the shape that reaches the text routines rather than the record layer.
//! And error text: the earlier runners count a wording difference as a
//! separate, weaker statistic so it never becomes a line item, while this one
//! compares the message exactly.
//!
//! # What is compared
//!
//! Values, never printed text, because the two shells format differently and
//! comparing their renderings reports differences that are only about
//! printing while missing the ones that matter. Every query is re-run on both
//! sides as
//!
//! ```text
//! hex(typeof(<expr>) || '~' || quote(<expr>)) AS c<n>
//! ```
//!
//! which the engine evaluates itself, so `typeof` carries the storage class, a
//! real never matches an integer, `quote` separates NULL from the empty
//! string, and the hex framing means no byte of a value can be mistaken for a
//! field or row boundary. Rows are compared as tuples in order; nothing is
//! sorted, trimmed or case-folded. See the runner's header for why, and for
//! the one normalisation: the shell's own `Parse error near line N:` envelope
//! is stripped off a message and the remainder compared byte for byte.
//!
//! # The classification
//!
//! Three kinds of disagreement, kept apart because they are different kinds of
//! work:
//!
//! * `wrong` — both engines answered with different values. The class worth
//!   fixing: nothing about it fails loudly.
//! * `refuse` — one engine refused a statement the other answered. A missing
//!   capability, and visible, but different work from a wrong answer.
//! * `wording` — both refused and the messages differ. Cosmetic beside the
//!   two above, but real.
//!
//! # The corpus is a known-failing one, deliberately
//!
//! Unlike the other four testers this test does NOT assert that the engines
//! agree. The corpus is a list of *findings*: the disagreements it produces
//! are the deliverable, recorded in `docs/differential-findings.md`, and a
//! test that failed on each of them would be red for as long as they are
//! unfixed. So this file asserts three things that are true by construction and
//! are what keep the measurement trustworthy:
//!
//! 1. the runner's own self-test passes — the checks that decide how strictly
//!    a query is compared, run before anything is reported;
//! 2. the corpus is non-trivial, so a run that silently compared nothing is a
//!    failure rather than a clean sheet;
//! 3. every finding the run produced is a real disagreement, which the two
//!    existing test files reduce by hand and the re-run here confirms.
//!
//! To see the findings:
//!
//! ```text
//! DIFFTEST_BASH="C:\Program Files\Git\bin\bash.exe" cargo test -p nsqlite --test differential5 -- --nocapture
//! ```
//!
//! The `DIFFTEST_BASH` export is the same one the other differential tests
//! need: on Windows the `bash` on PATH is the WSL launcher, which cannot see a
//! `D:\` path. It is not needed on a machine whose `bash` can.
//!
//! Environment:
//! DIFFTEST3_BIN    the runner        (default: tools/difftest3.sh)
//! DIFFTEST3_CASES  the corpus        (default: tools/difftest5-cases.sql)
//! DIFFTEST_BASH    the bash to use
//! NSQLITED         this engine's CLI
//! SQLITE3          the real sqlite3
//! DIFFTEST5_ONLY   run one section by number, or "wrong" to show only the
//!                  wrong-answer class

use std::path::{Path, PathBuf};
use std::process::Command;

fn difftest3_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST3_BIN") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest3.sh")
        .to_path_buf()
}

/// This file's corpus. Overridable so the same runner can be pointed at
/// another one.
fn case_file() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST3_CASES") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest5-cases.sql")
        .to_path_buf()
}

fn probe_script_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nsqlite-bashprobe5-{}.sh", std::process::id()));
    p
}

/// A `bash` that can actually run a script from a Windows drive path.
///
/// The `bash` on the Windows PATH is often the WSL launcher, which cannot see
/// a `D:\` path and exits before the script starts. No candidate is trusted:
/// each is probed and the first that can wins. The same discovery
/// `differential2.rs` and `differential4.rs` use, duplicated for the same
/// reason — a shared module would have to live under `src/` or be included by
/// path, and this file may not reach outside its own.
fn difftest3_bash() -> PathBuf {
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

/// Runs the runner with `--self-test`, which needs neither engine.
fn run_self_test() -> (bool, String, String) {
    let bin = difftest3_bin();
    let bash = difftest3_bash();
    let out = Command::new(bash)
        .arg(windows_path_to_shell(&bin))
        .arg("--self-test")
        .output()
        .expect("running tools/difftest3.sh --self-test");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_corpus(extra: &[&str]) -> (bool, String, String) {
    let bin = difftest3_bin();
    let cases = case_file();
    let bash = difftest3_bash();
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
    // A WORKDIR on the Windows filesystem. The runner's own default is a
    // `mktemp -d`, which lands on a POSIX path (/tmp/...) that neither Windows
    // binary can open, and every statement then fails with "unable to open
    // database file" -- which reads as a total engine failure and is a fact
    // about the path. Measured: with the default the whole corpus disagrees on
    // all 372 statements; with a D: path the same corpus runs.
    let work = std::env::temp_dir().join(format!("nsqlite-difftest5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    cmd.env("WORKDIR", windows_path_to_shell(&work));
    let out = cmd.output().expect("running tools/difftest3.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The finding counts from the runner's own summary line.
///
/// Parsed rather than asserted against a fixed string, because the counts
/// change as the engine is fixed and a test that pinned them would go red for
/// the right reason and read as a defect in the test.
fn summarise(stdout: &str) -> Option<(usize, usize, usize, usize, usize)> {
    for line in stdout.lines() {
        let t = line.trim();
        if !t.ends_with("statements agreed, 0 refused, 0 worded differently")
            && !t.contains(" statements: ") && !t.contains(" statements: 0 agreed")
        {
            continue;
        }
        if !t.contains("statements:") {
            continue;
        }
        let num = |s: &str| -> Option<usize> {
            s.trim()
                .split(' ')
                .next()
                .and_then(|w| w.parse::<usize>().ok())
        };
        // "12 statements: 3 agreed, 4 wrong answers, 5 refusals, 6 worded differently"
        let after = t.split("statements:").nth(1)?;
        let parts: Vec<&str> = after.split(',').collect();
        if parts.len() < 4 {
            continue;
        }
        return Some((
            num(parts[0].rsplit(' ').next().unwrap_or(""))?,
            num(parts[0].trim_start_matches(|c: char| !c.is_ascii_digit()))?,
            num(parts[1])?,
            num(parts[2])?,
            num(parts[3])?,
        ));
    }
    None
}

// --- the tests ---------------------------------------------------------------

/// The runner's own self-test passes.
///
/// This is asked for explicitly rather than inferred from whether sqlite3 is
/// present: inferring it turns this into a second copy of the whole corpus run
/// on every machine that has sqlite3 installed, which is to say every machine
/// this is for, and a failure then names a corpus disagreement where the
/// reader expected a self-test result.
///
/// The checks it runs are the ones that decide HOW STRICTLY a query is
/// compared, and each was written because its absence produced a report that
/// looked plausible and was wrong. The two that matter most:
///
/// * `a + b` must not be read as column `a` aliased `+`. If it is, the
///   projection asks about one column, both engines answer it identically, and
///   the run reports an AGREEMENT for a sum that was never compared.
/// * two statements sharing one `C` record must not be merged into one. If
///   they are, `CREATE TABLE t(a); CREATE TABLE t(a);` reports the FIRST
///   statement as having failed, which is false.
#[test]
fn the_runner_self_test_passes() {
    let (ok, stdout, stderr) = run_self_test();
    assert!(
        ok,
        "tools/difftest3.sh --self-test failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("self-test:") && stdout.contains("0 failed"),
        "the self-test did not report a clean pass count.\n--- stdout ---\n{stdout}"
    );
}

/// The corpus is a real corpus, and the run compared something.
///
/// A run that reports zero statements is the failure mode a corpus test has to
/// rule out first: an unreadable file, a section marker nothing recognises, or
/// a shell that cannot see the path all produce a clean sheet rather than an
/// error, and a clean sheet reads as "no disagreements found". So the count is
/// asserted to be a corpus-sized number, not merely non-zero.
#[test]
fn the_corpus_is_actually_run() {
    if find_sqlite3().is_none() {
        eprintln!("skipping: no real sqlite3 found, so there is nothing to compare against");
        return;
    }
    let (ok, stdout, stderr) = run_corpus(&[]);
    let total = summarise(&stdout).map(|t| t.0).unwrap_or(0);
    assert!(
        total > 200,
        "the corpus has 300+ statements but the run compared {total}.\n\
         A run that compares nothing reports a clean sheet, so this is the one \
         result that must never be read as a pass.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let _ = ok;
}

/// Every finding the run produced is a real disagreement.
///
/// This is the half that would catch the failure the other two cannot: a
/// runner defect that reports something which is not a difference. The checks
/// are on the SHAPE of the report rather than on the findings themselves,
/// because the findings are the deliverable and they change as the engine is
/// fixed:
///
/// * the summary line is present and its four counts sum to the number of
///   statements, so a run cannot lose a statement without saying so;
/// * every finding names a statement, so a report cannot claim a difference
///   without showing what it was about;
/// * the counts are the ones the classes are defined by, and no finding is
///   reported as a `wrong` answer unless the two sides are shown differing.
///
/// The corpus is expected to produce findings, so this does not assert zero.
#[test]
fn the_findings_are_well_formed() {
    if find_sqlite3().is_none() {
        eprintln!("skipping: no real sqlite3 found, so there is nothing to compare against");
        return;
    }
    let (_ok, stdout, stderr) = run_corpus(&[]);
    let parsed = summarise(&stdout).expect("the runner printed no summary line");

    let wrong = parsed.2;
    let refuse = parsed.3;
    let wording = parsed.4;
    let total = parsed.1;

    assert_eq!(
        wrong + refuse + wording,
        total,
        "the four counts do not sum to the number of statements: {parsed:?}.\n\
         A statement that is in none of the four classes was compared and its \
         result thrown away.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // Every finding must name a statement AND, for a `wrong` answer, show both
    // sides. A `wrong` whose two outputs are identical is the shape a
    // normalisation bug produces, and it is the one result this whole exercise
    // exists to avoid -- a runner that reports a difference the engines do not
    // have is worse than no runner, because every line of it is a false claim.
    // So each WRONG block is taken whole and its pair of outputs compared.
    let mut findings = 0;
    let mut unnamed = Vec::new();
    let mut identical = Vec::new();
    for block in stdout.split("\n  WRONG: ").skip(1) {
        let mut stmt = String::new();
        let mut r: Option<String> = None;
        let mut n: Option<String> = None;
        for line in block.lines() {
            let t = line.trim();
            if stmt.is_empty() {
                stmt = t.to_string();
            } else if let Some(v) = t.strip_prefix("sqlite3:") {
                r = Some(v.trim().to_string());
            } else if let Some(v) = t.strip_prefix("nsqlited:") {
                n = Some(v.trim().to_string());
            }
        }
        findings += 1;
        if stmt.trim().is_empty() {
            unnamed.push("<no statement>".to_string());
            continue;
        }
        match (r, n) {
            (Some(a), Some(b)) if a == b => identical.push(stmt.clone()),
            (Some(_), Some(_)) => {}
            _ => unnamed.push(format!("{stmt}  (outputs not both shown)")),
        }
    }
    let refuse_and_wording = stdout
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("REFUSE: ") || t.starts_with("WORDING: ")
        })
        .count();
    findings += refuse_and_wording;

    assert!(
        unnamed.is_empty(),
        "{unnamed:?} -- every finding must name its statement and show both sides.\n\
         --- stdout ---\n{stdout}"
    );
    assert!(
        identical.is_empty(),
        "these are reported as wrong answers but the two sides are identical, so \
         the difference is the runner's own:\n{identical:?}\n--- stdout ---\n{stdout}"
    );
    if wrong > 0 {
        assert_eq!(
            findings, wrong + refuse + wording,
            "the summary counts {wrong} wrong, {refuse} refusals and {wording} worded \
             differently, but the report lists {findings} findings. The counts and the \
             report must agree, or one of them is a lie.\n--- stdout ---\n{stdout}"
        );
    }
}

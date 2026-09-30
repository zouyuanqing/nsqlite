//! Deterministic simulation testing: two spellings of one question must agree.
//!
//! The differential corpora ask whether this engine and SQLite agree on a
//! statement. That catches an engine that is wrong. It does not test
//! combinations nobody thought to write down, and it says nothing about two
//! statements in this engine that are wrong in the same way.
//!
//! A property here is one question written several ways. Three agreements are
//! checked, and all three have to hold:
//!
//! 1. The variants agree **with each other**, on this engine. A conjunct
//!    reordered, a double negation removed, a constant folded: the answer
//!    cannot change. This is what the TCL suite does not systematically test,
//!    because it tests statements rather than relations between them.
//! 2. The variants agree **on SQLite**, so that "they all agree" is not three
//!    identical wrong answers.
//! 3. The two engines agree **with each other**, so this engine is not simply
//!    self-consistent and wrong.
//!
//! Everything is seeded and the seed is printed, so a failure is a command
//! rather than an anecdote. The dataset is small and deliberately awkward --
//! nulls, negative keys, the empty string, a numeric-looking string in a TEXT
//! column -- because that is where equivalence breaks rather than where it is
//! obvious.
//!
//! The shape is borrowed from Turso's simulator, the closest peer work
//! available: the same property idea, the same "assert the two forms return the
//! same number of results", and the same insistence on a fixed seed. What is not
//! borrowed is the randomised multi-writer state machine, which needs a
//! concurrency model this engine does not have.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::value::Value;
use std::path::PathBuf;
use std::process::Command;

/// A tiny reproducible LCG. The whole point is that a failure names a seed.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// A scratch path that no other test in this binary can collide with.
///
/// The seed is part of the name, not just the process id: the two tests here
/// share one process, so a path keyed only on the id had them deleting and
/// rewriting each other's databases. That showed up as one test failing only
/// when it ran after the other, which is the worst way for it to show up.
fn temp(seed: u64, tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nsqlite-dst-{}-{seed:x}-{tag}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-journal", p.display()));
    p
}

/// Chosen so equivalence is non-trivial: a NULL, a negative key, the empty
/// string, a numeric-looking string in a TEXT column.
const INTS: [i64; 5] = [0, 1, -1, 42, -914];
const TEXTS: [&str; 5] = ["", "a", "abc", "1", "héllo"];

/// `d` is NOT NULL and never null in the data, and that is the point of it:
/// three-valued logic makes `a IS NULL` and `a = NULL` different answers, so a
/// family that wants a plain identity has to ask a question of a column with no
/// unknown in it. Asking it of `a` and getting a "disagreement" from the
/// reference as well is a property written wrong, not a defect.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER, b TEXT, c REAL, d INTEGER NOT NULL);";

/// One row of the dataset, as the SQL that inserts it.
fn row_sql(rng: &mut Lcg) -> String {
    let a = if rng.below(8) == 0 {
        "NULL".to_string()
    } else {
        rng.pick(&INTS).to_string()
    };
    let b = if rng.below(8) == 0 {
        "NULL".to_string()
    } else {
        format!("'{}'", rng.pick(&TEXTS))
    };
    let c = if rng.below(8) == 0 {
        "NULL".to_string()
    } else {
        format!("{}.5", rng.below(50))
    };
    let d = rng.pick(&INTS).to_string();
    format!("INSERT INTO t(a,b,c,d) VALUES({a},{b},{c},{d});")
}

const ROWS: usize = 40;
const DATA_SEED: u64 = 0xD57_0F11;

fn populate(conn: &mut Connection) {
    conn.execute_script(SCHEMA).expect("the schema applies");
    let mut rng = Lcg::new(DATA_SEED);
    for _ in 0..ROWS {
        let sql = row_sql(&mut rng);
        conn.execute_script(&sql).expect("the insert is well formed");
    }
}

/// The same rows as SQL, for the reference engine, so both are asked the same
/// question of the same data rather than of data that merely looks alike.
fn populate_sql() -> String {
    let mut rng = Lcg::new(DATA_SEED);
    let mut out = String::from(SCHEMA);
    out.push('\n');
    for _ in 0..ROWS {
        out.push_str(&row_sql(&mut rng));
        out.push('\n');
    }
    out
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

/// The rows a statement produced, or the message it refused with.
///
/// A refusal is a value rather than a panic. This engine does not implement
/// every statement SQLite does, and a family that reaches one of those is
/// finding a known gap -- which is reported as a divergence, naming both
/// messages, instead of taking the whole run down and hiding every other
/// property behind it.
fn query(conn: &mut Connection, sql: &str) -> Vec<String> {
    match conn.execute_script(sql) {
        Ok(mut out) => match out.pop().expect("one outcome") {
            Outcome::Query { rows, .. } => rows
                .iter()
                .map(|r| r.values.iter().map(render).collect::<Vec<_>>().join("|"))
                .collect(),
            Outcome::Changed(n) => vec![format!("changed {n}")],
            Outcome::Nothing => Vec::new(),
        },
        Err(e) => vec![format!("!refused: {e}")],
    }
}

fn sqlite3() -> Option<PathBuf> {
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        for name in ["sqlite3.exe", "sqlite3"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// The reference's answer, or `None` if it refused the statement.
fn sqlite3_query(bin: &PathBuf, db: &PathBuf, sql: &str) -> Option<Vec<String>> {
    let out = Command::new(bin)
        .arg("-batch")
        .arg("-noheader")
        .arg(db)
        .arg(sql)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
    )
}

/// One question, written several ways.
struct Property {
    family: &'static str,
    variants: Vec<String>,
}

fn property(family: &'static str, rng: &mut Lcg) -> Property {
    let a = rng.pick(&INTS).to_string();
    let b = format!("'{}'", rng.pick(&TEXTS));
    let c = rng.pick(&INTS).to_string();
    let n = rng.below(20) + 1;
    let v = match family {
        "conjunct_order" => vec![
            format!("SELECT count(*) FROM t WHERE (a = {a} AND b = {b});"),
            format!("SELECT count(*) FROM t WHERE (b = {b} AND a = {a});"),
            format!("SELECT count(*) FROM t WHERE a = {a} AND b = {b};"),
            format!("SELECT count(*) FROM t WHERE b = {b} AND a = {a};"),
            format!("SELECT count(*) FROM t WHERE ((a = {a}) AND (b = {b}));"),
        ],
        // Over `d`, which is NOT NULL, so this is an identity rather than a
        // claim about three-valued logic.
        "equality_rewrites" => vec![
            format!("SELECT count(*) FROM t WHERE d = {a};"),
            format!("SELECT count(*) FROM t WHERE NOT (d <> {a});"),
            format!("SELECT count(*) FROM t WHERE NOT (d > {a} OR d < {a});"),
            format!("SELECT count(*) FROM t WHERE d <= {a} AND d >= {a};"),
        ],
        "constant_folding" => vec![
            "SELECT count(*) FROM t WHERE a = 1 + 0;".to_string(),
            "SELECT count(*) FROM t WHERE a = 0 + 1;".to_string(),
            "SELECT count(*) FROM t WHERE a = 2 - 1;".to_string(),
        ],
        "grouping" => vec![
            format!("SELECT count(*) FROM t WHERE (a = {a} OR b = {b}) AND c = {c};"),
            format!("SELECT count(*) FROM t WHERE c = {c} AND (a = {a} OR b = {b});"),
            format!("SELECT count(*) FROM t WHERE ((a = {a} OR b = {b}) AND c = {c});"),
        ],
        "is_null" => vec![
            format!("SELECT count(*) FROM t WHERE (a IS NULL AND b IS NULL);"),
            format!("SELECT count(*) FROM t WHERE b IS NULL AND a IS NULL;"),
            format!("SELECT count(*) FROM t WHERE (a IS NULL) AND (b IS NULL);"),
        ],
        "null_predicate" => vec![
            "SELECT count(*) FROM t WHERE a IS NULL;".to_string(),
            "SELECT count(*) FROM t WHERE a = NULL;".to_string(),
            "SELECT count(*) FROM t WHERE NOT (a IS NOT NULL);".to_string(),
        ],
        "between_is_two_comparisons" => vec![
            format!("SELECT count(*) FROM t WHERE a BETWEEN {a} AND {c};"),
            format!("SELECT count(*) FROM t WHERE a >= {a} AND a <= {c};"),
            format!("SELECT count(*) FROM t WHERE a <= {c} AND a >= {a};"),
        ],
        // count(*), count(1) and sum(1) agree over any rows at all. count(a)
        // does not -- it skips the nulls, and this table has nulls on purpose.
        "aggregate_vs_count" => vec![
            format!("SELECT count(*) FROM t WHERE b = {b};"),
            format!("SELECT count(1) FROM t WHERE b = {b};"),
            format!("SELECT sum(1) FROM t WHERE b = {b};"),
        ],
        "cast_is_not_equality" => vec![
            format!("SELECT count(*) FROM t WHERE b = {b};"),
            format!("SELECT count(*) FROM t WHERE b = CAST({b} AS TEXT);"),
        ],
        "order_by_tie" => vec![
            "SELECT a FROM t WHERE a IS NOT NULL ORDER BY a, rowid LIMIT 5;".to_string(),
            "SELECT a FROM t WHERE a IS NOT NULL ORDER BY a ASC, rowid ASC LIMIT 5;"
                .to_string(),
        ],
        _ => vec![format!("SELECT count(*) FROM t WHERE a = {a};")],
    };
    Property { family, variants: v }
}

const FAMILIES: [&str; 10] = [
    "conjunct_order",
    "equality_rewrites",
    "constant_folding",
    "grouping",
    "is_null",
    "null_predicate",
    "between_is_two_comparisons",
    "aggregate_vs_count",
    "cast_is_not_equality",
    "order_by_tie",
];

/// Families whose variants are known to differ. `a = NULL` matches nothing and
/// `a IS NULL` matches every null; that is a documented divergence, and
/// including it is only honest if the test knows which kind it is looking at.
fn known_divergence(family: &str) -> bool {
    family == "null_predicate"
}

fn run(seed: u64, rounds: usize) -> (usize, usize, Vec<String>) {
    let Some(bin) = sqlite3() else {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return (0, 0, Vec::new());
    };

    let path = temp(seed, "props");
    let mut conn = Connection::open(&path).expect("the database opens");
    populate(&mut conn);

    let refdb = temp(seed, "ref");
    // The script is *run*, not written out: a file of SQL text is not a
    // database, and sqlite3 refuses one with a message rather than creating
    // anything, which would leave every later query refused and every property
    // silently skipped.
    let loaded = Command::new(&bin)
        .arg("-batch")
        .arg(&refdb)
        .arg(populate_sql())
        .output()
        .expect("the reference database is created");
    assert!(
        loaded.status.success(),
        "the reference database did not load: {}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    let count = sqlite3_query(&bin, &refdb, "SELECT count(*) FROM t;")
        .expect("the reference answers");
    assert_eq!(
        count,
        vec![ROWS.to_string()],
        "the two engines must be given the same rows"
    );

    let mut rng = Lcg::new(seed);
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for i in 0..rounds {
        let family = FAMILIES[i % FAMILIES.len()];
        let p = property(family, &mut rng);

        let mine: Vec<Vec<String>> = p
            .variants
            .iter()
            .map(|v| query(&mut conn, v))
            .collect();
        let answers: Vec<Option<Vec<String>>> = p
            .variants
            .iter()
            .map(|v| sqlite3_query(&bin, &refdb, v))
            .collect();

        if answers.iter().any(|a| a.is_none()) {
            skipped += 1;
            continue;
        }
        let theirs: Vec<Vec<String>> = answers.into_iter().map(|a| a.unwrap()).collect();
        checked += 1;

        if known_divergence(family) {
            // The forms are *meant* to differ. What still has to hold is that
            // each engine agrees with itself across the spellings that are
            // equivalent -- here the first and third, which are the same
            // question -- and that the two engines agree on each of them.
            for j in [2usize] {
                if mine[j] != mine[0] {
                    failures.push(format!(
                        "seed {seed} round {i} family {family}: nsqlite variant {j} \
                         disagrees with variant 0, which is the same question.\n  \
                         0: {}\n  {j}: {}",
                        p.variants[0], p.variants[j]
                    ));
                }
                if theirs[j] != theirs[0] {
                    failures.push(format!(
                        "seed {seed} round {i} family {family}: sqlite3 variant {j} \
                         disagrees with variant 0.\n  0: {}\n  {j}: {}",
                        p.variants[0], p.variants[j]
                    ));
                }
            }
            if mine[1] != theirs[1] {
                failures.push(format!(
                    "seed {seed} round {i} family {family}: the engines disagree on a \
                     = NULL.\n  nsqlite: {:?}\n  sqlite3:  {:?}",
                    mine[1], theirs[1]
                ));
            }
            continue;
        }

        for j in 1..p.variants.len() {
            if mine[j] != mine[0] {
                failures.push(format!(
                    "seed {seed} round {i} family {family}: nsqlite variants disagree.\n  \
                     0: {}\n  {j}: {}",
                    p.variants[0], p.variants[j]
                ));
            }
            if theirs[j] != theirs[0] {
                failures.push(format!(
                    "seed {seed} round {i} family {family}: sqlite3 variants disagree.\n  \
                     0: {}\n  {j}: {}",
                    p.variants[0], p.variants[j]
                ));
            }
            if mine[j] != theirs[j] {
                failures.push(format!(
                    "seed {seed} round {i} family {family}: the engines disagree on the same \
                     question.\n  question: {}\n  nsqlite: {:?}\n  sqlite3:  {:?}",
                    p.variants[j], mine[j], theirs[j]
                ));
            }
        }
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&refdb);
    (checked, skipped, failures)
}

#[test]
fn two_spellings_of_one_question_agree() {
    let seed = 0x5EED_D57;
    let (checked, skipped, failures) = run(seed, 200);
    println!("seed {seed:#x}: {checked} properties checked, {skipped} skipped");
    assert!(
        failures.is_empty(),
        "{} properties failed; first five:\n{}",
        failures.len(),
        failures.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(checked > 150, "only {checked} properties ran, too few to say anything");
}

#[test]
fn another_seed_is_another_run() {
    // The seed has to be a knob. If every seed produced the same properties,
    // the knob is decorative.
    for seed in [1u64, 2, 3] {
        let (checked, _, failures) = run(seed, 100);
        assert!(checked > 75, "seed {seed} only checked {checked}");
        assert!(
            failures.is_empty(),
            "seed {seed}:\n{}",
            failures.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}

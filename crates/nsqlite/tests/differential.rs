//! A differential test: a seeded generator of SQL, run through `tools/difftest.sh`
//! against the real `sqlite3` and this engine, reporting every disagreement.
//!
//! The point of this file is not to assert a fixed set of expected results. It
//! is to *generate* cases from a seed, write them out, and let the differential
//! runner compare the two engines. That way a disagreement is a reproducible
//! artifact: the seed is printed with the failure, and re-running with the same
//! seed produces exactly the same statements.
//!
//! The generator is deterministic. It is driven by a xorshift64* PRNG seeded
//! from the test's seed, so nothing depends on the clock, on hash iteration
//! order, or on how many cases a previous run happened to take. Given a seed
//! the byte stream of statements is fixed.
//!
//! # What it generates
//!
//! * `CREATE TABLE` with assorted column types -- INTEGER, REAL, TEXT, BLOB,
//!   NUMERIC, an untyped column (BLOB affinity), and the INT/CHAR/CLOB
//!   spellings that carry a rule rather than a type -- and assorted
//!   constraints: NOT NULL, UNIQUE, PRIMARY KEY, DEFAULT, CHECK, and a
//!   composite table constraint.
//! * `INSERT` with a literal of every storage class, including the ones that
//!   are easy to conflate: `0` and `0.0`, `''` and `NULL`, `x'31'` and `'1'`,
//!   and the integer and text spellings of the same digits.
//! * expressions over those columns: arithmetic, concatenation, comparison,
//!   the scalar functions, CAST, and CASE.
//! * `SELECT` with WHERE, ORDER BY, LIMIT, OFFSET, GROUP BY, aggregates, and
//!   DISTINCT.
//!
//! # The comparisons that matter
//!
//! Every case is compared through a *typed projection* rather than through the
//! text the two shells print, so the comparison can tell values apart that look
//! the same when printed. `tools/difftest.sh` does the rewriting; see its header
//! for the full argument. In short, each output column becomes
//!
//! ```text
//! hex(typeof(<expr>) || '~' || quote(<expr>))
//! ```
//!
//! which keeps these apart, and each of them is a class of bug the printed
//! output cannot see:
//!
//! * NULL and the empty string. Both print as nothing on one side or the
//!   other, but `quote` gives them `NULL` and `''`.
//! * the real `1.0` and the integer `1`. Both print as `1` in this engine's
//!   own shell, but `typeof` gives `real` and `integer`.
//! * the blob `x'31'` and the text `'1'`. The blob is one byte and the text
//!   is one byte, and `quote` renders them `X'31'` and `'1'`.
//! * the integer `1` and the text `'1'`, which compare *equal* under SQL's
//!   numeric coercion but are different storage classes.
//!
//! DISTINCT is the shape that leans on this hardest, because there the storage
//! class decides *which rows survive* rather than only how a surviving row
//! prints: `1` and `1.0` are one group, while NULL and `''` are two. So the
//! generator emits DISTINCT over one column, over the whole row, and combined
//! with ORDER BY and LIMIT.
//!
//! The projection covers the result list, not the query shape. A statement
//! whose result list cannot be rewritten -- a compound select, a bare VALUES,
//! a `SELECT *` from a join -- is compared on the values as the two shells
//! render them, which cannot separate a real from an integer, and the runner
//! counts and reports those separately so the count of strict comparisons is
//! not confused with the count of agreements.
//!
//! # Running it
//!
//! The test needs both the `nsqlited` CLI and the real `sqlite3`, so it skips
//! rather than fails when either is missing -- a machine without them cannot
//! run a differential test, and that is not a defect in the engine. Point
//! `NSQLITED` at a non-default build, or `DIFFTEST_BIN` at the runner, to test a
//! build outside `target/debug`:
//!
//! ```text
//! NSQLITED=target/diff/debug/nsqlited.exe cargo test -p nsqlite --test differential
//! DIFFTEST_BIN=/path/to/difftest.sh cargo test -p nsqlite --test differential -- --nocapture
//! ```
//!
//! `DIFFTEST_CASES` sets how many cases to generate (default 400) and
//! `DIFFTEST_SEED` overrides the seed. Passing a seed by hand is how a reported
//! disagreement is re-checked.

use std::path::{Path, PathBuf};
use std::process::Command;

// --- the PRNG --------------------------------------------------------------

/// A xorshift64* generator.
///
/// Chosen because it is short enough to be obviously deterministic: the state
/// advances through three fixed shifts and xors, and the seed is the only
/// input. `rand` is not a dependency of this crate and adding one for a test
/// generator would be heavier than the generator itself.
#[derive(Debug, Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> XorShift64 {
        // A zero state is a fixed point of xorshift, so it is replaced with a
        // constant. Any non-zero value works; this one is arbitrary but fixed.
        XorShift64 {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..n`. `n` is never 0 in this file.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// A value in `lo..=hi`.
    fn between(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        lo + (self.next_u64() % ((hi - lo + 1) as u64)) as i64
    }

    /// True with probability `num/den`.
    fn chance(&mut self, num: u32, den: u32) -> bool {
        (self.next_u64() % den as u64) < num as u64
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// --- literals --------------------------------------------------------------

/// A SQL literal, held as the text that goes into a statement.
///
/// Every storage class gets a representation, and several get more than one so
/// the generator can produce the pairs that are easy to get wrong: an integer
/// and a real with the same value, an empty string and NULL, a blob and a text
/// with the same bytes.
#[derive(Debug, Clone, PartialEq)]
enum Lit {
    Null,
    Int(i64),
    Real(String),
    Text(String),
    Blob(Vec<u8>),
}

impl Lit {
    /// The literal as it appears in a statement.
    fn sql(&self) -> String {
        match self {
            Lit::Null => "NULL".to_string(),
            Lit::Int(i) => i.to_string(),
            // The text is already a valid float spelling, kept as written so a
            // literal that needs an exponent keeps it.
            Lit::Real(s) => s.clone(),
            Lit::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Lit::Blob(b) => {
                let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
                format!("x'{hex}'")
            }
        }
    }
}

/// The literal pool a generated INSERT draws from.
///
/// The pool is deliberately full of confusable pairs. `Int(1)` and `Real("1.0")`
/// are equal under SQL comparison but different storage classes; `Text("")` and
/// `Null` both print as nothing; `Blob(vec![b'1'])` and `Text("1")` hold the same
/// byte. A comparison that conflated any of these would pass while the engine is
/// wrong, so the generator emits them often.
fn literal_pool() -> Vec<Lit> {
    vec![
        Lit::Null,
        Lit::Int(0),
        Lit::Int(1),
        Lit::Int(-1),
        Lit::Int(2),
        Lit::Int(42),
        Lit::Int(-42),
        Lit::Int(1000),
        Lit::Int(i64::MAX),
        Lit::Int(i64::MIN),
        Lit::Real("0.0".to_string()),
        Lit::Real("1.0".to_string()),
        Lit::Real("-1.0".to_string()),
        Lit::Real("0.5".to_string()),
        Lit::Real("2.5".to_string()),
        Lit::Real("-0.0".to_string()),
        Lit::Real("1e300".to_string()),
        Lit::Real("1.5e-8".to_string()),
        Lit::Real("3.0".to_string()),
        Lit::Text(String::new()),
        Lit::Text("1".to_string()),
        Lit::Text("0".to_string()),
        Lit::Text("abc".to_string()),
        Lit::Text("ABC".to_string()),
        Lit::Text("a'b".to_string()),
        Lit::Text("a,b".to_string()),
        Lit::Text(" pad ".to_string()),
        // A newline and a pipe: the characters a line- or pipe-separated
        // comparison would most easily mistake for a boundary.
        Lit::Text("a\nb".to_string()),
        Lit::Text("a|b".to_string()),
        Lit::Text("héllo".to_string()),
        Lit::Blob(Vec::new()),
        Lit::Blob(vec![0x00]),
        Lit::Blob(vec![b'1']),
        Lit::Blob(vec![0xDE, 0xAD]),
        Lit::Blob(vec![0x00, 0xFF, 0x7F]),
    ]
}

// --- schema ----------------------------------------------------------------

/// A column's declared type and constraints, as the generator emits them.
#[derive(Clone)]
struct Column {
    name: &'static str,
    ty: &'static str,
    constraints: &'static str,
}

impl Column {
    fn new(name: &'static str, ty: &'static str, constraints: &'static str) -> Column {
        Column {
            name,
            ty,
            constraints,
        }
    }

    /// The column as it appears in a CREATE TABLE.
    ///
    /// The constraints are separated by a space, because `INTEGERPRIMARY KEY`
    /// is not two tokens: without the space the type name and the constraint run
    /// together and both engines read it as a syntax error, which would make
    /// every constraint case compare two identical failures and test nothing.
    fn sql(&self) -> String {
        let mut s = format!("{} {}", self.name, self.ty);
        if !self.constraints.is_empty() {
            s.push(' ');
            s.push_str(self.constraints);
        }
        s
    }
}

/// The column sets a generated table is built from.
///
/// The types are the ones that carry different affinity rules, because affinity
/// is where a value's storage class can be changed on the way into a table:
/// `INTEGER` converts a real that is exactly an integer, `REAL` converts an
/// integer, `TEXT` converts a number to text, `NUMERIC` converts text that looks
/// like a number, and no declared type at all means BLOB affinity, which
/// converts nothing. The constraints are the ones whose violation is an error
/// rather than a coercion, so a case that violates one must be refused by both
/// engines with the same message.
fn column_sets() -> Vec<Vec<Column>> {
    vec![
        vec![
            Column::new("a", "INTEGER", ""),
            Column::new("b", "REAL", ""),
            Column::new("c", "TEXT", ""),
        ],
        vec![
            Column::new("a", "INTEGER", "NOT NULL"),
            Column::new("b", "TEXT", "DEFAULT 'dflt'"),
            Column::new("c", "NUMERIC", ""),
        ],
        vec![
            Column::new("a", "INT", ""),
            Column::new("b", "CHAR(10)", ""),
            Column::new("c", "CLOB", ""),
        ],
        vec![
            Column::new("a", "INTEGER", "PRIMARY KEY"),
            Column::new("b", "REAL", ""),
            Column::new("c", "TEXT", "UNIQUE"),
        ],
        vec![
            Column::new("a", "", ""),
            Column::new("b", "BLOB", "NOT NULL"),
            Column::new("c", "VARCHAR(5)", "CHECK(length(c) <= 5)"),
        ],
        vec![
            Column::new("a", "INTEGER", "DEFAULT 0"),
            Column::new("b", "REAL", "DEFAULT 1.5"),
            Column::new("c", "TEXT", "DEFAULT 'x'"),
        ],
    ]
}

// --- the generator ---------------------------------------------------------

/// A generated case: the table it runs against, the statements, and a label.
///
/// Every case is self-contained: it creates its own table, named after its index
/// in the corpus, so a case's statements depend on its own setup and on
/// nothing that came before it. That matters because the whole corpus is run as
/// one script through one pair of databases, and a single shared table name
/// made every case after the first read the previous case's schema.
struct Case {
    /// The CREATE TABLE and INSERTs that set up the table.
    setup: Vec<String>,
    /// The statement under test.
    probe: String,
}

impl Case {
    /// Every statement of the case, in order, as one script.
    fn script(&self) -> String {
        let mut s = String::new();
        for st in &self.setup {
            s.push_str(st);
            s.push_str(";\n");
        }
        s.push_str(&self.probe);
        s.push(';');
        s
    }
}

/// Builds one case from the PRNG.
fn generate(rng: &mut XorShift64, index: usize) -> Case {
    let sets = column_sets();
    let cols: Vec<Column> = rng.pick(&sets).clone();
    // Every case gets its own table name.
    //
    // The name used to be `t` for all of them, and since the whole corpus is
    // written to one file and run as a script, every case after the first
    // began with `CREATE TABLE t(...)` against a table that already existed.
    // So a case whose own schema the engine accepts reported
    // `table t already exists` and every statement after it ran against the
    // *previous* case's table, or against nothing at all. In a 400-case run
    // that produced 738 raw disagreements of which 704 were downstream of a
    // single genuine parse bug, and the cases that were not downstream were
    // still being read against a foreign schema, so a difference in the answer
    // could not be attributed to the case being run.
    //
    // A per-case name makes each case's statements depend only on its own setup.
    // Nothing else has to change: the tables accumulate, which is harmless, and
    // a disagreement is now about the case that reported it.
    let table = format!("t{index}");
    let table = table.as_str();
    let mut setup = vec![format!(
        "CREATE TABLE {table}({})",
        cols.iter().map(|c| c.sql()).collect::<Vec<_>>().join(", ")
    )];

    // Rows. Each INSERT names its columns explicitly, so a row that supplies
    // fewer values than the table has takes the declared defaults -- which is
    // itself worth comparing.
    let pool = literal_pool();
    let nrows = 1 + rng.below(5);
    for _ in 0..nrows {
        let ncols = 1 + rng.below(cols.len());
        let taken: Vec<Lit> = (0..ncols).map(|_| rng.pick(&pool).clone()).collect();
        let names: Vec<&str> = cols.iter().take(ncols).map(|c| c.name).collect();
        let vals: Vec<String> = taken.iter().map(|l| l.sql()).collect();
        setup.push(format!(
            "INSERT INTO {table}({}) VALUES({})",
            names.join(", "),
            vals.join(", ")
        ));
    }

    let probe = match rng.below(9) {
        0 => probe_arithmetic(rng, table, &cols),
        1 => probe_functions(rng, table, &cols),
        2 => probe_where(rng, table, &cols),
        3 => probe_order_limit(rng, table, &cols),
        4 => probe_aggregate(rng, table, &cols),
        5 => probe_cast_case(rng, table, &cols),
        6 => probe_constants(rng, &pool),
        7 => probe_projection(rng, table, &cols),
        _ => probe_distinct(rng, table, &cols),
    };
    Case { setup, probe }
}

/// Picks a column name from the table.
fn a_col<'a>(rng: &mut XorShift64, cols: &'a [Column]) -> &'a str {
    rng.pick(cols).name
}

/// A binary operator, weighted towards the ones whose NULL and numeric-coercion
/// behaviour is the most intricate.
fn binop(rng: &mut XorShift64) -> &'static str {
    const OPS: &[&str] = &[
        "+", "-", "*", "/", "%", "||", "=", "<>", "<", "<=", ">", ">=", "AND", "OR",
    ];
    rng.pick(OPS)
}

/// A scalar expression over one column, an alias, and a literal.
fn scalar_expr(rng: &mut XorShift64, cols: &[Column], pool: &[Lit]) -> String {
    let col = a_col(rng, cols);
    let lit = rng.pick(pool).sql();
    match rng.below(6) {
        0 => col.to_string(),
        1 => lit,
        2 => format!("{col} {} {}", binop(rng), lit),
        3 => format!("({col} {} {lit})", binop(rng)),
        4 => format!("-({col})"),
        _ => format!("{col} {} {lit}", rng.pick(&["+", "-", "*", "||"])),
    }
}

/// Arithmetic and concatenation over the table's columns.
fn probe_arithmetic(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let pool = literal_pool();
    let n = 1 + rng.below(3);
    let exprs: Vec<String> = (0..n).map(|_| scalar_expr(rng, cols, &pool)).collect();
    format!("SELECT {} FROM {table}", exprs.join(", "))
}

/// The scalar functions, over a column or a literal.
///
/// Only functions the engine is documented to have are used, so a difference
/// here is a difference in the answer and not in what is implemented. The
/// function is always aliased, because the two engines generate different
/// names for an unaliased expression column and the differential runner has to
/// compare values, not generated names.
fn probe_functions(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let pool = literal_pool();
    let col = a_col(rng, cols);
    let lit = rng.pick(&pool).sql();
    let n = 1 + rng.below(3);
    let mut parts = Vec::new();
    for i in 0..n {
        let arg = match i % 2 {
            0 => col.to_string(),
            _ => lit.clone(),
        };
        let call = match rng.below(12) {
            0 => format!("abs({arg})"),
            1 => format!("coalesce({arg}, {lit})"),
            2 => format!("ifnull({arg}, {lit})"),
            3 => format!("nullif({arg}, {lit})"),
            4 => format!("length({arg})"),
            5 => format!("lower({arg})"),
            6 => format!("upper({arg})"),
            7 => format!("trim({arg})"),
            8 => format!("instr({arg}, {lit})"),
            9 => format!("replace({arg}, {lit}, 'Z')"),
            10 => format!("typeof({arg})"),
            _ => format!("hex({arg})"),
        };
        parts.push(format!("{call} AS f{i}"));
    }
    format!("SELECT {} FROM {table}", parts.join(", "))
}

/// A WHERE clause: comparison, IS NULL, BETWEEN, IN, LIKE, or a compound.
fn probe_where(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let pool = literal_pool();
    let col = a_col(rng, cols);
    let lit = rng.pick(&pool).sql();
    let pred = match rng.below(8) {
        0 => format!("{col} IS NULL"),
        1 => format!("{col} IS NOT NULL"),
        2 => format!("{col} = {lit}"),
        3 => format!(
            "{col} BETWEEN {} AND {}",
            rng.pick(&pool).sql(),
            rng.pick(&pool).sql()
        ),
        4 => format!(
            "{col} IN ({}, {})",
            rng.pick(&pool).sql(),
            rng.pick(&pool).sql()
        ),
        5 => format!(
            "{col} NOT IN ({}, {})",
            rng.pick(&pool).sql(),
            rng.pick(&pool).sql()
        ),
        6 => format!("{col} LIKE '%a%'"),
        _ => format!("({col} {op} {lit} OR {col} IS NULL)", op = binop(rng)),
    };
    format!(
        "SELECT {} FROM {table} WHERE {pred}",
        cols.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
    )
}

/// ORDER BY with a direction, and LIMIT or OFFSET.
fn probe_order_limit(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let col = a_col(rng, cols);
    let dir = if rng.chance(1, 2) { "DESC" } else { "ASC" };
    let mut q = format!(
        "SELECT {} FROM {table} ORDER BY {col} {dir}",
        cols.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
    );
    if rng.chance(2, 3) {
        q.push_str(&format!(" LIMIT {}", rng.below(4)));
    }
    if rng.chance(1, 3) {
        q.push_str(&format!(" OFFSET {}", rng.below(3)));
    }
    q
}

/// GROUP BY with an aggregate.
fn probe_aggregate(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let col = a_col(rng, cols);
    let agg = match rng.below(5) {
        0 => "count(*)".to_string(),
        1 => format!("count({col})"),
        2 => format!("sum({col})"),
        3 => format!("avg({col})"),
        _ => format!("min({col})"),
    };
    let sel = match rng.chance(1, 2) {
        true => format!("{col}, {agg} AS agg"),
        false => format!("{agg} AS agg"),
    };
    match rng.chance(1, 2) {
        true => format!("SELECT {sel} FROM {table} GROUP BY {col}"),
        false => format!("SELECT {sel} FROM {table}"),
    }
}

/// CAST to each storage class, and CASE.
fn probe_cast_case(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let pool = literal_pool();
    let col = a_col(rng, cols);
    let lit = rng.pick(&pool).sql();
    let expr = if rng.chance(1, 2) {
        let ty = rng.pick(&["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC"]);
        format!("CAST({col} AS {ty})")
    } else {
        format!(
            "CASE WHEN {col} IS NULL THEN 'null' WHEN {col} {op} {lit} THEN 'yes' ELSE 'no' END",
            op = rng.pick(&["=", ">", "<"])
        )
    };
    format!("SELECT {expr} AS e FROM {table}")
}

/// A query with no table at all: only literals and expressions over them.
///
/// These are the cases that pin down the value model itself, with no affinity
/// or storage in the way.
fn probe_constants(rng: &mut XorShift64, pool: &[Lit]) -> String {
    let n = 1 + rng.below(4);
    let mut parts = Vec::new();
    for i in 0..n {
        let l = rng.pick(pool).sql();
        let e = match rng.below(4) {
            0 => l,
            1 => format!("{l} {} {}", binop(rng), rng.pick(pool).sql()),
            2 => format!("typeof({l})"),
            _ => format!("quote({l})"),
        };
        parts.push(format!("{e} AS e{i}"));
    }
    format!("SELECT {}", parts.join(", "))
}

/// Every column of the table, projected.
fn probe_projection(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let names: Vec<&str> = cols.iter().map(|c| c.name).collect();
    let _ = rng;
    format!("SELECT {} FROM {table}", names.join(", "))
}

/// `SELECT DISTINCT`, over one column or over the whole row.
///
/// DISTINCT is the query shape where the *storage class* decides which rows
/// survive, which makes it the sharpest probe of the value model there is:
/// the integer `1` and the real `1.0` are one group under DISTINCT but two
/// classes under the projection, while NULL and the empty string are two
/// groups. An engine that collapsed a real onto an integer on the way into a
/// table would return a different number of rows here, and an engine that
/// conflated NULL with '' would as well.
///
/// It is also the shape the runner had the least coverage of and the weakest
/// comparison for. `SELECT DISTINCT a FROM t` reads as *one* result item to
/// the runner's result-list splitter -- the keyword is part of the first item,
/// not a separate one -- so the item count came out one above the column
/// count, the shape check failed, and the statement dropped to the untyped
/// comparison, which cannot tell a real from an integer at all. The runner now
/// lifts DISTINCT off the result list and re-attaches it in front of the
/// projection, so this gets the strict comparison; emitting it here is what
/// makes that fix observable.
///
/// The rows behind these probes come from `literal_pool`, so the duplicates
/// that matter -- `Int(1)` beside `Real("1.0")`, `Null` beside `Text("")`,
/// `Blob(b'1')` beside `Text("1")` -- are present by construction.
fn probe_distinct(rng: &mut XorShift64, table: &str, cols: &[Column]) -> String {
    let names: Vec<&str> = cols.iter().map(|c| c.name).collect();
    match rng.below(3) {
        // One column: the case where a real and an integer collapse.
        0 => format!("SELECT DISTINCT {} FROM {table}", a_col(rng, cols)),
        // The whole row: DISTINCT over a tuple, where NULL and '' differ.
        1 => format!("SELECT DISTINCT {} FROM {table}", names.join(", ")),
        // DISTINCT with an ORDER BY and a LIMIT on top, so the two clauses
        // have to survive being lifted apart as well.
        _ => {
            let col = a_col(rng, cols);
            format!(
                "SELECT DISTINCT {col} FROM {table} ORDER BY {col} LIMIT {}",
                1 + rng.below(3)
            )
        }
    }
}

// --- the test --------------------------------------------------------------

/// The runner script, overridable so a build outside `target/debug` can be
/// tested without editing this file.
fn difftest_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST_BIN") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/difftest.sh")
        .to_path_buf()
}

/// The shell that runs the runner script.
///
/// The script is POSIX sh, so any bash works, but *which* bash matters on
/// Windows: `bash` on the Windows PATH is often the WSL launcher, which cannot
/// see a `D:\` path at all and exits 127 before the script starts. Git Bash,
/// MSYS2 and Cygwin all can, so `DIFFTEST_BASH` names one explicitly.
///
/// When it is unset, no candidate is assumed to work -- each one is *probed*,
/// and the first that can actually run a script from a Windows drive path
/// wins. Trusting the name is what left this test failing on a box where
/// `C:\Windows\System32\bash.exe` is a PATH entry: the WSL launcher printed a
/// WSL-install message to stderr and exited 127, and both
/// `the_runner_compares_strictly` and `generated_cases_agree_with_real_sqlite`
/// reported that the differential runner did nothing, which reads as an engine
/// failure and is not one. The probe is the whole fix; a candidate is only
/// accepted if it answers the question the runner depends on.
///
/// Every PATH entry is tried, not just the first, because the first is the one
/// that cannot work. A candidate is skipped when it is the same file as one
/// already tried, since Windows lists a shell under several PATH entries and
/// re-running the WSL launcher three more times finds no shell in it.
fn difftest_bash() -> PathBuf {
    if let Ok(p) = std::env::var("DIFFTEST_BASH") {
        return PathBuf::from(p);
    }
    let probe = probe_script_path();
    if std::fs::write(&probe, "printf 'nsqlite-bash-probe\\n'\n").is_err() {
        // No probe means no way to tell a shell from the WSL launcher, so fall
        // back to the bare name and let the shell's own failure speak.
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
    // Nothing probed worked. Fall back to the bare name so the failure the
    // caller sees is the shell's own, not a "no bash found" invented here.
    PathBuf::from("bash")
}

/// Every `bash` this machine could mean, best guess first.
///
/// The bare name is resolved against PATH here rather than handed to the
/// spawner, because the spawner takes the first hit and the first hit is the
/// WSL launcher. The well-known install locations follow, so a box whose PATH
/// is entirely System32 still finds Git Bash.
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
            r"C:\cygwin64\bin\bash.exe",
        ] {
            out.push(PathBuf::from(p));
        }
    }
    out
}

/// True if `$bash` can run a shell script from a Windows drive path.
///
/// The probe is the runner's own question, asked of a throwaway script: can
/// this shell be handed a `D:\...\difftest.sh` argument and print what the
/// script prints? The WSL launcher answers with a WSL-install message on stderr
/// and nothing on stdout, which is the whole reason this is a probe and not a
/// PATH lookup. The output is the signal, not the exit status, so a launcher
/// that reports success while having run nothing is still rejected.
fn bash_runs_the_script(bash: &Path, probe: &Path) -> bool {
    let arg = windows_path_to_shell(probe);
    let Ok(out) = Command::new(bash).arg(&arg).output() else {
        return false;
    };
    out.status.success() && String::from_utf8_lossy(&out.stdout).contains("nsqlite-bash-probe")
}

/// Where the shell probe is written.
///
/// It has to sit on the same drive as the runner, and it has to be a path this
/// test can hand to a shell without converting. The probe used to go to
/// `std::env::temp_dir()`, which is `C:\Users\...\Temp`, and that was wrong in
/// two separate ways.
///
/// The first is that `C:\` is the one drive the MSYS families disagree about.
/// Cygwin mounts it at `/tmp` and Git Bash and MSYS2 at `/c/Users/.../Temp`, so
/// a path that is correct for one is a dangling path for the other.
///
/// The second, and the one that made the test fail outright, is that the
/// `D:\` drive is where the repository is, and the WSL launcher cannot see it
/// at all. Asked to run `/tmp/probe-target.sh` the launcher reported success
/// and printed the WSL-install banner, but asked to run
/// `/d/Prj-SQLite-Rust/tools/difftest.sh` -- the path the test actually passes
/// -- it printed
/// `/bin/bash: D:/Prj-SQLite-Rust/tools/difftest.sh: No such file or directory`
/// and the test reported that the differential runner had compared nothing.
///
/// A probe that only tests a `C:\` path cannot see that difference, so it
/// accepts the launcher and the real failure happens later. The probe is
/// therefore written beside the runner, on the repository's own drive, which is
/// the drive the paths under test are on.
fn probe_script_path() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("nsqlite-difftest-bash-probe.sh")
}

/// `C:\dir\file` as `/c/dir/file`, for a shell that cannot take a drive path.
///
/// This is deliberately *not* done with a bare `cygpath` from PATH. Each MSYS
/// family mounts a drive at its own point -- Cygwin maps `C:\Users\...\Temp` to
/// `/tmp`, while Git Bash and MSYS2 map it to `/c/Users/.../Temp` -- so a
/// `cygpath` belonging to one family hands the other a path that does not
/// resolve. That is not hypothetical: the bare `cygpath` on this machine is
/// Cygwin's, and asking it for the probe file gave `/tmp/...`, which the Git
/// Bash that had passed the probe could not open:
///
/// ```text
/// /usr/bin/bash: C:/Users/zyq/AppData/Local/Temp/nsqlite-difftest-bash-probe.sh: No such file or directory
/// ```
///
/// so every candidate after Cygwin was rejected, no shell was found, and the
/// test fell back to the bare name -- which resolved to the WSL launcher, or to
/// whichever bash happened to be first, and reported that the runner had done
/// nothing. The drive-letter form is used unconditionally instead: every one of
/// these shells agrees that `C:` is `/c/` and `D:` is `/d/`, which is the
/// convention the runner's own `mktemp -d "${TMPDIR:-/tmp}"` also relies on.
fn windows_path_to_shell(p: &Path) -> String {
    let raw = p.to_string_lossy().to_string();
    if cfg!(windows) {
        // A leading \\?\ or //?/ UNC-style prefix is not a drive path and is
        // left alone.
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

/// How many cases to generate.
///
/// `DIFFTEST_CASES` is the documented name and the only one read. An earlier
/// version checked a misspelled `DIQLITE_CASES` *first* and fell back to the
/// documented name, so a stray `DIQLITE_CASES` in the environment silently
/// overrode the documented variable and changed the corpus size with no
/// warning -- and nothing else in the tree ever set it.
fn case_count() -> usize {
    std::env::var("DIFFTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400)
}

/// The seed, or a fixed default so an unseeded run is still reproducible.
fn seed() -> u64 {
    std::env::var("DIFFTEST_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5EED_1234_ABCD_9F01)
}

/// Writes the generated cases and runs the differential tester over them.
///
/// The cases are written to one file so the runner sees them as a script, and
/// the file is kept when the run fails so the exact statements can be re-run by
/// hand. A failure prints the seed and the path.
#[test]
fn generated_cases_agree_with_real_sqlite() {
    let bin = difftest_bin();
    let cli = nsqlited();
    let Some(real) = find_sqlite3() else {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return;
    };
    if !cli.exists() {
        eprintln!(
            "skipping: {} does not exist; run `cargo build -p nsqlited` first",
            cli.display()
        );
        return;
    }
    if !bin.exists() {
        eprintln!("skipping: {} does not exist", bin.display());
        return;
    }

    let n = case_count();
    let s = seed();
    let mut rng = XorShift64::new(s);
    let cases: Vec<Case> = (0..n).map(|i| generate(&mut rng, i)).collect();

    let dir = std::env::temp_dir().join(format!("nsqlite-differential-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    let path = dir.join("cases.sql");
    let mut script = String::new();
    for c in &cases {
        script.push_str(&c.script());
        script.push('\n');
    }
    std::fs::write(&path, &script).expect("writing the generated cases");

    // The paths handed to the script are converted to the form an MSYS shell
    // understands, because `CARGO_MANIFEST_DIR` is a Windows path
    // (`D:\Prj-...`) and a Git Bash or MSYS2 shell cannot open one. This is the
    // same conversion windows_path_to_shell does, and for the same reason: a
    // bare `cygpath` from PATH belongs to whichever MSYS family is installed,
    // and Cygwin maps `C:\Users\...\Temp` to `/tmp` where Git Bash maps it to
    // `/c/Users/.../Temp`, so its answer is not necessarily usable by the shell
    // that was selected. The drive-letter form is what all of them agree on.
    let for_shell = windows_path_to_shell;

    let out = Command::new(difftest_bash())
        .arg(for_shell(&bin))
        .arg(for_shell(&path))
        .env("NSQLITED", for_shell(&cli))
        .env("SQLITE3", for_shell(&real))
        // The scratch directory is kept for this run so the databases the
        // runner built can be inspected after a failure.
        .env("WORKDIR", for_shell(&dir.join("work")))
        .output()
        .expect("running tools/difftest.sh");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        eprintln!("difftest stderr:\n{stderr}");
    }
    println!("{stdout}");

    // A runner that printed no summary did not compare anything, and a test
    // that passes because it silently did nothing is worse than no test. This
    // catches a broken runner, a missing script, and a `bash` that is not
    // bash, rather than reporting a clean run.
    if !stdout.contains("statements agreed") {
        panic!(
            "the differential runner produced no summary, so nothing was compared.\n\
             status: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );
    }

    if !out.status.success() {
        eprintln!(
            "\nseed: {s} ({n} cases)\ncases: {}\nre-run with: DIFFTEST_SEED={s} cargo test -p nsqlite --test differential",
            path.display()
        );
        panic!("the engines disagreed; the cases are at {}", path.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runner's own text handling, checked through `--self-test`.
///
/// The runner decides how *strictly* a query is compared by splitting
/// statements, finding the result list and stripping aliases, and an error in
/// any of those turns a typed comparison into a looser one without ever saying
/// so. A bug there does not show up as a failure here; it shows up as
/// differential runs quietly agreeing about things that differ. So the
/// runner's self-test runs as part of this test, and a failure in it fails the
/// suite rather than waiting to be noticed in a diff count.
#[test]
fn the_runner_compares_strictly() {
    let bin = difftest_bin();
    if !bin.exists() {
        eprintln!("skipping: {} does not exist", bin.display());
        return;
    }
    let out = Command::new(difftest_bash())
        .arg(&bin)
        .arg("--self-test")
        .output()
        .expect("running tools/difftest.sh --self-test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    assert!(
        out.status.success() && stdout.contains("all checks passed"),
        "the differential runner's self-test failed:\n{stdout}"
    );
}

/// The generator itself, checked without running the engines.
///
/// These assertions are about the generator, not the engine: that a given seed
/// produces the same script every time, that different seeds produce different
/// scripts, and that the confusable literals the comparison depends on are
/// actually in the pool. A generator that stopped emitting them would make the
/// differential test much weaker while still passing, so this is checked
/// directly.
#[test]
fn the_generator_is_deterministic_and_emits_the_hard_cases() {
    let a: Vec<String> = {
        let mut r = XorShift64::new(42);
        (0..50).map(|i| generate(&mut r, i).script()).collect()
    };
    let b: Vec<String> = {
        let mut r = XorShift64::new(42);
        (0..50).map(|i| generate(&mut r, i).script()).collect()
    };
    assert_eq!(a, b, "the same seed must produce the same statements");

    let c: Vec<String> = {
        let mut r = XorShift64::new(43);
        (0..50).map(|i| generate(&mut r, i).script()).collect()
    };
    assert_ne!(a, c, "different seeds must produce different statements");

    // A large batch, to check the whole script rather than 50 samples.
    let big: String = {
        let mut r = XorShift64::new(7);
        let scripts: Vec<String> = (0..500).map(|i| generate(&mut r, i).script()).collect();
        scripts.join("\n")
    };
    // The literal pairs a loose comparison would conflate.
    assert!(big.contains("x'31'"), "a blob holding the digit one");
    assert!(big.contains("'1'"), "the text digit one");
    assert!(big.contains("1.0"), "the real one point zero");
    assert!(big.contains("''"), "the empty string");
    assert!(big.contains("NULL"), "the null literal");
    // The query shapes under test.
    for shape in [
        "CREATE TABLE",
        "INSERT INTO",
        "SELECT",
        "ORDER BY",
        "LIMIT",
        "WHERE",
        "GROUP BY",
        "IS NULL",
        "BETWEEN",
        " IN (",
        "CASE WHEN",
        "CAST(",
        "typeof(",
        "quote(",
        // DISTINCT is the shape where the storage class decides which rows
        // survive, and it is the one the runner's result-list splitter used to
        // mis-handle, so its absence from the corpus would quietly remove both
        // a real class of engine bug and the check on that fix.
        "SELECT DISTINCT ",
    ] {
        assert!(big.contains(shape), "generated cases must include {shape}");
    }
    // A newline inside a string literal, which a line-based comparison would
    // most easily mistake for a row boundary. It reaches the script as a real
    // newline character, because that is what a SQL literal holding one is.
    assert!(big.contains("'a\nb'"), "a value containing a newline");
    // A pipe, which a pipe-separated comparison would most easily mistake for
    // a field boundary.
    assert!(big.contains("'a|b'"), "a value containing a pipe");

    // Every case must create its own table. The corpus is run as one script
    // through one pair of databases, so two cases sharing a table name means
    // the second reads the first's schema: the CREATE is refused as
    // `table t already exists`, and every statement after it is answering a
    // question about the wrong table. In a 400-case run that turned 23 real
    // disagreements into 738, and made every case's result unattributable.
    //
    // The names are checked by counting the distinct `CREATE TABLE` names in a
    // batch, which is the property that matters, rather than by matching one
    // spelling.
    {
        let mut r = XorShift64::new(11);
        let scripts: Vec<String> = (0..200).map(|i| generate(&mut r, i).script()).collect();
        let mut names: Vec<&str> = Vec::new();
        for s in &scripts {
            let start = s.find("CREATE TABLE ").expect("a CREATE TABLE");
            let rest = &s[start + "CREATE TABLE ".len()..];
            let end = rest.find('(').expect("a column list");
            names.push(rest[..end].trim());
        }
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            before,
            "every case must create its own table, or a case reads the previous one's schema"
        );
        assert!(
            scripts[0].contains("CREATE TABLE t0"),
            "case 0 names its table t0"
        );
        assert!(
            scripts[199].contains("CREATE TABLE t199"),
            "case 199 names its table t199"
        );
    }
}

/// The xorshift's own properties, so a bug in it shows up as a generator bug
/// rather than as a mysterious absence of test cases.
#[test]
fn the_prng_covers_its_range_and_never_sticks() {
    let mut r = XorShift64::new(1);
    let mut seen = [false; 7];
    for _ in 0..200 {
        let v = r.below(7);
        assert!(v < 7);
        seen[v] = true;
    }
    assert!(seen.iter().all(|s| *s), "below() must reach every value");

    // A zero seed is replaced, so the generator still moves.
    let mut z = XorShift64::new(0);
    let first = z.next_u64();
    let second = z.next_u64();
    assert_ne!(first, second, "a zero seed must not be a fixed point");

    // between() stays inside its bounds, including a single-value range.
    let mut r = XorShift64::new(9);
    for _ in 0..500 {
        let v = r.between(-3, 3);
        assert!((-3..=3).contains(&v), "between() went out of range: {v}");
    }
    assert_eq!(r.between(5, 5), 5);
}

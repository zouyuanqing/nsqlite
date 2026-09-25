// --- PRAGMA, through the executor ----------------------------------------
//
// This file and `tests/pragma.rs` split the work in two. That file tests the
// grammar and the schema reader as pure functions, which is the right way to
// test them. This one drives `pragma::Exec`, the single entry point a
// connection's `Stmt::Pragma` arm calls.
//
// The split is deliberate, and the seam is worth being explicit about: these
// tests cover the EXECUTOR, not the parser's reachability from `parse_script`.
// Reachability is a change in `parser.rs`, which this track does not own, and
// no test that lives outside the crate can prove it without a `Stmt::Pragma`
// variant existing at all. What these tests do prove is the thing that was
// missing: an unknown pragma really does come back as `Outcome::Nothing`
// rather than as an error or an empty result, and every pragma on the known
// list really does produce the result shape sqlite3 gives it.

use nsqlite::pragma;
use nsqlite::value::Value;

// --- helpers, mirroring the ones in tests/pragma.rs ----------------------

/// Parses a PRAGMA statement.
fn p(sql: &str) -> pragma::Pragma {
    pragma::parse_pragma(sql).unwrap_or_else(|e| panic!("{sql} should parse: {e}"))
}

/// A row rendered the way sqlite3's `.mode list` prints it: values joined by
/// `|`, a NULL as the empty string.
fn render(values: &[Value]) -> String {
    values
        .iter()
        .map(|v| match v {
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The rows a result carried, rendered.
fn rendered(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.iter().map(|r| render(r)).collect()
}

// Runs one statement through the executor against a fresh in-memory state.
fn exec(sql: &str) -> pragma::Outcome {
    let p = p(sql);
    let mut state = pragma::PragmaState::default();
    let mut e = pragma::Exec {
        state: &mut state,
        header: pragma::HeaderScalars::default(),
        create_sql: None,
        databases: &[],
        cache_size: -2000,
    };
    e.run(&p).expect("the executor should answer")
}

/// Runs one statement and returns the rows rendered the way sqlite3 prints
/// them.
fn exec_rows(sql: &str) -> Vec<String> {
    match exec(sql) {
        pragma::Outcome::Result((_, rows)) => rendered(rows),
        pragma::Outcome::Nothing => Vec::new(),
    }
}

/// Runs one statement with a table's CREATE TABLE text available, which is what
/// the introspection pragmas read.
fn exec_rows_with(sql: &str, create: &str) -> Vec<String> {
    let p = p(sql);
    let mut state = pragma::PragmaState::default();
    let mut e = pragma::Exec {
        state: &mut state,
        header: pragma::HeaderScalars::default(),
        create_sql: Some(create),
        databases: &[],
        cache_size: -2000,
    };
    match e.run(&p).expect("the executor should answer") {
        pragma::Outcome::Result((_, rows)) => rendered(rows),
        pragma::Outcome::Nothing => Vec::new(),
    }
}

/// The columns a statement answers under, which is what a harness sees through
/// `sqlite3_column_name` and a CLI sees through an empty result.
fn exec_columns(sql: &str) -> Vec<String> {
    match exec(sql) {
        pragma::Outcome::Result((cols, _)) => cols,
        pragma::Outcome::Nothing => Vec::new(),
    }
}

// sqlite3: PRAGMA nosuchpragma;  -> rc=0, no output, and description IS NULL
//
// This is the assertion the pure-function tests could not make. `Outcome::Nothing`
// is the only value that means "no result set at all", and it is what makes an
// unknown pragma a no-op rather than an error -- which is what unblocks the 19
// suite files that stop at their first PRAGMA.
#[test]
fn an_unknown_pragma_really_produces_no_result_set() {
    for sql in [
        "PRAGMA nosuchpragma",
        "PRAGMA nosuchpragma=1",
        "PRAGMA locking_mode=EXCLUSIVE",
        "PRAGMA trusted_schema=ON",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA wal_autocheckpoint=1000",
        "PRAGMA integrity_check",
    ] {
        assert_eq!(
            exec(sql),
            pragma::Outcome::Nothing,
            "{sql} is a silent no-op, not an error and not an empty result"
        );
    }
}

// sqlite3: PRAGMA full_column_names=on;  -> rc=0, no rows, no columns
// sqlite3: PRAGMA user_version=5;         -> rc=0, no rows, no columns
#[test]
fn an_assignment_produces_no_result_set_either() {
    for sql in [
        "PRAGMA full_column_names=on",
        "PRAGMA short_column_names=0",
        "PRAGMA user_version=5",
        "PRAGMA application_id=7",
        "PRAGMA cache_size=100",
        "PRAGMA page_size=1000",
    ] {
        assert_eq!(
            exec(sql),
            pragma::Outcome::Nothing,
            "{sql} is an assignment, and an assignment has no result set"
        );
    }
}

// sqlite3: PRAGMA table_info(nosuch);  -> 0 rows, and SIX columns named
//                                           cid|name|type|notnull|dflt_value|pk
#[test]
fn an_empty_result_still_carries_its_columns() {
    assert_eq!(exec_rows("PRAGMA table_info(nosuch)"), Vec::<String>::new());
    assert_eq!(
        exec_columns("PRAGMA table_info(nosuch)"),
        vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]
    );
    assert_eq!(
        exec_columns("PRAGMA foreign_key_list(nosuch)"),
        vec![
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match"
        ]
    );
    assert_eq!(
        exec_columns("PRAGMA index_list(nosuch)"),
        vec!["seq", "name", "unique", "origin", "partial"]
    );
    assert_eq!(
        exec_columns("PRAGMA index_info(nosuch)"),
        vec!["seqno", "cid", "name"]
    );
}

// sqlite3: PRAGMA table_info;   -> 0 rows, the six table_info columns
//
// The argument is optional, and a missing one is not an error: the pragma table
// is keyed by name, and `PRAGMA table_info` is the table_info entry with no
// argument bound.
#[test]
fn a_bare_introspection_pragma_keeps_its_columns() {
    assert_eq!(
        exec_columns("PRAGMA table_info"),
        vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]
    );
    assert_eq!(exec_rows("PRAGMA table_info"), Vec::<String>::new());
    assert_eq!(
        exec_columns("PRAGMA main.foreign_key_list"),
        vec![
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match"
        ]
    );
}

// sqlite3: CREATE TABLE t(a,b); PRAGMA table_info(t);
//        -> 0|a||0||0
//           1|b||0||0
#[test]
fn table_info_reads_a_tables_schema_text() {
    assert_eq!(
        exec_rows_with("PRAGMA table_info(t)", "CREATE TABLE t(a,b)"),
        vec!["0|a||0||0", "1|b||0||0"]
    );
}

// sqlite3: PRAGMA page_size;       -> 4096
// sqlite3: PRAGMA encoding;        -> UTF-8
// sqlite3: PRAGMA user_version;    -> 0
// sqlite3: PRAGMA application_id;  -> 0
// sqlite3: PRAGMA journal_mode;    -> delete
// sqlite3: PRAGMA cache_size;      -> -2000
//
// Each is a ONE-COLUMN result whose column is named after the pragma, which is
// the shape the module was missing for four of the eight.
#[test]
fn a_scalar_pragma_answers_one_column_named_after_itself() {
    let cases = [
        ("PRAGMA page_size", "page_size", "4096"),
        ("PRAGMA encoding", "encoding", "UTF-8"),
        ("PRAGMA user_version", "user_version", "0"),
        ("PRAGMA application_id", "application_id", "0"),
        ("PRAGMA schema_version", "schema_version", "0"),
        ("PRAGMA page_count", "page_count", "0"),
        ("PRAGMA journal_mode", "journal_mode", "delete"),
        ("PRAGMA cache_size", "cache_size", "-2000"),
    ];
    for (sql, col, want) in cases {
        assert_eq!(p(sql).name(), col, "{sql} names its own column");
        match exec(sql) {
            pragma::Outcome::Result((cols, rows)) => {
                assert_eq!(cols, vec![col.to_string()], "{sql} has one column");
                assert_eq!(render(&rows[0]), want, "{sql} value");
            }
            pragma::Outcome::Nothing => panic!("{sql} should answer a row"),
        }
    }
}

// sqlite3: PRAGMA full_column_names; -> 0
// sqlite3: PRAGMA short_column_names; -> 1
#[test]
fn the_column_name_flags_read_back_as_zero_and_one() {
    assert_eq!(exec_rows("PRAGMA full_column_names"), vec!["0"]);
    assert_eq!(exec_rows("PRAGMA short_column_names"), vec!["1"]);
}

/// An `Exec` over a caller-owned state, for the tests that assign and then read.
fn exec_over(sql: &str, state: &mut pragma::PragmaState) -> pragma::Outcome {
    let p = p(sql);
    let mut e = pragma::Exec {
        state,
        header: pragma::HeaderScalars::default(),
        create_sql: None,
        databases: &[],
        cache_size: -2000,
    };
    e.run(&p).expect("the executor should answer")
}

// sqlite3: PRAGMA user_version=5; PRAGMA user_version;  -> 5
// sqlite3: PRAGMA user_version=abc; PRAGMA user_version; -> unchanged, no error
#[test]
fn a_header_setting_round_trips_and_ignores_a_value_it_cannot_read() {
    for (set, field, want) in [
        ("PRAGMA user_version=5", 0i8, "5"),
        ("PRAGMA user_version=-5", 0, "-5"),
        ("PRAGMA application_id=7", 1, "7"),
    ] {
        let mut state = pragma::PragmaState::default();
        assert_eq!(
            exec_over(set, &mut state),
            pragma::Outcome::Nothing,
            "{set} is an assignment"
        );
        let got = if field == 0 {
            state.user_version
        } else {
            state.application_id
        };
        assert_eq!(got.to_string(), want, "{set} round-trips");

        // A value that is not an integer leaves the setting alone and is not
        // an error, which is what `PRAGMA user_version=abc` does.
        let bad = format!("PRAGMA {}=abc", p(set).name());
        assert_eq!(
            exec_over(&bad, &mut state),
            pragma::Outcome::Nothing,
            "{bad} is ignored, not an error"
        );
        let still = if field == 0 {
            state.user_version
        } else {
            state.application_id
        };
        assert_eq!(still.to_string(), want, "{bad} changed nothing");
    }
}

// sqlite3: PRAGMA full_column_names=on;  SELECT test1.f1 FROM test1; -> test1.f1
// sqlite3: PRAGMA full_column_names=off;                        -> f1
#[test]
fn the_column_name_flag_reaches_the_result_column_name() {
    for (sql, want) in [
        ("PRAGMA full_column_names=on", "test1.f1"),
        ("PRAGMA full_column_names=off", "f1"),
    ] {
        let mut state = pragma::PragmaState::default();
        assert_eq!(exec_over(sql, &mut state), pragma::Outcome::Nothing);
        // select1 writes the same column both ways, and the source text
        // differs, so both spellings go through the naming rule.
        assert_eq!(
            pragma::column_name(
                state.column_names,
                None,
                Some("test1"),
                Some("f1"),
                "test1.f1"
            ),
            want,
            "{sql}"
        );
    }
}

// sqlite3: PRAGMA journal_mode=memory; PRAGMA journal_mode;  -> memory
// sqlite3: PRAGMA journal_mode=bogus;  PRAGMA journal_mode;  -> delete
//
// journal_mode is the ONE pragma whose assignment answers a result set, so
// this is the one assignment that is not Outcome::Nothing.
#[test]
fn journal_mode_is_the_one_assignment_that_answers() {
    let mut state = pragma::PragmaState::default();
    match exec_over("PRAGMA journal_mode=memory", &mut state) {
        pragma::Outcome::Result((cols, rows)) => {
            assert_eq!(cols, vec!["journal_mode".to_string()]);
            assert_eq!(render(&rows[0]), "memory");
        }
        pragma::Outcome::Nothing => panic!("journal_mode=memory answers a row in sqlite3"),
    }
    // An unrecognised name leaves the mode alone and is not an error, and it
    // still answers the mode it kept.
    match exec_over("PRAGMA journal_mode=bogus", &mut state) {
        pragma::Outcome::Result((_, rows)) => assert_eq!(render(&rows[0]), "memory"),
        pragma::Outcome::Nothing => panic!("journal_mode=bogus still answers the mode"),
    }
}

// sqlite3: PRAGMA database_list;  -> 0|main|<the file>
// sqlite3: in-memory temp         -> 0|temp|        (empty file, not NULL)
#[test]
fn database_list_reports_the_schema_and_the_file() {
    let dbs = vec![
        (0i64, "main".to_string(), "test.db".to_string()),
        (2i64, "temp".to_string(), String::new()),
    ];
    let p = p("PRAGMA database_list");
    let mut state = pragma::PragmaState::default();
    let mut e = pragma::Exec {
        state: &mut state,
        header: pragma::HeaderScalars::default(),
        create_sql: None,
        databases: &dbs,
        cache_size: -2000,
    };
    match e.run(&p).expect("the executor should answer") {
        pragma::Outcome::Result((cols, rows)) => {
            assert_eq!(cols, vec!["seq", "name", "file"]);
            assert_eq!(render(&rows[0]), "0|main|test.db");
            // An in-memory database reports an EMPTY file, not a NULL.
            assert_eq!(render(&rows[1]), "2|temp|");
        }
        pragma::Outcome::Nothing => panic!("database_list answers rows"),
    }
}

// sqlite3: PRAGMA 'page_size';        -> the same as the unquoted spelling
// sqlite3: PRAGMA "page_size";        -> 4096
// sqlite3: PRAGMA [page_size];        -> 4096
#[test]
fn a_quoted_pragma_name_reads_the_same_pragma() {
    for sql in [
        "PRAGMA 'page_size'",
        "PRAGMA \"page_size\"",
        "PRAGMA [page_size]",
    ] {
        assert_eq!(p(sql).name(), "page_size", "{sql}");
        match exec(sql) {
            pragma::Outcome::Result((cols, rows)) => {
                assert_eq!(cols, vec!["page_size".to_string()]);
                assert_eq!(render(&rows[0]), "4096");
            }
            pragma::Outcome::Nothing => panic!("{sql} should answer a row"),
        }
    }
}

// sqlite3: PRAGMA 'table_info'(t);  -> the six columns, and t's rows
#[test]
fn a_quoted_introspection_pragma_reads_its_argument_too() {
    assert_eq!(
        exec_rows_with("PRAGMA 'table_info'(t)", "CREATE TABLE t(a,b)"),
        vec!["0|a||0||0", "1|b||0||0"]
    );
    assert_eq!(
        exec_columns("PRAGMA 'table_info'(nosuch)"),
        vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]
    );
}

// The defaults this module claims are the ones sqlite3 prints on a fresh file.
// Every one of them was read off the binary rather than reasoned about, and the
// list is here so a change to one of them is a visible edit.
#[test]
fn the_scalar_defaults_are_the_ones_sqlite_reports_on_a_fresh_file() {
    for (name, want) in [
        ("page_size", "4096"),
        ("page_count", "0"),
        ("encoding", "UTF-8"),
        ("schema_version", "0"),
        ("user_version", "0"),
        ("application_id", "0"),
        ("journal_mode", "delete"),
        ("cache_size", "-2000"),
    ] {
        assert_eq!(
            pragma::default_scalar(name)
                .map(|v| v.to_string())
                .as_deref(),
            Some(want),
            "PRAGMA {name} on a fresh file"
        );
    }
}

// The state a connection starts from, which is sqlite3's own defaults.
#[test]
fn the_settings_state_defaults_to_sqlites() {
    let s = pragma::PragmaState::default();
    assert!(!s.column_names.full);
    assert!(s.column_names.short);
    assert_eq!(s.user_version, 0);
    assert_eq!(s.application_id, 0);
    assert_eq!(s.journal_mode, pragma::JournalMode::Delete);
    // And the flag struct's own default is the same value.
    assert_eq!(pragma::ColumnNameFlags::default(), s.column_names);
}

// The known set is what separates "not implemented" from "an error", and the
// two must not drift. Unlike the pure-function version of this check, the next
// test confirms every name on this list really does answer.
#[test]
fn every_known_pragma_answers_something_and_no_other_one_does() {
    for name in [
        "full_column_names",
        "short_column_names",
        "table_info",
        "index_list",
        "index_info",
        "foreign_key_list",
        "database_list",
        "user_version",
        "application_id",
        "page_count",
        "page_size",
        "encoding",
        "schema_version",
        "journal_mode",
        "cache_size",
    ] {
        assert!(pragma::is_known(name), "{name} should be answered");
    }
    for name in [
        "locking_mode",
        "trusted_schema",
        "nosuchpragma",
        "temp_store",
        "integrity_check",
        "wal_checkpoint",
    ] {
        assert!(!pragma::is_known(name), "{name} is not implemented");
    }
}

// A pragma on the known list must actually produce a result set. This is what
// catches the two lists drifting apart, which the pure-function version could
// not see: `is_known` returning true for a name the executor then treats as a
// no-op would leave the no-op contract untested.
#[test]
fn a_read_of_every_known_pragma_produces_a_result_set() {
    for name in [
        "full_column_names",
        "short_column_names",
        "table_info",
        "index_list",
        "index_info",
        "foreign_key_list",
        "database_list",
        "user_version",
        "application_id",
        "page_count",
        "page_size",
        "encoding",
        "schema_version",
        "journal_mode",
        "cache_size",
    ] {
        assert!(
            matches!(exec(&format!("PRAGMA {name}")), pragma::Outcome::Result(_)),
            "PRAGMA {name} should answer a result set"
        );
    }
}

// sqlite3: PRAGMA user_version   -> one column, user_version
// sqlite3: PRAGMA user_version=5 -> no columns at all
//
// A read and its assignment are different result shapes, and the difference is
// invisible in row output, which is why it has to be asserted separately.
#[test]
fn a_read_and_its_assignment_are_different_result_shapes() {
    assert_eq!(exec_columns("PRAGMA user_version"), vec!["user_version"]);
    assert!(exec_columns("PRAGMA user_version=5").is_empty());
}

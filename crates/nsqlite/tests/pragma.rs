//! PRAGMA: the statement form, the pragmas the suite needs, and the
//! result-column naming they control.
//!
//! This file tests the grammar and the schema reader as pure functions.
//! `tests/pragma_engine.rs` is the other half: it drives the executor and
//! checks the result SHAPES, which are invisible in the values a pure function
//! returns.
//!
//! Every expected value here was produced by the real `sqlite3` 3.53.4 on a
//! throwaway database, and the statement that produced it is quoted above the
//! test. Three things are worth knowing when reading the expectations:
//!
//! * **An unknown pragma is a silent no-op.** `PRAGMA nosuchpragma` runs, is
//!   not an error, and returns *no result set at all* — not an empty result
//!   with no columns. `Outcome::Nothing` is how this engine says a statement
//!   produced no result set, and it is asserted in `tests/pragma_engine.rs`
//!   against the executor, because the difference is invisible in row output
//!   and is exactly what `execsql` sees.
//! * **An empty result still has its columns.** `PRAGMA table_info(nosuch)`
//!   returns zero rows and the six `table_info` columns. The column set is part
//!   of the pragma's answer, not a by-product of which rows matched.
//! * **The two column-naming settings do not combine as a three-way choice.**
//!   For a direct column reference the two are ORed, so `full` wins even while
//!   `short` is on; for a `*` expansion `short` wins and the expansion stays
//!   bare. Both halves come from `select.c` — `sqlite3GenerateColumnNames` and
//!   the `longNames` flag — and both were confirmed against the binary before
//!   being written down.

use nsqlite::pragma::{self, columns as pcols, ColumnNameFlags, JournalMode, PragmaBody};
use nsqlite::value::Value;

// --- helpers -------------------------------------------------------------

/// Parses a PRAGMA statement.
fn p(sql: &str) -> pragma::Pragma {
    pragma::parse_pragma(sql).unwrap_or_else(|e| panic!("{sql} should parse: {e}"))
}

/// The error a statement produces, or a panic if it parses.
fn err(sql: &str) -> String {
    pragma::parse_pragma(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} should not parse"))
        .message
}

/// The columns of a table, read back out of its CREATE TABLE text.
fn cols(sql: &str) -> Vec<pragma::ColumnInfo> {
    pragma::SchemaText::new(sql)
        .expect("schema text should tokenize")
        .columns()
}

/// The foreign keys of a table, read back out of its CREATE TABLE text.
fn fks(sql: &str) -> Vec<pragma::ForeignKeyInfo> {
    pragma::SchemaText::new(sql)
        .expect("schema text should tokenize")
        .foreign_keys()
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

/// The rows a row builder produced, rendered.
fn rendered(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.iter().map(|r| render(r)).collect()
}

// --- 1. the statement form ----------------------------------------------

// sqlite3: PRAGMA full_column_names=on;  -> accepted, no result set
// sqlite3: PRAGMA cache_size=-2000;      -> accepted, no result set
// sqlite3: PRAGMA table_info(t);         -> accepted, the six table_info columns
// sqlite3: PRAGMA index_list(t);         -> accepted, the five index_list columns
#[test]
fn the_four_pragma_shapes_parse() {
    let set = p("PRAGMA full_column_names=on");
    assert_eq!(set.name(), "full_column_names");
    assert_eq!(set.body, PragmaBody::Set("on".into()));
    assert!(set.is_set());

    let neg = p("PRAGMA cache_size=-2000");
    assert_eq!(neg.name(), "cache_size");
    assert_eq!(neg.body, PragmaBody::Set("-2000".into()));

    let paren = p("PRAGMA table_info(t)");
    assert_eq!(paren.name(), "table_info");
    assert_eq!(paren.body, PragmaBody::ReadArg("t".into()));
    assert!(!paren.is_set());

    let list = p("PRAGMA index_list(t)");
    assert_eq!(list.name(), "index_list");
    assert_eq!(list.body, PragmaBody::ReadArg("t".into()));
}

// The parenthesised form is an argument and the `=` form is an assignment.
// They are told apart, because only one of them changes anything.
#[test]
fn the_parenthesised_form_is_not_the_assignment_form() {
    let paren = p("PRAGMA cache_size(-2000)");
    assert!(!paren.is_set(), "(arg) is a read");
    assert_eq!(paren.argument(), Some("-2000"));

    let assign = p("PRAGMA cache_size=-2000");
    assert!(assign.is_set(), "=arg is an assignment");
    assert_eq!(assign.argument(), Some("-2000"));
}

// sqlite3: PRAGMA page_size;   -> a result whose column is called page_size
// sqlite3: PRAGMA main.page_size;   -> the same; the schema is not in the name
// sqlite3: PRAGMA temp.user_version; -> 0
#[test]
fn a_bare_read_and_a_qualified_read_are_both_reads() {
    let bare = p("PRAGMA page_size");
    assert_eq!(bare.name(), "page_size");
    assert_eq!(bare.body, PragmaBody::Read);
    assert_eq!(bare.argument(), None);
    assert_eq!(bare.schema, None);

    let main = p("PRAGMA main.page_size");
    assert_eq!(main.name(), "page_size");
    assert_eq!(main.schema.as_deref(), Some("main"));
    assert_eq!(main.body, PragmaBody::Read);

    let temp = p("PRAGMA temp.user_version");
    assert_eq!(temp.schema.as_deref(), Some("temp"));
}

// sqlite3: PRAGMA PAGE_SIZE;   -> page_size. The pragma table is sorted and
// binary-searched, so the name is matched exactly and folded first.
#[test]
fn pragma_names_are_case_insensitive() {
    assert_eq!(p("PRAGMA PAGE_SIZE").name(), "page_size");
    assert_eq!(p("PrAgMa TaBlE_InFo(t)").name(), "table_info");
    assert_eq!(
        p("PRAGMA main.FOREIGN_KEY_LIST(t)").name(),
        "foreign_key_list"
    );
}

// sqlite3: PRAGMA main.table_info(t);  -> the same rows as PRAGMA table_info(t)
#[test]
fn a_qualified_introspection_pragma_keeps_its_schema() {
    let q = p("PRAGMA main.table_info(t)");
    assert_eq!(q.schema.as_deref(), Some("main"));
    assert_eq!(q.name(), "table_info");
    assert_eq!(q.argument(), Some("t"));
}

// sqlite3: PRAGMA bogus.user_version;  -> Parse error: unknown database bogus
//
// The executor owns the check, because it owns the schema list. What the parser
// owes is that the qualifier survives verbatim, so the message can name it.
#[test]
fn an_unknown_database_is_carried_through_for_the_executor_to_report() {
    let q = p("PRAGMA bogus.user_version");
    assert_eq!(q.schema.as_deref(), Some("bogus"));
    assert_eq!(q.name(), "user_version");
}

// sqlite3: PRAGMA                     -> Parse error: incomplete input
// sqlite3: PRAGMA;                    -> Parse error: near ";": syntax error
// sqlite3: PRAGMA =on;                -> Parse error: near "=": syntax error
// sqlite3: PRAGMA full_column_names=  -> Parse error: incomplete input
// sqlite3: PRAGMA main.               -> Parse error: incomplete input
// sqlite3: PRAGMA main.;              -> Parse error: near ";": syntax error
// sqlite3: PRAGMA 5                   -> Parse error: near "5": syntax error
//
// The `PRAGMA` / `PRAGMA;` pair is the whole rule in miniature: a name absent
// because the STATEMENT ENDED is `incomplete input`, and a name absent because
// a token is sitting there is `near "x": syntax error`. A semicolon is a token,
// so `PRAGMA;` is the second and not the first. That was asserted the other way
// round here before, against an oracle comment rather than against the binary;
// both answers above were re-measured with
//   printf 'PRAGMA;' | sqlite3   -> Parse error near line 1: near ";": syntax error
//   printf 'PRAGMA'  | sqlite3   -> Parse error near line 1: incomplete input
#[test]
fn a_malformed_pragma_reports_sqlites_wording() {
    for (sql, want) in [
        ("PRAGMA", "incomplete input"),
        ("PRAGMA;", "near \";\": syntax error"),
        ("PRAGMA =on;", "near \"=\": syntax error"),
        ("PRAGMA full_column_names=", "incomplete input"),
        ("PRAGMA main.", "incomplete input"),
        ("PRAGMA main.;", "near \";\": syntax error"),
        ("PRAGMA 5", "near \"5\": syntax error"),
    ] {
        assert_eq!(err(sql), want, "in {sql}");
    }
}

// sqlite3: PRAGMA 'x'          -> rc=0, no output, no error
// sqlite3: PRAGMA 'page_size'  -> 4096
//
// A single quote names a pragma, exactly as `"` and `[]` do. This parser used
// to refuse it, which turned a statement the real engine runs into a syntax
// error -- and a syntax error in a suite's setup block abandons the block, so
// every table it was going to create stays missing.
#[test]
fn a_single_quoted_pragma_name_is_a_name() {
    for sql in [
        "PRAGMA 'x'",
        "PRAGMA ''",
        "PRAGMA 'page_size'",
        "PRAGMA 'TABLE_INFO'(t)",
    ] {
        assert!(
            pragma::parse_pragma(sql).is_ok(),
            "{sql} is accepted by sqlite3 and should parse"
        );
    }
    assert_eq!(p("PRAGMA 'page_size'").name(), "page_size");
    assert_eq!(p("PRAGMA 'TABLE_INFO'(t)").name(), "table_info");
}

// sqlite3: PRAGMA table_info((t))  -> Parse error: near "(": syntax error
// sqlite3: PRAGMA x(a(b)c)         -> Parse error: near "(": syntax error
// sqlite3: PRAGMA x(a b)           -> Parse error: near "b": syntax error
// sqlite3: PRAGMA x(a.b)           -> Parse error: near ".": syntax error
// sqlite3: PRAGMA x(a-b)           -> Parse error: near "-": syntax error
// sqlite3: PRAGMA x(*)             -> Parse error: near "*": syntax error
// sqlite3: PRAGMA x(a)             -> ok
// sqlite3: PRAGMA x(1)             -> ok
// sqlite3: PRAGMA x(-1)            -> ok
// sqlite3: PRAGMA x(- 1)           -> ok
//
// The argument is ONE token. Tracking a paren depth, which this parser used to
// do, accepts `PRAGMA table_info((t))` with an argument of `(t)` where sqlite3
// refuses it, and that is a parse the engine has no meaning for.
#[test]
fn an_argument_is_one_token_and_a_paren_never_starts_one() {
    for (sql, want) in [
        ("PRAGMA table_info((t))", "near \"(\": syntax error"),
        ("PRAGMA x(a(b)c)", "near \"(\": syntax error"),
        ("PRAGMA x(a b)", "near \"b\": syntax error"),
        ("PRAGMA x(a.b)", "near \".\": syntax error"),
        ("PRAGMA x(a-b)", "near \"-\": syntax error"),
        ("PRAGMA x(*)", "near \"*\": syntax error"),
        ("PRAGMA x(a[0])", "near \"[0]\": syntax error"),
        ("PRAGMA x(1-2)", "near \"-\": syntax error"),
        ("PRAGMA x(-)", "near \")\": syntax error"),
        ("PRAGMA x(-a)", "near \"a\": syntax error"),
        ("PRAGMA x(-1)", "x(-1)"),
        ("PRAGMA x(- 1)", "x(- 1)"),
        ("PRAGMA x(1.5)", "x(1.5)"),
        ("PRAGMA x(0x10)", "x(0x10)"),
        ("PRAGMA x('a b')", "x('a b')"),
        ("PRAGMA x(a)", "x(a)"),
    ] {
        let got = match pragma::parse_pragma(sql) {
            Ok(pr) => format!("{}({})", pr.name(), pr.argument().unwrap_or("")),
            Err(e) => e.message.clone(),
        };
        assert_eq!(got, want, "in {sql}");
    }
}

// sqlite3: PRAGMA table_info( t )  -> the table's rows; whitespace is not a token
// sqlite3: PRAGMA table_info(t ))  -> Parse error: near ")": syntax error
// sqlite3: PRAGMA x(t;              -> Parse error: near ";": syntax error
// sqlite3: PRAGMA x(                -> Parse error: incomplete input
#[test]
fn the_parenthesised_form_closes_on_the_next_token_and_nothing_else() {
    assert_eq!(p("PRAGMA table_info( t )").argument(), Some("t"));
    assert_eq!(p("PRAGMA table_info(t) ;").argument(), Some("t"));
    for (sql, want) in [
        ("PRAGMA table_info(t ))", "near \")\": syntax error"),
        ("PRAGMA x(t;", "near \";\": syntax error"),
        ("PRAGMA x(,)", "near \",\": syntax error"),
        ("PRAGMA x(", "incomplete input"),
        ("PRAGMA x(t", "incomplete input"),
    ] {
        assert_eq!(err(sql), want, "in {sql}");
    }
}

// sqlite3: PRAGMA table_info();       -> Parse error: near ")": syntax error
// sqlite3: PRAGMA table_info(t, x);   -> Parse error: near ",": syntax error
// sqlite3: PRAGMA full_column_names=on extra  -> Parse error: near "extra": syntax error
// sqlite3: PRAGMA main.t.user_version -> Parse error: near ".": syntax error
#[test]
fn a_pragma_with_the_wrong_tail_is_a_syntax_error() {
    for (sql, want) in [
        ("PRAGMA table_info()", "near \")\": syntax error"),
        ("PRAGMA table_info(t, x)", "near \",\": syntax error"),
        (
            "PRAGMA full_column_names=on extra",
            "near \"extra\": syntax error",
        ),
        ("PRAGMA main.t.user_version", "near \".\": syntax error"),
    ] {
        assert_eq!(err(sql), want, "in {sql}");
    }
}

// sqlite3: PRAGMA main; PRAGMA table_info(t);   -> both run
#[test]
fn a_trailing_semicolon_is_left_for_the_statement_loop() {
    for sql in [
        "PRAGMA page_size;",
        "PRAGMA table_info(t);",
        "PRAGMA full_column_names=off;",
    ] {
        assert!(pragma::parse_pragma(sql).is_ok(), "{sql} should parse");
    }
}

// The argument keeps its own spelling, because the executor is what
// interprets it: a quoted name arrives quoted.
#[test]
fn an_argument_keeps_the_spelling_it_was_written_with() {
    assert_eq!(p("PRAGMA table_info('t')").argument(), Some("'t'"));
    assert_eq!(p("PRAGMA table_info=t").argument(), Some("t"));
    assert_eq!(p("PRAGMA table_info(\"t\")").argument(), Some("\"t\""));
    assert_eq!(p("PRAGMA user_version=5").argument(), Some("5"));
    assert_eq!(p("PRAGMA user_version=-5").argument(), Some("-5"));
    assert_eq!(p("PRAGMA journal_mode=WAL").argument(), Some("WAL"));
}

// --- 2. the silent no-op -------------------------------------------------

// sqlite3: PRAGMA nosuchpragma;    -> no error, no rows, and NO RESULT SET
// sqlite3: PRAGMA locking_mode=EXCLUSIVE;  -> the same
//
// sqlite3's own rule, from pragma.c: "IMP: R-43042-22504 No error messages are
// generated if an unknown pragma is issued."
#[test]
fn an_unknown_pragma_is_a_silent_no_op_not_an_error() {
    // A PRAGMA statement is what the parser hands back, and the executor turns
    // a name outside the known set into `Outcome::Nothing`. The parse
    // succeeding is the half of this that lives in this module.
    for sql in [
        "PRAGMA nosuchpragma",
        "PRAGMA nosuchpragma=1",
        "PRAGMA locking_mode=EXCLUSIVE",
        "PRAGMA trusted_schema=ON",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA wal_autocheckpoint=1000",
    ] {
        let stmt = p(sql);
        assert!(
            !is_answered(&stmt.name),
            "{sql} is not one this engine answers for"
        );
    }
}

// The names this engine answers for. A name outside this set is the silent
// no-op, and being a whitelist is what keeps "not implemented" from drifting
// into "is an error".
#[test]
fn the_answered_set_is_the_list_the_module_implements() {
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
        assert!(is_answered(name), "{name} should be answered");
    }
    for name in [
        "locking_mode",
        "trusted_schema",
        "nosuchpragma",
        "temp_store",
        "integrity_check",
    ] {
        assert!(!is_answered(name), "{name} is not implemented yet");
    }
}

// sqlite3: PRAGMA table_info(nosuch);  -> no rows
// sqlite3: PRAGMA table_info(nosuch);  -> but six columns, named as usual
#[test]
fn an_introspection_pragma_on_a_missing_table_keeps_its_columns() {
    // The builder is handed an empty column list, and the column set comes
    // from the pragma rather than from the rows, which is the whole point.
    let rows = pragma::table_info_rows(&[]);
    assert!(rows.is_empty());
    assert_eq!(pcols::TABLE_INFO.len(), 6);
}

// sqlite3, through the C API (so the column set is visible even with no rows):
//   PRAGMA table_info(nosuch)       -> ['cid','name','type','notnull','dflt_value','pk']
//   PRAGMA index_list(nosuch)       -> ['seq','name','unique','origin','partial']
//   PRAGMA index_info(nosuch)       -> ['seqno','cid','name']
//   PRAGMA foreign_key_list(nosuch) -> ['id','seq','table','from','to',
//                                       'on_update','on_delete','match']
//   PRAGMA database_list            -> ['seq','name','file']
#[test]
fn the_introspection_column_sets_are_sqlites() {
    assert_eq!(
        pcols::TABLE_INFO,
        ["cid", "name", "type", "notnull", "dflt_value", "pk"]
    );
    assert_eq!(
        pcols::INDEX_LIST,
        ["seq", "name", "unique", "origin", "partial"]
    );
    assert_eq!(pcols::INDEX_INFO, ["seqno", "cid", "name"]);
    assert_eq!(
        pcols::FOREIGN_KEY_LIST,
        [
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
    assert_eq!(pcols::DATABASE_LIST, ["seq", "name", "file"]);
}

// --- 3. table_info -------------------------------------------------------

// sqlite3, on
//   CREATE TABLE t1(a INTEGER PRIMARY KEY, b TEXT DEFAULT 'x' NOT NULL, c,
//                   d VARCHAR(20) DEFAULT 5, e TEXT DEFAULT (1+2),
//                   f INTEGER REFERENCES t2(q));
// PRAGMA table_info(t1);   ->
//   cid|name|type|notnull|dflt_value|pk
//   0|a|INTEGER|0||1
//   1|b|TEXT|1|'x'|0
//   2|c||0||0
//   3|d|VARCHAR(20)|0|5|0
//   4|e|TEXT|0|1+2|0
//   5|f|INTEGER|0||0
#[test]
fn table_info_reports_every_column_the_way_sqlite_does() {
    let ci = cols(
        "CREATE TABLE t1(a INTEGER PRIMARY KEY, b TEXT DEFAULT 'x' NOT NULL, c,
                         d VARCHAR(20) DEFAULT 5, e TEXT DEFAULT (1+2),
                         f INTEGER REFERENCES t2(q))",
    );
    assert_eq!(ci.len(), 6);
    assert_eq!(
        rendered(pragma::table_info_rows(&ci)),
        vec![
            "0|a|INTEGER|0||1",
            "1|b|TEXT|1|'x'|0",
            "2|c||0||0",
            "3|d|VARCHAR(20)|0|5|0",
            "4|e|TEXT|0|1+2|0",
            "5|f|INTEGER|0||0",
        ]
    );
}

// sqlite3, on
//   CREATE TABLE a(x INTEGER PRIMARY KEY, y);
//   CREATE TABLE b(x, y, PRIMARY KEY(x, y));
// PRAGMA table_info(a);  -> 0|x|INTEGER|0||1   1|y||0||0
// PRAGMA table_info(b);  -> 0|x||0||1         1|y||0||2
//
// `pk` is the column's 1-based POSITION in the primary key, which is why a
// composite key answers 1 then 2 rather than 1 then 1. The engine's own parser
// discards the column list of a table-level PRIMARY KEY, so this is one of the
// answers that has to come from the schema text.
#[test]
fn the_pk_column_is_the_position_in_the_primary_key() {
    let a = cols("CREATE TABLE a(x INTEGER PRIMARY KEY, y)");
    assert_eq!(
        rendered(pragma::table_info_rows(&a)),
        vec!["0|x|INTEGER|0||1", "1|y||0||0"]
    );

    let b = cols("CREATE TABLE b(x, y, PRIMARY KEY(x, y))");
    assert_eq!(
        rendered(pragma::table_info_rows(&b)),
        vec!["0|x||0||1", "1|y||0||2"]
    );
}

// sqlite3, on CREATE TABLE f(x UNIQUE, y);
// PRAGMA table_info(f);  -> 0|x||0||0   1|y||0||0
#[test]
fn a_unique_column_is_not_a_primary_key() {
    let f = cols("CREATE TABLE f(x UNIQUE, y)");
    assert_eq!(
        rendered(pragma::table_info_rows(&f)),
        vec!["0|x||0||0", "1|y||0||0"]
    );
}

// sqlite3, on
//   CREATE TABLE t1(a INTEGER, B VARCHAR(20), c "Weird Type",
//                   d UNSIGNED BIG INT, e);
// PRAGMA table_info(t1);  ->
//   0|a|INTEGER|0||0
//   1|B|VARCHAR(20)|0||0
//   2|c|Weird Type|0||0
//   3|d|UNSIGNED BIG INT|0||0
//   4|e||0||0
//
// The declared type comes back as it was written, and a quoted one comes back
// without its quotes. This is why the reader is pointed at the schema text: the
// engine's parser folds every bare word to lower case, so `B` and
// `UNSIGNED BIG INT` would both be lost by the time the tree reached it.
#[test]
fn the_declared_type_is_reported_as_written() {
    let t = cols(
        "CREATE TABLE t1(a INTEGER, B VARCHAR(20), c \"Weird Type\",
                         d UNSIGNED BIG INT, e)",
    );
    assert_eq!(
        rendered(pragma::table_info_rows(&t)),
        vec![
            "0|a|INTEGER|0||0",
            "1|B|VARCHAR(20)|0||0",
            "2|c|Weird Type|0||0",
            "3|d|UNSIGNED BIG INT|0||0",
            "4|e||0||0",
        ]
    );
}

// sqlite3, on
//   CREATE TABLE t2(n NOT NULL DEFAULT CURRENT_TIMESTAMP, s DEFAULT 'it''s',
//                   r REAL DEFAULT 1.50, neg DEFAULT -3, p DEFAULT (1+2),
//                   nl DEFAULT NULL, dt DEFAULT CURRENT_DATE,
//                   bl DEFAULT x'0102', nn NUMERIC DEFAULT 007,
//                   q DEFAULT "quoted", e DEFAULT 1e3, negp DEFAULT (+5));
// PRAGMA table_info(t2);  -> dflt_value, in order:
//   CURRENT_TIMESTAMP / 'it''s' / 1.50 / -3 / 1+2 / NULL / CURRENT_DATE /
//   x'0102' / 007 / "quoted" / 1e3 / +5
//
// The default is reported AS WRITTEN, not as evaluated: `007` keeps its leading
// zero, `1e3` keeps its exponent, and a parenthesised default loses only the
// parentheses SQLite drops.
#[test]
fn a_default_is_reported_as_written_not_as_evaluated() {
    let t = cols(
        "CREATE TABLE t2(n NOT NULL DEFAULT CURRENT_TIMESTAMP,
                         s DEFAULT 'it''s',
                         r REAL DEFAULT 1.50,
                         neg DEFAULT -3,
                         p DEFAULT (1+2),
                         nl DEFAULT NULL,
                         dt DEFAULT CURRENT_DATE,
                         bl DEFAULT x'0102',
                         nn NUMERIC DEFAULT 007,
                         q DEFAULT \"quoted\",
                         e DEFAULT 1e3,
                         negp DEFAULT (+5))",
    );
    let defaults: Vec<String> = pragma::table_info_rows(&t)
        .iter()
        .map(|r| r[4].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        defaults,
        vec![
            "CURRENT_TIMESTAMP",
            "'it''s'",
            "1.50",
            "-3",
            "1+2",
            "NULL",
            "CURRENT_DATE",
            "x'0102'",
            "007",
            "\"quoted\"",
            "1e3",
            "+5",
        ]
    );
}

// sqlite3, on CREATE TABLE t(a NOT NULL, b);
// PRAGMA table_info(t);  -> 0|a||1||0   1|b||0||0
#[test]
fn not_null_is_reported_as_an_integer() {
    let t = cols("CREATE TABLE t(a NOT NULL, b)");
    assert_eq!(
        rendered(pragma::table_info_rows(&t)),
        vec!["0|a||1||0", "1|b||0||0"]
    );
}

// sqlite3, on CREATE TABLE t(a INTEGER PRIMARY KEY, b NOT NULL);
// PRAGMA table_info(t);  -> 0|a|INTEGER|0||1  1|b||1||0
#[test]
fn a_not_null_primary_key_still_reports_notnull_and_pk_separately() {
    let t = cols("CREATE TABLE t(a INTEGER PRIMARY KEY, b NOT NULL)");
    assert_eq!(
        rendered(pragma::table_info_rows(&t)),
        vec!["0|a|INTEGER|0||1", "1|b||1||0"]
    );
}

// sqlite3, on CREATE TABLE t(a, b);
// PRAGMA table_info(t);  -> 0|a||0||0   1|b||0||0
#[test]
fn a_column_with_nothing_declared_reports_empty_type_not_null_and_default() {
    let t = cols("CREATE TABLE t(a, b)");
    assert_eq!(
        rendered(pragma::table_info_rows(&t)),
        vec!["0|a||0||0", "1|b||0||0"]
    );
}

// sqlite3, on CREATE TABLE t(a, PRIMARY KEY(a));
// PRAGMA table_info(t);  -> 0|a||0||1
#[test]
fn a_single_column_table_primary_key_is_position_one() {
    let t = cols("CREATE TABLE t(a, PRIMARY KEY(a))");
    assert_eq!(rendered(pragma::table_info_rows(&t)), vec!["0|a||0||1"]);
}

// --- 4. index_list and index_info ---------------------------------------

// sqlite3, on
//   CREATE TABLE t1(a,b,c);
//   CREATE UNIQUE INDEX aa ON t1(a,b);
//   CREATE INDEX bb ON t1(c);
//   CREATE INDEX cc ON t1(a) WHERE a>0;
//   CREATE INDEX zz ON t1(b);
// PRAGMA index_list(t1);  ->
//   seq|name|unique|origin|partial
//   0|zz|0|c|0
//   1|cc|0|c|1
//   2|bb|0|c|0
//   3|aa|1|c|0
//
// The order is REVERSE CREATION order, newest first, and `origin` is `c` for an
// index a CREATE INDEX made (against `u` for one a UNIQUE constraint made, or
// `pk` for a WITHOUT ROWID table's primary key).
#[test]
fn index_list_reports_indexes_newest_first() {
    let ix = [
        pragma::IndexInfo {
            name: "aa".into(),
            origin: "c",
            unique: true,
            partial: false,
        },
        pragma::IndexInfo {
            name: "bb".into(),
            origin: "c",
            unique: false,
            partial: false,
        },
        pragma::IndexInfo {
            name: "cc".into(),
            origin: "c",
            unique: false,
            partial: true,
        },
        pragma::IndexInfo {
            name: "zz".into(),
            origin: "c",
            unique: false,
            partial: false,
        },
    ];
    // `aa` was created first, so it comes back last with the highest seq.
    assert_eq!(
        rendered(pragma::index_list_rows(&ix)),
        vec!["0|zz|0|c|0", "1|cc|0|c|1", "2|bb|0|c|0", "3|aa|1|c|0",]
    );
}

// sqlite3, on  CREATE TABLE t1(a,b,c); CREATE INDEX bb ON t1(c);
// PRAGMA index_list(t1);  -> 0|bb|0|c|0
#[test]
fn a_non_unique_non_partial_index_reports_zero_flags() {
    let ix = [pragma::IndexInfo {
        name: "bb".into(),
        origin: "c",
        unique: false,
        partial: false,
    }];
    assert_eq!(rendered(pragma::index_list_rows(&ix)), vec!["0|bb|0|c|0"]);
}

// sqlite3, on CREATE TABLE t2(x,y,z);  (no indexes)
// PRAGMA index_list(t2);  -> no rows, and still the five columns
#[test]
fn index_list_on_a_table_with_no_indexes_is_empty() {
    assert!(pragma::index_list_rows(&[]).is_empty());
    assert_eq!(pcols::INDEX_LIST.len(), 5);
}

// sqlite3, on
//   CREATE TABLE u(x UNIQUE);  PRAGMA index_list(u);  ->
//     0|sqlite_autoindex_u_1|1|u|0
//   CREATE TABLE w(a TEXT PRIMARY KEY, b) WITHOUT ROWID;
//   PRAGMA index_list(w);  ->  0|sqlite_autoindex_w_1|1|pk|0
#[test]
fn the_implicit_index_of_a_unique_column_and_of_a_without_rowid_key() {
    let u = pragma::SchemaText::new("CREATE TABLE u(x UNIQUE)")
        .unwrap()
        .implicit_index()
        .expect("a UNIQUE column makes an implicit index");
    assert_eq!(u.name, "sqlite_autoindex_u_1");
    assert_eq!(u.origin, "u");
    assert!(u.unique);
    assert_eq!(
        rendered(pragma::index_list_rows(&[u])),
        vec!["0|sqlite_autoindex_u_1|1|u|0"]
    );

    let w = pragma::SchemaText::new("CREATE TABLE w(a TEXT PRIMARY KEY, b) WITHOUT ROWID")
        .unwrap()
        .implicit_index()
        .expect("a WITHOUT ROWID primary key makes an implicit index");
    assert_eq!(w.name, "sqlite_autoindex_w_1");
    assert_eq!(w.origin, "pk");
    assert_eq!(
        rendered(pragma::index_list_rows(&[w])),
        vec!["0|sqlite_autoindex_w_1|1|pk|0"]
    );
}

// sqlite3, on CREATE TABLE t(a,b);  (nothing to index)
// PRAGMA index_list(t);  -> no rows
#[test]
fn a_table_with_no_unique_constraint_has_no_implicit_index() {
    assert!(pragma::SchemaText::new("CREATE TABLE t(a, b)")
        .unwrap()
        .implicit_index()
        .is_none());
}

// sqlite3, on  CREATE TABLE t1(a,b,c); CREATE UNIQUE INDEX aa ON t1(a,b);
// PRAGMA index_info(aa);  -> seqno|cid|name  /  0|0|a  /  1|1|b
#[test]
fn index_info_reports_the_indexed_columns_in_key_order() {
    assert_eq!(
        rendered(pragma::index_info_rows(&[(0, "a".into()), (1, "b".into())])),
        vec!["0|0|a", "1|1|b"]
    );
}

// sqlite3, on  CREATE TABLE t1(a,b,c); CREATE INDEX zz ON t1(b);
// PRAGMA index_info(zz);  -> 0|1|b
//
// `cid` is the column's index in the TABLE and `seqno` its position in the
// index, so an index over the table's columns in a different order answers with
// the two out of step.
#[test]
fn index_info_cid_is_the_column_index_in_the_table() {
    assert_eq!(
        rendered(pragma::index_info_rows(&[(1, "b".into())])),
        vec!["0|1|b"]
    );
}

// sqlite3: PRAGMA index_info(nosuch);  -> no rows, and still three columns
#[test]
fn index_info_on_a_missing_index_is_empty() {
    assert!(pragma::index_info_rows(&[]).is_empty());
    assert_eq!(pcols::INDEX_INFO.len(), 3);
}

// --- 5. foreign_key_list -------------------------------------------------

// sqlite3, on
//   CREATE TABLE p1(x, y, PRIMARY KEY(x));
//   CREATE TABLE c1(a INTEGER PRIMARY KEY, b REFERENCES p1(x), c,
//                   FOREIGN KEY(c) REFERENCES p1(y));
// PRAGMA foreign_key_list(c1);  ->
//   id|seq|table|from|to|on_update|on_delete|match
//   0|0|p1|c|y|NO ACTION|NO ACTION|NONE
//   1|0|p1|b|x|NO ACTION|NO ACTION|NONE
//
// The order is the thing that is easy to get wrong: SQLite walks the
// column-level references BACKWARDS, so the table-level FOREIGN KEY is
// reported before the column-level one that preceded it in the statement.
#[test]
fn foreign_key_list_reports_each_reference_with_its_own_id() {
    let fk = fks(
        "CREATE TABLE c1(a INTEGER PRIMARY KEY, b REFERENCES p1(x), c,
                         FOREIGN KEY(c) REFERENCES p1(y))",
    );
    assert_eq!(
        rendered(pragma::foreign_key_list_rows(&fk)),
        vec![
            "0|0|p1|c|y|NO ACTION|NO ACTION|NONE",
            "1|0|p1|b|x|NO ACTION|NO ACTION|NONE",
        ]
    );
}

// sqlite3, on CREATE TABLE c3(a REFERENCES nosuchtable(z) ON DELETE SET NULL);
// PRAGMA foreign_key_list(c3);  ->
//   0|0|nosuchtable|a|z|NO ACTION|SET NULL|NONE
//
// The referenced table does not have to exist: the pragma reads the schema
// text, not the target.
#[test]
fn foreign_key_list_does_not_require_the_target_table_to_exist() {
    let fk = fks("CREATE TABLE c3(a REFERENCES nosuchtable(z) ON DELETE SET NULL)");
    assert_eq!(
        rendered(pragma::foreign_key_list_rows(&fk)),
        vec!["0|0|nosuchtable|a|z|NO ACTION|SET NULL|NONE"]
    );
}

// sqlite3, on CREATE TABLE t(a);
// PRAGMA foreign_key_list(t);  -> no rows, and still the eight columns
#[test]
fn foreign_key_list_on_a_table_without_keys_is_empty() {
    assert!(pragma::foreign_key_list_rows(&[]).is_empty());
    assert_eq!(pcols::FOREIGN_KEY_LIST.len(), 8);
}

// sqlite3, on
//   CREATE TABLE c2(a, b REFERENCES p1,
//                   FOREIGN KEY (a,b) REFERENCES p1(x,y)
//                     ON DELETE CASCADE ON UPDATE SET NULL);
// PRAGMA foreign_key_list(c2);  ->
//   0|0|p1|a|x|SET NULL|CASCADE|NONE
//   0|1|p1|b|y|SET NULL|CASCADE|NONE
//   1|0|p1|b||NO ACTION|NO ACTION|NONE
//
// A composite key takes one row per column pair under ONE id, and a reference
// with no column list has an empty `to`.
#[test]
fn a_composite_foreign_key_shares_one_id_across_its_columns() {
    let fk = fks("CREATE TABLE c2(a, b REFERENCES p1,
                         FOREIGN KEY (a,b) REFERENCES p1(x,y)
                           ON DELETE CASCADE ON UPDATE SET NULL)");
    assert_eq!(
        rendered(pragma::foreign_key_list_rows(&fk)),
        vec![
            "0|0|p1|a|x|SET NULL|CASCADE|NONE",
            "0|1|p1|b|y|SET NULL|CASCADE|NONE",
            "1|0|p1|b||NO ACTION|NO ACTION|NONE",
        ]
    );
}

// sqlite3, on
//   CREATE TABLE c4(a REFERENCES p1 ON DELETE RESTRICT ON UPDATE CASCADE);
// PRAGMA foreign_key_list(c4);  ->
//   0|0|p1|a||CASCADE|RESTRICT|NONE
#[test]
fn the_actions_are_reported_in_sqlites_wording() {
    let fk = fks("CREATE TABLE c4(a REFERENCES p1 ON DELETE RESTRICT ON UPDATE CASCADE)");
    assert_eq!(
        rendered(pragma::foreign_key_list_rows(&fk)),
        vec!["0|0|p1|a||CASCADE|RESTRICT|NONE"]
    );
}

// sqlite3, on CREATE TABLE t(a REFERENCES p1(x));
// PRAGMA foreign_key_list(t);  -> 0|0|p1|a|x|NO ACTION|NO ACTION|NONE
#[test]
fn a_plain_column_level_reference_reports_its_own_column() {
    let fk = fks("CREATE TABLE t(a, b REFERENCES p1(x))");
    assert_eq!(
        rendered(pragma::foreign_key_list_rows(&fk)),
        vec!["0|0|p1|b|x|NO ACTION|NO ACTION|NONE"]
    );
}

// --- 6. database_list ----------------------------------------------------

// sqlite3: PRAGMA database_list;  ->  seq|name|file  /  0|main|<the file>
// and for an in-memory database the file column is the empty string.
#[test]
fn database_list_names_the_main_database_and_its_file() {
    assert_eq!(
        rendered(pragma::database_list_rows(&[(
            0,
            "main".into(),
            String::new()
        )])),
        vec!["0|main|"]
    );
    assert_eq!(
        rendered(pragma::database_list_rows(&[(
            0,
            "main".into(),
            "/tmp/pg4.db".into()
        )])),
        vec!["0|main|/tmp/pg4.db"]
    );
}

// sqlite3: PRAGMA database_list;  -> one row for main, and one for temp only
// when a temp b-tree exists.
#[test]
fn a_database_this_connection_has_not_opened_is_not_listed() {
    // SQLite skips a schema with no b-tree rather than answering it with an
    // empty file, so the caller decides which databases to pass.
    assert_eq!(pragma::database_list_rows(&[]).len(), 0);
    assert_eq!(
        pragma::database_list_rows(&[(0, "main".into(), String::new())]).len(),
        1
    );
}

// --- 7. the settings -----------------------------------------------------

// sqlite3, on a fresh file:
//   PRAGMA user_version;       -> user_version / 0
//   PRAGMA application_id;     -> application_id / 0
//   PRAGMA page_size;          -> page_size / 4096
//   PRAGMA encoding;           -> encoding / UTF-8
//   PRAGMA schema_version;     -> schema_version / 0
//   PRAGMA journal_mode;       -> journal_mode / delete
//   PRAGMA cache_size;         -> cache_size / -2000
#[test]
fn the_settings_state_defaults_to_sqlites() {
    let s = pragma::PragmaState::default();
    assert_eq!(s.user_version, 0);
    assert_eq!(s.application_id, 0);
    assert_eq!(s.journal_mode, JournalMode::Delete);
    assert!(!s.column_names.full);
    assert!(s.column_names.short);
}

// sqlite3: PRAGMA full_column_names=ON;   -> 1
// sqlite3: PRAGMA full_column_names=no;   -> 0
// sqlite3: PRAGMA full_column_names=TRUE; -> 1
// sqlite3: PRAGMA full_column_names=bogus; -> 0, and no error
#[test]
fn a_boolean_setting_reads_the_yes_and_no_spellings() {
    for (spelling, want) in [
        ("ON", true),
        ("on", true),
        ("yes", true),
        ("TRUE", true),
        ("1", true),
        ("2", true),
        ("0", false),
        ("off", false),
        ("no", false),
        ("false", false),
        ("bogus", false),
        ("", false),
    ] {
        assert_eq!(pragma::parse_bool(spelling), want, "for {spelling:?}");
    }
}

// sqlite3: PRAGMA user_version=abc;  -> accepted, and the setting is unchanged
#[test]
fn a_numeric_pragma_ignores_a_value_that_is_not_a_number() {
    // SQLite does not fail an assignment it cannot interpret; it ignores it.
    assert_eq!(pragma::parse_int("abc"), None);
    assert_eq!(pragma::parse_int(" 42 "), Some(42));
    assert_eq!(pragma::parse_int("-2000"), Some(-2000));
    assert_eq!(pragma::parse_int("5.5"), None);
}

// sqlite3: PRAGMA journal_mode=WAL;  -> journal_mode / wal
// sqlite3, an unrecognised mode: PRAGMA journal_mode=nosuchmode;
//          -> the mode in force, which is `delete` on a file
#[test]
fn journal_mode_names_are_the_ones_sqlite_prints() {
    assert_eq!(JournalMode::Delete.as_str(), "delete");
    assert_eq!(JournalMode::Memory.as_str(), "memory");
    assert_eq!(JournalMode::parse("DELETE"), Some(JournalMode::Delete));
    assert_eq!(JournalMode::parse("Memory"), Some(JournalMode::Memory));
    assert_eq!(JournalMode::parse("wal"), None, "no WAL in this engine");
    // An unrecognised mode leaves the mode alone rather than failing.
    assert_eq!(JournalMode::parse("nosuchmode"), None);
}

// sqlite3: PRAGMA encoding;  -> UTF-8
#[test]
fn the_encoding_name_is_the_one_text_encodings_prints() {
    use nsqlite::text::Encoding;
    assert_eq!(Encoding::Utf8.name(), "UTF-8");
    assert_eq!(Encoding::Utf16Le.name(), "UTF-16le");
    assert_eq!(Encoding::Utf16Be.name(), "UTF-16be");
}

// sqlite3, after PRAGMA user_version=5;      -> PRAGMA user_version;      -> 5
// sqlite3, after PRAGMA application_id=99;   -> PRAGMA application_id;   -> 99
#[test]
fn a_numeric_setting_round_trips_through_its_own_parser() {
    let mut s = pragma::PragmaState::default();
    if let Some(n) = pragma::parse_int("5") {
        s.user_version = n;
    }
    if let Some(n) = pragma::parse_int("99") {
        s.application_id = n;
    }
    assert_eq!(s.user_version, 5);
    assert_eq!(s.application_id, 99);
}

// The two flags round-trip through the same state the connection keeps, which
// is what lets a PRAGMA before a SELECT change what the SELECT is called.
#[test]
fn a_flag_setting_round_trips_through_its_own_parser() {
    let mut s = pragma::PragmaState::default();
    s.column_names.full = pragma::parse_bool("on");
    assert!(s.column_names.full);
    s.column_names.full = pragma::parse_bool("no");
    assert!(!s.column_names.full);
}

// --- 8. result-column naming --------------------------------------------

// The three spellings, from select1.test 6.9.3 and 6.9.4, confirmed against the
// binary on 3.53.4:
//
//   PRAGMA short_column_names=OFF; PRAGMA full_column_names=OFF;
//   SELECT test1 . f1, test1 . f2 FROM test1 LIMIT 1;   -> test1 . f1 | test1 . f2
//   PRAGMA short_column_names=OFF; PRAGMA full_column_names=ON;
//   SELECT test1 . f1, test1 . f2 FROM test1 LIMIT 1;   -> test1.f1 | test1.f2
//   and with neither set (short=ON, full=OFF):
//   SELECT test1 . f1 FROM test1 LIMIT 1;               -> f1
#[test]
fn a_join_column_has_three_names_and_the_settings_choose_between_them() {
    let bare = "test1 . f1";
    let dot = "test1.f1";
    let off = ColumnNameFlags {
        full: false,
        short: false,
    };
    let full = ColumnNameFlags {
        full: true,
        short: false,
    };
    let def = ColumnNameFlags::default();

    // Neither set: the short name, from the column itself.
    assert_eq!(column_name(def, bare, "test1", "f1"), "f1");
    assert_eq!(column_name(def, dot, "test1", "f1"), "f1");
    // short off, full off: the source text, spaces and all.
    assert_eq!(column_name(off, bare, "test1", "f1"), "test1 . f1");
    assert_eq!(column_name(off, dot, "test1", "f1"), "test1.f1");
    // short off, full on: the table-qualified name, normalised to one dot.
    assert_eq!(column_name(full, bare, "test1", "f1"), "test1.f1");
    assert_eq!(column_name(full, dot, "test1", "f1"), "test1.f1");
}

// The naming rule against SQLite's own default and against the one table this
/// engine has, taken from its schema rather than from the query's spelling.
#[test]
fn the_naming_rule_is_the_one_sqlite_writes_down() {
    let def = ColumnNameFlags::default();
    let full = ColumnNameFlags {
        full: true,
        short: false,
    };
    let both = ColumnNameFlags {
        full: true,
        short: true,
    };

    // full on: qualified, whatever the source said and whatever the alias was.
    assert_eq!(direct_name(full, "test1 . f1", "test1", "f1"), "test1.f1");
    assert_eq!(direct_name(full, "t.f1", "test1", "f1"), "test1.f1");
    // short off, full off: the source text.
    assert_eq!(
        direct_name(
            ColumnNameFlags {
                full: false,
                short: false
            },
            "test1 . f1",
            "test1",
            "f1"
        ),
        "test1 . f1"
    );
    // default: the bare column name.
    assert_eq!(direct_name(def, "t.f1", "test1", "f1"), "f1");
    // an expression is its source text in every mode.
    assert_eq!(source_name(both, "f1+1"), "f1+1");
    assert_eq!(source_name(def, "1 + 1"), "1 + 1");
}

// sqlite3, for every setting of the two flags:
//   SELECT f1 FROM test1          -> f1        (full off)
//                                -> test1.f1  (full on)
//   SELECT t.f1 FROM test1 t      -> t.f1  (short off, full off)
//                                -> f1    (short on,  full off)
//                                -> test1.f1 (short off, full on)
//                                -> test1.f1 (short on,  full on)
//
// The asymmetry is the whole point: a DIRECT reference ORs the two settings, so
// `full` wins while `short` is still on, whereas a star requires `short` to be
// off. That is `sqlite3GenerateColumnNames` versus the `longNames` flag in
// `select.c`, and the two disagree on purpose.
#[test]
fn a_direct_reference_ors_the_flags_while_a_star_requires_short_off() {
    // `bare` is `SELECT f1 FROM test1` and `aliased` is
    // `SELECT t.f1 FROM test1 t`, each confirmed on 3.53.4:
    //
    //   full short | bare      aliased    star (through alias t)
    //   0    0     | f1       t.f1       f1
    //   0    1     | f1       f1         f1
    //   1    0     | test1.f1 test1.f1   t.f1
    //   1    1     | test1.f1 test1.f1   f1
    for (full, short, bare, aliased, star) in [
        (false, false, "f1", "t.f1", "f1"),
        (false, true, "f1", "f1", "f1"),
        (true, false, "test1.f1", "test1.f1", "t.f1"),
        (true, true, "test1.f1", "test1.f1", "f1"),
    ] {
        let f = ColumnNameFlags { full, short };
        let tag = format!("full={full} short={short}");
        assert_eq!(
            direct_name(f, "f1", "test1", "f1"),
            bare,
            "bare column, {tag}"
        );
        assert_eq!(
            direct_name(f, "t.f1", "test1", "f1"),
            aliased,
            "qualified through an alias, {tag}"
        );
        // The star is the other OR: `longNames` is FullColNames && !ShortColNames.
        assert_eq!(f.qualified_star(), full && !short, "{tag}");
        assert_eq!(
            pragma::star_column_name(f, "t", "f1"),
            star,
            "star through an alias, {tag}"
        );
        assert_eq!(
            pragma::star_column_name(f, "test2", "f1"),
            if full && !short { "test2.f1" } else { "f1" },
            "star through a table, {tag}"
        );
    }
}

// sqlite3:
//   PRAGMA full_column_names=0; PRAGMA short_column_names=0;
//   SELECT * FROM test1 a, test2 LIMIT 1;  -> f1|f2|f1|f2
//   PRAGMA full_column_names=1; PRAGMA short_column_names=0;
//   SELECT * FROM test1 a, test2 LIMIT 1;  -> test1.f1|test1.f2|test2.f1|test2.f2
//   PRAGMA full_column_names=1; PRAGMA short_column_names=1;
//   SELECT * FROM test1 a, test2 LIMIT 1;  -> f1|f2|f1|f2
//
// The expansion qualifies with the SOURCE the column was reached through, which
// is the alias where there is one, so a star through `test1 a` answers `a.f1`
// and a star through the bare table answers `test1.f1`.
#[test]
fn a_star_expansion_qualifies_with_the_source_it_was_reached_through() {
    let f = ColumnNameFlags {
        full: true,
        short: false,
    };
    assert_eq!(pragma::star_column_name(f, "a", "f1"), "a.f1");
    assert_eq!(pragma::star_column_name(f, "test2", "f1"), "test2.f1");
    // With short on, the whole expansion is bare even though full is set.
    let g = ColumnNameFlags {
        full: true,
        short: true,
    };
    assert_eq!(pragma::star_column_name(g, "a", "f1"), "f1");
    assert_eq!(pragma::star_column_name(g, "test2", "f1"), "f1");
}

// sqlite3, on CREATE TABLE T1(MiXeD, B):
//   PRAGMA full_column_names=1; SELECT B FROM T1;         -> T1.B
//   PRAGMA full_column_names=1; SELECT * FROM T1;         -> T1.MiXeD|T1.B
//   PRAGMA full_column_names=1; SELECT t1."MiXeD" FROM T1;-> T1.MiXeD
//
// The qualified name uses the SCHEMA's spelling, not the query's: writing the
// name in lower case still comes back as declared.
#[test]
fn a_qualified_name_uses_the_schemas_case_not_the_querys() {
    let f = ColumnNameFlags {
        full: true,
        short: false,
    };
    // The table's columns are named as the schema wrote them, and the query
    // spelled it `b` / `t1."MiXeD"`.
    assert_eq!(direct_name(f, "b", "T1", "B"), "T1.B");
    assert_eq!(direct_name(f, "t1.\"MiXeD\"", "T1", "MiXeD"), "T1.MiXeD");
}

// sqlite3:
//   SELECT f1 AS x FROM test1          -> x
//   SELECT f1+F2 FROM test1            -> f1+F2
//   SELECT test1.f1+F2 FROM test1      -> test1.f1+F2
//   SELECT 1 + 1                      -> 1 + 1
//   SELECT count(*) FROM test1         -> count(*)
//
// An alias wins outright. Otherwise the name is the expression's source text,
// which is why `1 + 1` and `1+1` are different column names.
#[test]
fn an_alias_wins_and_an_expression_is_named_by_its_source_text() {
    let both = ColumnNameFlags {
        full: true,
        short: true,
    };
    assert_eq!(aliased_name(both, "f1", "x"), "x");
    assert_eq!(source_name(both, "f1+F2"), "f1+F2");
    assert_eq!(source_name(both, "test1.f1+F2"), "test1.f1+F2");
    assert_eq!(source_name(both, "1 + 1"), "1 + 1");
    assert_eq!(source_name(both, "count(*)"), "count(*)");
}

// sqlite3: SELECT f1 AS 'xyzzy ' FROM test1;  -> a column called "xyzzy ",
// with its trailing space.
#[test]
fn an_alias_is_used_verbatim_including_its_spaces() {
    let def = ColumnNameFlags::default();
    assert_eq!(aliased_name(def, "f1", "xyzzy "), "xyzzy ");
}

/// The naming half of [`pragma::column_name`] for a direct column reference,
/// with the table and column taken from the schema rather than written out at
/// each call site.
fn column_name(flags: ColumnNameFlags, source: &str, table: &str, column: &str) -> String {
    pragma::column_name(flags, None, Some(table), Some(column), source)
}

/// A direct column reference that the query reached through an alias, so the
/// source text says `t.f1` while the schema's table and column are what the
/// name is built from.
fn direct_name(flags: ColumnNameFlags, source: &str, table: &str, column: &str) -> String {
    column_name(flags, source, table, column)
}

/// A result column that is not a reference to a table column at all, so its
/// name is its own source text in every mode.
fn source_name(flags: ColumnNameFlags, source: &str) -> String {
    pragma::column_name(flags, None, None, None, source)
}

/// A result column with an explicit alias, which wins over both settings.
fn aliased_name(flags: ColumnNameFlags, source: &str, alias: &str) -> String {
    pragma::column_name(flags, Some(alias), Some("test1"), Some("f1"), source)
}

/// Whether a PRAGMA name is one this engine answers for. Kept next to the
/// tests that pin the list down, and mirrored by the connection's own match.
fn is_answered(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "full_column_names"
            | "short_column_names"
            | "table_info"
            | "index_list"
            | "index_info"
            | "foreign_key_list"
            | "database_list"
            | "user_version"
            | "application_id"
            | "page_count"
            | "page_size"
            | "encoding"
            | "schema_version"
            | "journal_mode"
            | "cache_size"
    )
}

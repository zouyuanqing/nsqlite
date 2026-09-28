//! Tests for the recursive-descent parser.
//!
//! The cases here are the ones the official suite exercises through
//! `select1.test` and friends: operator precedence, the predicate forms, the
//! places where SQLite's grammar departs from standard SQL, and the inputs that
//! must produce an error rather than a crash.

use super::*;

fn ok(sql: &str) -> Stmt {
    parse_one(sql).unwrap_or_else(|e| panic!("failed to parse {sql:?}: {e}"))
}

fn err(sql: &str) -> Error {
    parse_one(sql).unwrap_err()
}

fn select_body(sql: &str) -> SelectBody {
    let Stmt::Select(s) = ok(sql) else {
        panic!("expected a select from {sql}")
    };
    s.body
}

#[test]
fn selects_the_projection() {
    let Stmt::Select(s) = ok("SELECT 1, 2") else {
        panic!("expected a select")
    };
    let SelectBody::Simple { columns, .. } = s.body else {
        panic!("expected a simple body")
    };
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].expr, Expr::Literal(Literal::Integer(1)));
}

#[test]
fn a_star_expands_later() {
    let Stmt::Select(s) = ok("SELECT * FROM t") else {
        panic!()
    };
    let SelectBody::Simple { columns, from, .. } = s.body else {
        panic!()
    };
    assert!(
        columns[0].expr
            == Expr::Function {
                name: "*".into(),
                args: vec![],
                star: true,
                distinct: false
            }
    );
    assert_eq!(from.len(), 1);
    assert_eq!(
        from[0],
        FromItem::Table(TableRef {
            name: "t".into(),
            alias: None,
            join: None,
            on: None,
            using: vec![],
            natural: false,
            indexed_by: None,
        })
    );
}

#[test]
fn arithmetic_binds_multiplication_tighter_than_addition() {
    let SelectBody::Simple { columns, .. } = select_body("SELECT 1 + 2 * 3") else {
        panic!()
    };
    // 1 + (2 * 3), not (1 + 2) * 3.
    let Expr::Binary {
        op: BinOp::Add,
        right,
        ..
    } = &columns[0].expr
    else {
        panic!()
    };
    assert!(matches!(**right, Expr::Binary { op: BinOp::Mul, .. }));
}

#[test]
fn and_binds_tighter_than_or() {
    let SelectBody::Simple {
        where_: Some(w), ..
    } = select_body("SELECT 1 WHERE a = 1 OR b = 2 AND c = 3")
    else {
        panic!()
    };
    // a = 1 OR (b = 2 AND c = 3)
    let Expr::Binary {
        op: BinOp::Or,
        right,
        ..
    } = &w
    else {
        panic!()
    };
    assert!(matches!(**right, Expr::Binary { op: BinOp::And, .. }));
}

#[test]
fn comparison_binds_looser_than_concat() {
    let SelectBody::Simple { columns, .. } = select_body("SELECT 'a' || 'b' = 'ab'") else {
        panic!()
    };
    // ('a' || 'b') = 'ab'
    let Expr::Binary {
        op: BinOp::Eq,
        left,
        ..
    } = &columns[0].expr
    else {
        panic!()
    };
    assert!(matches!(
        **left,
        Expr::Binary {
            op: BinOp::Concat,
            ..
        }
    ));
}

#[test]
fn is_null_and_not_null_are_predicates_not_comparisons() {
    let SelectBody::Simple {
        where_: Some(w), ..
    } = select_body("SELECT 1 WHERE a ISNULL AND b NOTNULL")
    else {
        panic!()
    };
    let Expr::Binary {
        op: BinOp::And,
        left,
        right,
        ..
    } = &w
    else {
        panic!()
    };
    assert!(matches!(**left, Expr::IsNull { negated: false, .. }));
    assert!(matches!(**right, Expr::IsNull { negated: true, .. }));
}

#[test]
fn between_in_and_like_parse() {
    assert!(matches!(
        select_body("SELECT 1 WHERE a BETWEEN 1 AND 2"),
        SelectBody::Simple {
            where_: Some(Expr::Between { .. }),
            ..
        }
    ));
    assert!(matches!(
        select_body("SELECT 1 WHERE a IN (1, 2, 3)"),
        SelectBody::Simple { where_: Some(Expr::InList { list, .. }), .. } if list.len() == 3
    ));
    assert!(matches!(
        select_body("SELECT 1 WHERE a NOT IN (1)"),
        SelectBody::Simple {
            where_: Some(Expr::InList { negated: true, .. }),
            ..
        }
    ));
    assert!(matches!(
        select_body("SELECT 1 WHERE a LIKE 'x%' ESCAPE '\\'"),
        SelectBody::Simple {
            where_: Some(Expr::Like {
                escape: Some(_),
                ..
            }),
            ..
        }
    ));
}

#[test]
fn a_function_call_with_a_star_is_marked_as_such() {
    let SelectBody::Simple { columns, .. } = select_body("SELECT count(*) FROM t") else {
        panic!()
    };
    let Expr::Function { name, star, .. } = &columns[0].expr else {
        panic!()
    };
    assert_eq!(name, "count");
    assert!(star);
}

#[test]
fn case_has_both_forms() {
    // The searched form: CASE a WHEN ... compares a against each arm.
    assert!(matches!(
        select_body("SELECT CASE a WHEN 1 THEN 2 ELSE 3 END"),
        SelectBody::Simple { columns, .. }
            if matches!(&columns[0].expr, Expr::Case { operand: Some(_), otherwise: Some(_), .. })
    ));
    // The simple form: CASE WHEN ... evaluates each arm on its own.
    assert!(matches!(
        select_body("SELECT CASE WHEN a = 1 THEN 2 END"),
        SelectBody::Simple { columns, .. }
            if matches!(&columns[0].expr, Expr::Case { operand: None, otherwise: None, .. })
    ));
}

#[test]
fn exists_and_cast_parse() {
    assert!(matches!(
        select_body("SELECT 1 WHERE EXISTS (SELECT 1)"),
        SelectBody::Simple {
            where_: Some(Expr::Exists { .. }),
            ..
        }
    ));
    assert!(matches!(
        select_body("SELECT CAST(a AS INTEGER)"),
        SelectBody::Simple { columns, .. }
            if matches!(&columns[0].expr, Expr::Cast { ty, .. } if ty == "INTEGER")
    ));
}

#[test]
fn order_by_and_limit_parse() {
    let Stmt::Select(s) = ok("SELECT a FROM t ORDER BY a DESC, b LIMIT 10 OFFSET 5") else {
        panic!()
    };
    assert_eq!(s.order_by.len(), 2);
    assert!(!s.order_by[0].1, "DESC is the first key's direction");
    assert!(s.order_by[1].1, "the second key defaults to ascending");
    assert!(s.limit.is_some() && s.offset.is_some());
}

#[test]
fn limit_with_a_comma_means_offset_first() {
    let Stmt::Select(s) = ok("SELECT 1 LIMIT 5, 10") else {
        panic!()
    };
    // LIMIT a, b is LIMIT b OFFSET a.
    assert!(s.offset.is_some(), "the first number is the offset");
    assert!(s.limit.is_some());
}

#[test]
fn insert_forms_parse() {
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = ok("INSERT INTO t (a, b) VALUES (1, 'x')")
    else {
        panic!()
    };
    assert_eq!(table, "t");
    assert_eq!(columns, Some(vec!["a".into(), "b".into()]));
    // A statement that says nothing about a conflict is an ABORT, which is what
    // the reference does with one.
    assert_eq!(conflict, ConflictAction::Abort);
    let InsertSource::Values(rows) = source else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
}

#[test]
fn insert_from_a_select_parses() {
    let Stmt::Insert { source, .. } = ok("INSERT INTO t SELECT a FROM u") else {
        panic!()
    };
    assert!(matches!(source, InsertSource::Select(_)));
}

#[test]
fn update_and_delete_parse() {
    let Stmt::Update {
        table,
        sets,
        where_,
        conflict,
    } = ok("UPDATE t SET a = 1, b = b + 1 WHERE c = 2")
    else {
        panic!()
    };
    assert_eq!(table, "t");
    assert_eq!(sets.len(), 2);
    assert!(where_.is_some());
    assert_eq!(conflict, ConflictAction::Abort);

    let Stmt::Delete { table, where_ } = ok("DELETE FROM t WHERE a = 1") else {
        panic!()
    };
    assert_eq!(table, "t");
    assert!(where_.is_some());
}

#[test]
fn create_table_captures_columns_and_constraints() {
    let Stmt::CreateTable { name, columns, constraints, if_not_exists, .. } = ok(
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, score REAL DEFAULT 0)",
    ) else {
        panic!()
    };
    assert_eq!(name, "t");
    assert!(if_not_exists);
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, "integer");
    assert!(columns[0].constraints.contains(&Constraint::PrimaryKey {
        ascending: true,
        columns: Vec::new(),
        autoincrement: true
    }));
    assert!(columns[1].constraints.contains(&Constraint::NotNull));
    assert_eq!(columns[2].ty, "real");
    let _ = constraints;
}

#[test]
fn create_table_with_a_table_constraint_parses() {
    let Stmt::CreateTable {
        columns,
        constraints,
        ..
    } = ok("CREATE TABLE t (a, b, PRIMARY KEY (a, b))")
    else {
        panic!()
    };
    assert_eq!(columns.len(), 2, "the constraint is not a column");
    // A *table-level* key names the columns it covers, in the order it wrote
    // them. That is what makes `PRIMARY KEY(a,b)` a uniqueness constraint over
    // the pair -- measured: the duplicate reports `UNIQUE constraint failed:
    // t.a, t.b` -- where a column-level primary key names none, because the
    // column carrying the constraint is the key.
    assert!(constraints.contains(&Constraint::PrimaryKey {
        ascending: true,
        columns: vec!["a".into(), "b".into()],
        autoincrement: false
    }));
}

#[test]
fn without_rowid_and_strict_are_recognised() {
    let Stmt::CreateTable {
        without_rowid,
        strict,
        ..
    } = ok("CREATE TABLE t (a TEXT PRIMARY KEY) WITHOUT ROWID, STRICT")
    else {
        panic!()
    };
    assert!(without_rowid);
    assert!(strict);
}

#[test]
fn compound_selects_parse() {
    for (sql, want) in [
        ("SELECT 1 UNION SELECT 2", CompoundOp::Union),
        ("SELECT 1 UNION ALL SELECT 2", CompoundOp::UnionAll),
        ("SELECT 1 INTERSECT SELECT 2", CompoundOp::Intersect),
        ("SELECT 1 EXCEPT SELECT 2", CompoundOp::Except),
    ] {
        let Stmt::Select(s) = ok(sql) else { panic!() };
        assert!(
            matches!(&s.body, SelectBody::Compound { op, .. } if *op == want),
            "wrong operator for {sql}"
        );
    }
}

#[test]
fn a_cte_parses() {
    let Stmt::Select(s) = ok("WITH t AS (SELECT 1 AS x) SELECT x FROM t") else {
        panic!()
    };
    assert_eq!(s.with.len(), 1);
    assert_eq!(s.with[0].name, "t");
}

#[test]
fn a_transaction_statement_parses() {
    assert!(matches!(ok("BEGIN"), Stmt::Begin));
    assert!(matches!(ok("COMMIT"), Stmt::Commit));
    assert!(matches!(ok("ROLLBACK"), Stmt::Rollback));
    assert!(matches!(ok("BEGIN TRANSACTION"), Stmt::Begin));
}

#[test]
fn keywords_are_usable_as_identifiers() {
    // A table and columns whose names collide with keywords, which SQLite
    // permits and a strict parser would reject.
    let Stmt::CreateTable { name, columns, .. } =
        ok("CREATE TABLE t (key TEXT, values INTEGER, \"order\" TEXT)")
    else {
        panic!()
    };
    assert_eq!(name, "t");
    assert_eq!(columns[0].name, "key");
    assert_eq!(columns[1].name, "values");
    assert_eq!(columns[2].name, "order");
}

#[test]
fn the_quoting_forms_produce_a_name() {
    for sql in [
        "CREATE TABLE t (a TEXT)",
        "CREATE TABLE t ([a] TEXT)",
        "CREATE TABLE t (`a` TEXT)",
        "CREATE TABLE t (\"a\" TEXT)",
    ] {
        let Stmt::CreateTable { columns, .. } = ok(sql) else {
            panic!("{sql}")
        };
        assert_eq!(columns[0].name, "a", "for {sql}");
    }
}

#[test]
fn a_syntax_error_names_the_offending_token() {
    let e = err("SELECT * FROM");
    assert!(e.message.contains("syntax error"), "got: {}", e.message);
    let e = err("SELECT 1 +");
    assert!(e.message.contains("syntax error"), "got: {}", e.message);
}

#[test]
fn a_deeply_nested_expression_is_refused_rather_than_overflowing() {
    // SQLite's own limit. A recursive-descent parser has to honour it or it
    // exhausts the stack instead of returning an error.
    let depth = MAX_EXPR_DEPTH + 10;
    let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
    let e = parse_one(&sql).unwrap_err();
    assert!(
        e.message.contains("too large") || e.message.contains("too deep"),
        "got: {}",
        e.message
    );
}

#[test]
fn a_moderately_nested_expression_still_parses() {
    let depth = 100;
    let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
    parse_one(&sql).expect("100 levels must be well within the limit");
}

#[test]
fn a_script_holds_several_statements() {
    let stmts = parse_script("CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT * FROM t;")
        .expect("a script must parse");
    assert_eq!(stmts.len(), 3);
}

#[test]
fn the_stress_inputs_never_panic() {
    // The parser has to return an error rather than crash on anything.
    let inputs = [
        "",
        " ",
        ";",
        ";;;",
        "SELECT",
        "SELECT *",
        "SELECT * FROM",
        "SELECT * FROM t WHERE",
        "SELECT (((((",
        "SELECT 'unterminated",
        "SELECT x'",
        "SELECT 1 UNION",
        "INSERT INTO",
        "CREATE TABLE",
        "CREATE TABLE t (",
        "UPDATE",
        "DELETE",
        "DROP",
        "SELECT 1 + + +",
        "SELECT (((((((((((((((((((1))))))))))))))))))))",
        "\u{0}\u{1}\u{2}",
    ];
    for sql in inputs {
        let _ = parse_script(sql);
    }
    // A nesting far past the limit, which must be refused rather than crash.
    let deep = format!("SELECT {}1", "(".repeat(5000));
    let _ = parse_script(&deep);
}

// --- FROM clause joins -------------------------------------------------
//
// Every expectation below was taken from the sqlite3 in this repository's
// toolchain (3.53.4) rather than from recollection: the shapes and the error
// wording both come from asking it.

/// The FROM items of a SELECT, as compact text, so a test can assert the
/// operator and constraint each table carries.
fn from_items(sql: &str) -> Vec<String> {
    match select_body(sql) {
        SelectBody::Simple { from, .. } => from
            .iter()
            .map(|f| match f {
                FromItem::Table(t) => format!(
                    "{}{} join={:?} on={} using={:?}",
                    t.name,
                    t.alias
                        .as_ref()
                        .map(|a| format!(" AS {a}"))
                        .unwrap_or_default(),
                    t.join,
                    if t.on.is_some() { "y" } else { "n" },
                    t.using
                ),
                FromItem::Subquery { alias, .. } => format!("subquery alias={alias:?}"),
            })
            .collect(),
        other => panic!("expected a simple body, got {other:?}"),
    }
}

/// The FROM items of a SELECT, as the table references they are, so a test can
/// assert on a field the text form does not carry.
fn from_tables(sql: &str) -> Vec<TableRef> {
    match select_body(sql) {
        SelectBody::Simple { from, .. } => from
            .iter()
            .map(|f| match f {
                FromItem::Table(t) => t.clone(),
                FromItem::Subquery { .. } => panic!("{sql} has a subquery in FROM"),
            })
            .collect(),
        other => panic!("expected a simple body, got {other:?}"),
    }
}

#[test]
fn a_comma_from_carries_no_join_operator() {
    // sqlite3 parses `a, b` as two items with no operator on either, because a
    // comma is not a join type; whether the pair is a cross product or an inner
    // join depends on whether a constraint follows.
    assert_eq!(
        from_items("SELECT * FROM a, b"),
        vec!["a join=None on=n using=[]", "b join=None on=n using=[]"]
    );
    assert_eq!(from_items("SELECT * FROM a, b, c").len(), 3);
}

#[test]
fn a_bare_join_is_an_inner_join() {
    assert_eq!(
        from_items("SELECT * FROM a JOIN b ON a.x = b.x"),
        vec![
            "a join=None on=n using=[]",
            "b join=Some(Inner) on=y using=[]"
        ]
    );
    // A JOIN with no constraint is a cross join in effect, but the operator is
    // still recorded as INNER because that is what was written.
    assert_eq!(
        from_items("SELECT * FROM a JOIN b"),
        vec![
            "a join=None on=n using=[]",
            "b join=Some(Inner) on=n using=[]"
        ]
    );
}

#[test]
fn the_join_type_is_recorded_as_written() {
    for (sql, kind) in [
        ("SELECT * FROM a LEFT JOIN b ON 1", "Some(Left)"),
        ("SELECT * FROM a LEFT OUTER JOIN b ON 1", "Some(Left)"),
        ("SELECT * FROM a RIGHT JOIN b ON 1", "Some(Right)"),
        ("SELECT * FROM a FULL OUTER JOIN b ON 1", "Some(Full)"),
        ("SELECT * FROM a CROSS JOIN b", "Some(Cross)"),
        ("SELECT * FROM a INNER JOIN b ON 1", "Some(Inner)"),
    ] {
        let items = from_items(sql);
        assert!(
            items[1].contains(&format!("join={kind}")),
            "{sql}: got {items:?}"
        );
    }
}

#[test]
fn a_constraint_may_follow_a_comma() {
    // sqlite3 accepts `a, b ON ...`; the ON attaches to `b` and makes the pair an
    // inner join. The operator stays absent, and the executor decides from the
    // constraint.
    assert_eq!(
        from_items("SELECT * FROM a, b ON a.x = b.x"),
        vec!["a join=None on=n using=[]", "b join=None on=y using=[]"]
    );
}

#[test]
fn a_using_clause_becomes_a_column_list_not_an_on_expression() {
    assert_eq!(
        from_items("SELECT * FROM a JOIN b USING (x)"),
        vec![
            "a join=None on=n using=[]",
            "b join=Some(Inner) on=n using=[\"x\"]"
        ]
    );
    assert_eq!(
        from_items("SELECT * FROM a JOIN b USING (x, y)")[1],
        "b join=Some(Inner) on=n using=[\"x\", \"y\"]"
    );
}

#[test]
fn a_join_chain_records_each_operator() {
    // Each table after the first carries the operator that attached it, so the
    // executor can fold the list in from the left.
    assert_eq!(
        from_items("SELECT * FROM a JOIN b ON a.x=b.x JOIN c ON b.x=c.x"),
        vec![
            "a join=None on=n using=[]",
            "b join=Some(Inner) on=y using=[]",
            "c join=Some(Inner) on=y using=[]"
        ]
    );
}

#[test]
fn a_table_alias_is_recorded_with_or_without_as() {
    assert_eq!(
        from_items("SELECT * FROM a AS p")[0],
        "a AS p join=None on=n using=[]"
    );
    assert_eq!(
        from_items("SELECT * FROM a p")[0],
        "a AS p join=None on=n using=[]"
    );
    assert_eq!(
        from_items("SELECT * FROM a p JOIN b q ON p.x = q.y"),
        vec![
            "a AS p join=None on=n using=[]",
            "b AS q join=Some(Inner) on=y using=[]"
        ]
    );
}

#[test]
fn an_unknown_join_type_says_so() {
    // The wording is sqlite3's, which names the two words it rejected.
    for (sql, msg) in [
        (
            "SELECT * FROM a INNER OUTER JOIN b ON 1",
            "unknown join type: INNER OUTER",
        ),
        (
            "SELECT * FROM a CROSS OUTER JOIN b",
            "unknown join type: CROSS OUTER",
        ),
        ("SELECT * FROM a OUTER JOIN b", "unknown join type: OUTER"),
    ] {
        assert_eq!(err(sql).message, msg, "for {sql}");
    }
}

#[test]
fn a_natural_join_is_parsed_in_every_position() {
    // NATURAL is a modifier in front of any join type, and the executor derives
    // its column list from the catalog, so all the parser records is the flag
    // and the operator. `SELECT * FROM a NATURAL LEFT JOIN b` used to be a
    // syntax error at the NATURAL keyword while the bare form parsed, which is
    // the inconsistency this pins shut.
    for (sql, kind) in [
        ("SELECT * FROM a NATURAL JOIN b", "Some(Inner)"),
        ("SELECT * FROM a NATURAL LEFT JOIN b", "Some(Left)"),
        ("SELECT * FROM a NATURAL LEFT OUTER JOIN b", "Some(Left)"),
        ("SELECT * FROM a NATURAL RIGHT JOIN b", "Some(Right)"),
        ("SELECT * FROM a NATURAL FULL OUTER JOIN b", "Some(Full)"),
        ("SELECT * FROM a NATURAL INNER JOIN b", "Some(Inner)"),
        ("SELECT * FROM a NATURAL CROSS JOIN b", "Some(Cross)"),
        // The type may also come first: SQLite reads both orders as the same
        // operator.
        ("SELECT * FROM a LEFT NATURAL JOIN b", "Some(Left)"),
    ] {
        let tref = &from_tables(sql)[1];
        assert!(tref.natural, "{sql} should be NATURAL");
        assert_eq!(format!("{:?}", tref.join), kind, "for {sql}");
    }
}

#[test]
fn a_natural_join_may_not_carry_a_constraint() {
    // SQLite names the combination rather than accepting it and ignoring one of
    // the two, and the wording is the suite's to match.
    for sql in [
        "SELECT * FROM a NATURAL JOIN b ON a.x = b.x",
        "SELECT * FROM a NATURAL JOIN b USING(x)",
        "SELECT * FROM a NATURAL LEFT JOIN b ON a.x = b.x",
        "SELECT * FROM a NATURAL LEFT JOIN b USING(x)",
    ] {
        assert_eq!(
            err(sql).message,
            "a NATURAL join may not have an ON or USING clause",
            "for {sql}"
        );
    }
}

#[test]
fn a_keyword_that_is_only_a_join_word_is_still_a_table_name() {
    // `left` on its own is not a join, so it is read as the table it names. This
    // is why the operator is only consumed once a JOIN is confirmed.
    assert_eq!(
        from_items("SELECT * FROM left")[0],
        "left join=None on=n using=[]"
    );
    assert_eq!(
        from_items("SELECT * FROM a AS outer")[0],
        "a AS outer join=None on=n using=[]"
    );
}

// --- ALTER TABLE ... ADD COLUMN --------------------------------------------

#[test]
fn alter_table_add_column_captures_the_column_and_its_own_text() {
    // The spliced text is the *source* slice, not a reconstruction from the
    // parsed column, so what the statement wrote is what lands in
    // sqlite_schema. Measured against sqlite3 3.53.4: every one of these is
    // the exact text it stores.
    for (sql, name, ty, column_sql) in [
        (
            "ALTER TABLE t ADD COLUMN c",
            "c",
            "",
            "c",
        ),
        // The COLUMN word is optional, and is not part of the text either way.
        (
            "ALTER TABLE t ADD c",
            "c",
            "",
            "c",
        ),
        (
            "ALTER TABLE t ADD COLUMN c TEXT",
            "c",
            // The *parsed* type is folded, as every declared type in this
            // engine is; the spliced text keeps the statement's spelling,
            // which is the one that has to survive.
            "text",
            "c TEXT",
        ),
        (
            "ALTER TABLE t ADD COLUMN c NOT NULL DEFAULT 3",
            "c",
            "",
            "c NOT NULL DEFAULT 3",
        ),
        // A default the evaluator could not re-render still has to survive.
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT (1+2)",
            "c",
            "",
            "c DEFAULT (1+2)",
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT x'00FF'",
            "c",
            "",
            "c DEFAULT x'00FF'",
        ),
        // A quoted name keeps its quoting, which is what reusing the table
        // grammar's `column_def` buys.
        (
            "ALTER TABLE t ADD COLUMN \"c d\"",
            "c d",
            "",
            "\"c d\"",
        ),
    ] {
        let Stmt::AlterTableAddColumn {
            name: table,
            default_name_reference,
            column,
            column_sql: got,
        } = ok(sql)
        else {
            panic!("expected an ADD COLUMN from {sql}")
        };
        assert_eq!(table, "t", "for {sql:?}");
        // None of these carries a `DEFAULT (name)`, so the pending refusal
        // must not have been left set on the statement for a later one in the
        // same script to pick up.
        assert_eq!(default_name_reference, None, "for {sql:?}");
        assert_eq!(column.name, name, "for {sql:?}");
        assert_eq!(column.ty, ty, "for {sql:?}");
        assert_eq!(got, column_sql, "for {sql:?}");
    }
}

#[test]
fn alter_table_add_column_reads_a_qualified_table_name() {
    let Stmt::AlterTableAddColumn { name, .. } = ok("ALTER TABLE main.t ADD COLUMN c") else {
        panic!()
    };
    assert_eq!(name, "main.t");
}

#[test]
fn rename_and_drop_column_are_unsupported_rather_than_syntax_errors() {
    // Both are ALTER clauses this engine has not written. Reporting them as a
    // syntax error about `ALTER` was the specific lie this replaced: the
    // statement is well-formed and the *feature* is missing, which is what
    // `Stmt::Unsupported` is for.
    for (sql, what) in [
        ("ALTER TABLE t RENAME TO z", "rename"),
        ("ALTER TABLE t DROP COLUMN b", "drop"),
    ] {
        let Stmt::Unsupported(w) = ok(sql) else {
            panic!("expected Unsupported from {sql}")
        };
        assert_eq!(w, what, "for {sql:?}");
    }
}

#[test]
fn a_malformed_alter_is_a_syntax_error_naming_its_token() {
    // The other side of the same line: these are not statements at all, and
    // saying "not supported yet" about them would be as wrong as the syntax
    // error was about RENAME. Measured on 3.53.4.
    for (sql, token) in [
        ("ALTER TABLE t;", ";"),
        ("ALTER TABLE t XYZZY;", "XYZZY"),
        ("ALTER TABLE t DEFAULT 7;", "DEFAULT"),
        ("ALTER TABLE t ADD COLUMN c DEFAULT 1+2;", "+"),
    ] {
        let e = err(sql);
        assert_eq!(
            e.message,
            format!("near \"{token}\": syntax error"),
            "for {sql:?}"
        );
    }
}

#[test]
fn a_parenthesised_default_is_kept_as_the_expression_it_is() {
    // The paren used to discard the expression and store a NULL, which lost
    // two things at once: the value, and the difference between a default that
    // is a string literal and one that is a reference to a column. Both are
    // visible in the column the parser now hands on.
    // `text` is what lands in sqlite_schema and `expr` is what the column
    // was read as. The two differ for `(1)`, which keeps its parentheses
    // in the text and loses them in the expression, and that difference is
    // the point: neither of them is a NULL any more.
    for (sql, text, want_expr) in [
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT (1+2)",
            "(1+2)",
            Expr::Binary {
                op: BinOp::Add,
                left: Box::new(Expr::Literal(Literal::Integer(1))),
                right: Box::new(Expr::Literal(Literal::Integer(2))),
            },
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT ((1))",
            "((1))",
            Expr::Literal(Literal::Integer(1)),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT (1)",
            "(1)",
            Expr::Literal(Literal::Integer(1)),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT (NULL)",
            "(NULL)",
            Expr::Literal(Literal::Null),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT (x'00FF')",
            "(x'00FF')",
            Expr::Literal(Literal::Blob(vec![0x00, 0xFF])),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT 7",
            "7",
            Expr::Literal(Literal::Integer(7)),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT -5",
            "-5",
            Expr::Literal(Literal::Integer(-5)),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT 'txt'",
            "'txt'",
            Expr::Literal(Literal::Text("txt".to_string())),
        ),
        (
            "ALTER TABLE t ADD COLUMN c DEFAULT 1 COLLATE nocase",
            "1 COLLATE nocase",
            // A COLLATE is the last thing a default may carry, and it
            // wraps the default rather than replacing it.
            Expr::Collate {
                expr: Box::new(Expr::Literal(Literal::Integer(1))),
                collation: "nocase".to_string(),
            },
        ),
    ] {
        let want = text;
        let Stmt::AlterTableAddColumn {
            column: ColumnDef { constraints, .. },
            column_sql,
            ..
        } = ok(sql)
        else {
            panic!("expected an ADD COLUMN from {sql}")
        };
        // The spliced text is the statement's own, sliced rather than
        // rebuilt, so it is checked first and on its own terms: it is the
        // whole of what lands in sqlite_schema, parentheses included.
        assert_eq!(column_sql, format!("c DEFAULT {want}"), "for {sql:?}");
        // And separately, the shape the column was actually read as. Before
        // this, every parenthesised default became a bare NULL whatever it
        // said, so a `(1+2)` and a `(NULL)` were the same column.
        assert_eq!(constraints.len(), 1, "for {sql:?}");
        // The variant is a struct, not a tuple: it also carries whether the
        // default was written parenthesised, which is load-bearing rather than
        // incidental. `is_builtin_constant_default` needs it, because SQLite's
        // grammar reads an unparenthesised `a` as a string literal and a
        // parenthesised `(a)` as a reference, and `ALTER TABLE ... ADD COLUMN`
        // accepts one and refuses the other. Destructuring only `expr` here
        // would have left that flag unchecked by this test.
        let Constraint::Default { expr: got, parenthesized } = &constraints[0] else {
            panic!("expected a DEFAULT from {sql}")
        };
        assert_eq!(got, &want_expr, "for {sql:?}");
        assert_eq!(
            *parenthesized,
            text.contains('('),
            "for {sql:?}: the parenthesised flag must match what was written"
        );
    }
}

#[test]
fn a_parenthesised_name_default_is_recorded_and_the_bare_one_is_not() {
    // SQLite's grammar reads an unparenthesised `a` as a string literal and
    // a parenthesised `(a)` as a reference, so the same word is a constant in
    // one and is not in the other. The two spellings produce the same
    // expression tree and differ only in the `parenthesized` flag, so this
    // test reads that flag rather than the refusal.
    //
    // The refusal itself is not raised here, and that is measured: on the
    // CREATE path it is (sqlite3 answers `CREATE TABLE t(a,b DEFAULT (a))`
    // with `default value of column [b] is not constant`, and a parse error
    // cannot be overridden), while on the ALTER path the table is resolved
    // first and `ALTER TABLE nosuch ADD COLUMN c DEFAULT (a)` is `no such
    // table: nosuch`. See `Parser::default_name_reference` for why one flag
    // serves both.
    for sql in [
        "ALTER TABLE t ADD COLUMN c DEFAULT (a)",
        "ALTER TABLE t ADD COLUMN c DEFAULT (a COLLATE nocase)",
        "ALTER TABLE t ADD COLUMN c DEFAULT (\"a\")",
        "ALTER TABLE t ADD COLUMN c DEFAULT (+a)",
    ] {
        let Stmt::AlterTableAddColumn {
            default_name_reference,
            column,
            ..
        } = ok(sql)
        else {
            panic!("expected an ADD COLUMN from {sql}")
        };
        assert_eq!(
            default_name_reference.as_deref(),
            Some("c"),
            "for {sql:?}"
        );
        assert!(
            matches!(
                column.constraints[0],
                Constraint::Default {
                    parenthesized: true,
                    ..
                }
            ),
            "for {sql:?}"
        );
    }

    // The bare spelling is the same word and is not a reference, which is
    // the whole of the distinction.
    for sql in [
        "ALTER TABLE t ADD COLUMN c DEFAULT a",
        "ALTER TABLE t ADD COLUMN c DEFAULT a COLLATE nocase",
    ] {
        let Stmt::AlterTableAddColumn {
            default_name_reference,
            ..
        } = ok(sql)
        else {
            panic!("expected an ADD COLUMN from {sql}")
        };
        assert_eq!(
            default_name_reference, None,
            "for {sql:?}"
        );
    }
}

#[test]
fn a_create_table_parenthesised_name_default_is_a_parse_error() {
    // The other half of the ordering, and the one that *is* raised by the
    // parser: on the CREATE path there is no table to resolve, so the
    // grammar is the whole answer. Measured on 3.53.4, and the message names
    // the column rather than the name written as the default.
    for (sql, column) in [
        ("CREATE TABLE t(a,b DEFAULT (a))", "b"),
        ("CREATE TABLE t(a,b DEFAULT (+a))", "b"),
        ("CREATE TABLE t(a,b DEFAULT (a COLLATE nocase))", "b"),
        ("CREATE TABLE t(a,b DEFAULT (nosuchcol))", "b"),
    ] {
        let e = err(sql);
        assert_eq!(
            e.message,
            format!("default value of column [{column}] is not constant"),
            "for {sql:?}"
        );
    }

    // And the bare spelling is accepted, which is the distinction the
    // parentheses make.
    let _ = ok("CREATE TABLE t(a,b DEFAULT a)");
}

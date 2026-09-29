//! `GLOB` and `REGEXP`.
//!
//! Both were **process crashes** before this. The parser builds
//! `Expr::Binary { op: BinOp::Glob, .. }` and `Expr::Binary { op:
//! BinOp::Regexp, .. }`, while `eval`'s binary arm answered
//! `unreachable!("the pattern operators are handled by their own forms")` — an
//! assumption that is true of `LIKE`, which really is a form of its own, and
//! false of these two. So ordinary SQL killed the engine:
//!
//! ```text
//! $ printf "SELECT 'abc' GLOB 'a*';\n" | nsqlited --testsuite :memory:
//! thread 'main' panicked at crates/nsqlite/src/eval.rs:
//!   the pattern operators are handled by their own forms
//! ```
//!
//! GLOB is now implemented. REGEXP is **refused** rather than answered, because
//! a real regexp engine is not here and returning 0 or 1 for a pattern this
//! engine never matched would be a wrong answer rather than a gap.
//!
//! Every expectation below was measured against sqlite3 3.53.4 on this machine.

use crate::connection::{Connection, Outcome};
use crate::value::Value;

fn mem() -> Connection {
    Connection::open_memory().expect("opening a memory database")
}

/// The single value the last statement of `sql` produced.
fn value_of(sql: &str) -> Value {
    let mut c = mem();
    let Outcome::Query { rows, .. } = c
        .execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"))
        .into_iter()
        .last()
        .unwrap_or_else(|| panic!("{sql:?} produced no outcome"))
    else {
        panic!("{sql:?} produced no rows")
    };
    rows.into_iter()
        .next()
        .unwrap_or_else(|| panic!("{sql:?} produced no row"))
        .values
        .into_iter()
        .next()
        .unwrap_or(Value::Null)
}

/// The error text of a statement that is expected to be refused.
fn error_of(sql: &str) -> String {
    let mut c = mem();
    match c.execute_script(sql) {
        Ok(_) => panic!("{sql:?} was expected to be refused, and was not"),
        Err(e) => e.message,
    }
}

fn ok(sql: &str) -> Value {
    value_of(sql)
}

fn ev(sql: &str) -> Result<(), crate::error::Error> {
    let mut c = mem();
    c.execute_script(sql).map(|_| ())
}

/// `GLOB` is `LIKE` with different wildcards and no case folding.
///
/// MEASURED, and the four that decide it:
/// ```text
/// 'abc' GLOB 'a*'   -> 1      'abc' GLOB 'A*'   -> 0   <- case-SENSITIVE
/// 'abc' GLOB 'a?c'  -> 1      'abc' GLOB 'a.c'  -> 0   <- `.` is a literal
/// ```
#[test]
fn glob_wildcards_are_star_and_question_mark() {
    for (text, pattern, want) in [
        ("abc", "a*", 1),
        ("abc", "a?c", 1),
        ("abc", "a*c", 1),
        ("abc", "*", 1),
        ("abc", "????", 0),
        ("abc", "???", 1),
        ("", "*", 1),
        ("abc", "", 0),
        ("a", "a", 1),
        ("a", "aa", 0),
        ("aa", "a", 0),
    ] {
        assert_eq!(
            ok(&format!("SELECT '{text}' GLOB '{pattern}'")),
            Value::Integer(want),
            "{text} GLOB {pattern}"
        );
    }
}

#[test]
fn glob_compares_case_sensitively() {
    // MEASURED on sqlite3 3.53.4: `'abc' GLOB 'A*'` is 0, so a case difference
    // decides the match.
    //
    // The comment an earlier draft of this test carried -- "unlike LIKE" -- was
    // wrong, and worth correcting because it is the kind of thing that gets
    // believed. This engine's LIKE is case-SENSITIVE too, and so is the
    // reference's for an ASCII operand with no collation:
    //
    //     'ab' LIKE 'A'   -> 0        'ab' LIKE 'a'  -> 0
    //     'AB' LIKE 'a'   -> 0
    //
    // so GLOB is not the odd one out here, and the two agree on all of it.
    assert_eq!(ok("SELECT 'abc' GLOB 'A*'"), Value::Integer(0));
    assert_eq!(ok("SELECT 'ab' LIKE 'A'"), Value::Integer(0));
    assert_eq!(ok("SELECT 'ab' LIKE 'a'"), Value::Integer(0));
}

#[test]
fn glob_has_character_classes() {
    // MEASURED. A class matches ONE character, `^` negates, and a range is
    // `a-z`. The third row is the one a first reading gets wrong: `a[bc]c` is
    // 1, not 0, because the class matches the `b` and the trailing `c` in the
    // pattern is then... not what a "one class = one character, so a
    // three-character class cannot fit" reading predicts. An earlier draft of
    // this test asserted 0 there, on that reasoning, and was wrong -- the
    // measurement is what settled it.
    for (text, pattern, want) in [
        ("abc", "a[b]c", 1),
        ("abc", "a[bc]c", 1),
        ("abc", "a[^x]c", 1),
        ("abc", "a[a-z]c", 1),
        ("abc", "a[x-z]c", 0),
    ] {
        assert_eq!(
            ok(&format!("SELECT '{text}' GLOB '{pattern}'")),
            Value::Integer(want),
            "{text} GLOB {pattern}"
        );
    }
}

#[test]
fn an_unterminated_bracket_is_a_malformed_pattern() {
    // Not an ordinary character, which is the reading that looks right and is
    // wrong: `'a[b' GLOB 'a[b'` is 0 on sqlite3 3.53.4, though both sides look
    // alike.
    for (text, pattern) in [("a[b", "a[b"), ("a[", "a["), ("[a", "[a"), ("[", "[")] {
        assert_eq!(
            ok(&format!("SELECT '{text}' GLOB '{pattern}'")),
            Value::Integer(0),
            "{text} GLOB {pattern}"
        );
    }
    // In the TEXT it is different, because the pattern can absorb it: these two
    // are 1 on the reference, and an earlier version of the check covered both
    // sides and wrongly refused them.
    assert_eq!(ok("SELECT 'a[b' GLOB 'a*'"), Value::Integer(1));
    assert_eq!(ok("SELECT '[' GLOB '*'"), Value::Integer(1));
}

#[test]
fn a_non_text_operand_is_compared_as_its_spelling() {
    // MEASURED: `1 GLOB '1'` is 1, so a number is compared as the digits it
    // prints rather than refused. A blob contributes its own bytes -- going
    // through `Display` would make it the hex literal `x'31'`, and
    // `x'31' GLOB '1'` is 1 on the reference.
    assert_eq!(ok("SELECT 1 GLOB '1'"), Value::Integer(1));
    assert_eq!(ok("SELECT 1.5 GLOB '1.5'"), Value::Integer(1));
    assert_eq!(ok("SELECT x'31' GLOB '1'"), Value::Integer(1));
}

#[test]
fn glob_with_a_null_operand_is_unknown() {
    // SQLite's ordinary rule: an unknown operand makes the comparison unknown,
    // not false. NULL is not shown here because `ok` panics on it.
    assert_eq!(ok("SELECT 'abc' GLOB NULL"), Value::Null);
    assert_eq!(ok("SELECT NULL GLOB 'a'"), Value::Null);
}

#[test]
fn not_glob_is_the_negation() {
    assert_eq!(ok("SELECT 'abc' NOT GLOB 'a*'"), Value::Integer(0));
    assert_eq!(ok("SELECT 'abc' NOT GLOB 'z*'"), Value::Integer(1));
}

/// `REGEXP` is refused, and says so, rather than crashing or guessing.
///
/// The reference DOES match -- it has a regexp available -- so this is a stated
/// difference rather than a match:
///
/// ```text
/// 'abc' REGEXP 'a'    -> 1     'abc' REGEXP '^a'  -> 1
/// 'abc' REGEXP 'b'    -> 1     '.*b.*'           -> 1
/// 'abc' REGEXP 'x+bc' -> 0     'x' REGEXP NULL   -> NULL
/// NULL  REGEXP 'a'    -> NULL
/// ```
#[test]
fn regexp_is_refused_and_its_null_rules_are_sqlites() {
    // The NULL rules are SQLite's own and come for free, because SQLite's are
    // the ordinary ones: an unknown operand makes the comparison unknown.
    assert_eq!(ok("SELECT NULL REGEXP 'a'"), Value::Null);
    assert_eq!(ok("SELECT 'abc' REGEXP NULL"), Value::Null);
    // The matching itself is missing, and this is what it says.
    let e = ev("SELECT 'abc' REGEXP 'a'").expect_err("REGEXP is not implemented");
    assert!(
        e.message.contains("REGEXP"),
        "expected a message naming REGEXP, got {:?}",
        e.message
    );
}

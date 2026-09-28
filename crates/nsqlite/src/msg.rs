//! The engine's error-message catalogue.
//!
//! SQLite's official test suite compares error text byte for byte, so a
//! message is not free text: every byte of it is part of the contract with
//! sqlite3. Writing one message at each of the fifty-odd sites that raise one
//! means a message is spelled once, changed once, and tested once, and that the
//! spelling cannot drift between two call sites that meant the same thing.
//!
//! This module is the single place the spelling lives. The rest of the engine
//! calls the constructors here and gets an [`Error`] back, so there is no
//! `format!("no such table: {name}")` left anywhere to get subtly wrong.
//!
//! # How a message is put together
//!
//! A message is either a fixed string or a fixed string with named holes in
//! it. A hole takes one [`Arg`], and [`Msg::render`] does the substitution, so
//! the argument's type is checked by the compiler and the punctuation around a
//! hole can never drift from the format string. A message with no holes is in
//! [`Msg::FIXED`] and takes no arguments.
//!
//! # Case: where a name comes from decides which spelling it echoes
//!
//! This is the whole point of the module, and the rule is *not* uniform.
//!
//! The first half of the rule was measured, not reasoned. An unquoted
//! identifier **is** folded to lower case while it is parsed, and it stays
//! folded in the token -- the engine's own tokenizer does the folding, in
//! `scan_identifier`. But a message does not echo the *token*; it echoes the
//! **source text the token was written as**, with the quoting removed. Those
//! are two different things, and the oracle keeps them apart:
//!
//! ```text
//! SELECT * FROM Foo;      ->  no such table: Foo
//! SELECT XYZZY(1);        ->  no such function: XYZZY
//! SELECT ABS(1,2);        ->  wrong number of arguments to function ABS()
//! SELECT BadCol;          ->  no such column: BadCol
//! SELECT NOSUCHCOL;       ->  no such column: NOSUCHCOL
//! SELECT * FROM "Foo";    ->  no such table: Foo
//! SELECT * FROM [Foo];    ->  no such table: Foo
//! SELECT * FROM `Foo`;    ->  no such table: Foo
//! ```
//!
//! So an unquoted name is **not** folded in the message either, and the
//! engine's rule is a single one: **every query-sourced name is the spelling
//! the statement wrote.** The parser recovers that spelling by span -- see
//! `parser::written_name` -- rather than reading it back off the folded token,
//! which is what `after_identifier` already had to do for a function call.
//!
//! The roadmap described the gap as an inconsistency in the *rule* -- an
//! unknown function keeping the case a table name was folded away from. It is
//! not a rule but a *caller*: the function name was read back off the source by
//! span, and every other name was read off the folded token, so the same
//! statement folded in one place and not the other. `msg` has nothing to decide
//! here, because it never folds anything -- the disagreement was always
//! upstream of [`Msg::render`], and `the_two_case_rules_cannot_be_confused` plus
//! the `n*` rows below are the measurement of both directions. A column name is
//! on the keeping side too (`SELECT BadCol` is `no such column: BadCol`), which
//! is a second row the roadmap's framing did not anticipate; folding a column
//! name was tried here and reverted precisely because the oracle does not do
//! it.
//!
//! The second half of the rule is the one that looks like an exception and is
//! not. A **constraint** message echoes the spelling the *schema* holds, which
//! was fixed when the table was created, and the query is not consulted at
//! all:
//!
//! ```text
//! CREATE TABLE Tbl(a,Bb NOT NULL); INSERT INTO tbl VALUES(1,NULL);
//!                           ->  NOT NULL constraint failed: Tbl.Bb
//! CREATE TABLE tbl(a,bb NOT NULL); INSERT INTO TBL VALUES(1,NULL);
//!                           ->  NOT NULL constraint failed: tbl.bb
//! ```
//!
//! The two mechanisms in SQLite's own source line up with the two behaviours.
//! Name-resolution messages use `%#T` (or `%T`), SQLite's conversion for a
//! *token* that reproduces the original spelling; constraint messages use `%s`
//! against the table and column names held in the schema. So the engine's rule
//! is the same rule SQLite follows, and a call site picks the right behaviour
//! by which constructor it reaches for.
//!
//! # Which names are schema-sourced and which are query-sourced
//!
//! Verified by running each against sqlite3 3.53.4 with a deliberately
//! mismatched query spelling:
//!
//! | message                                   | source of the name            |
//! |-------------------------------------------|-------------------------------|
//! | `no such table: N`                          | query (case kept)             |
//! | `no such column: N`                         | query (case kept)             |
//! | `no such column: T.C`                       | query (case kept)             |
//! | `ambiguous column name: N`                  | query (case kept)             |
//! | `ambiguous column name: main.T.C`           | **schema** (case kept)        |
//! | `no such function: N`                       | query (case kept)             |
//! | `no such collation sequence: N`             | query (case kept)             |
//! | `no such index: N`                          | query (case kept)             |
//! | `wrong number of arguments to function N()` | query (case kept)             |
//! | `misuse of aggregate: N()` / `... function N()` | query (case kept)         |
//! | `misuse of aliased aggregate N`             | query (case kept)             |
//! | `near "T": syntax error`                    | query (case kept)             |
//! | `unrecognized token: "T"`                   | query (case kept)             |
//! | `NOT NULL constraint failed: T.C`           | **schema** (query ignored)    |
//! | `UNIQUE constraint failed: T.C`             | **schema** (query ignored)    |
//! | `CHECK constraint failed: CLAUSE`           | **schema** (query ignored)    |
//! | `foreign key mismatch - "C" referencing "P"`| **schema** (query ignored)    |
//! | `table T already exists`                    | query (case kept)             |
//! | `index I already exists`                    | query (case kept)             |
//! | `table T has no column named C`             | query (case kept)             |
//! | `table T has N columns but M values...`     | query (case kept)             |
//! | `Nth ORDER BY / GROUP BY term out of range` | a count, not a name      |
//! | `sub-select returns N columns`              | a count, not a name           |
//! | `SELECTs ... left and right of OP ...`      | an operator, not a name       |
//! | `datatype mismatch`                         | names nothing at all          |
//!
//! Three rows deserve their own note, because they are the ones a uniform rule
//! gets wrong -- and they fall on opposite sides of the line, which is why the
//! line has to be drawn per message rather than per family:
//!
//! * `ambiguous column name: main.T.C` is schema-sourced: with
//!   `CREATE TABLE T(a)` and `SELECT * FROM T, T` the message says
//!   `main.T.a`, with the `T` the schema holds. With `CREATE TABLE MiXeD(a)`
//!   and `SELECT * FROM MiXeD, mixed` it says `main.MiXeD.a` -- and note
//!   that even when the statement spells the table in a *different* case from
//!   the schema (`SELECT * FROM t, t` against a schema of `T`), the schema
//!   spelling is what comes back. Two rows agreeing on the alias still
//!   disambiguate, because the resolver matched them.
//! * `table T has no column named C` sits right next to it in the DML family
//!   and is query-sourced: with `CREATE TABLE MiXeD(A)` and
//!   `INSERT INTO MIXED(zz) VALUES(1)` the message says
//!   `table MIXED has no column named zz`.
//! * `no such table: N` under a schema qualifier echoes *both* halves as
//!   written, and the two disagree about which is which. `DROP TABLE xyz.i`
//!   says `no such table: xyz.i` -- bare, unqualified, and echoing the
//!   query. `CREATE INDEX i ON Foo(a)` says `no such table: main.Foo` --
//!   qualified, and naming the schema the index was made in. The first goes
//!   through the index DDL's own lookup and the second through the FROM
//!   clause's, and neither consults the schema for a name.
//!
//! No message mixes the two sources. Every one of them is wholly query-sourced
//! or wholly schema-sourced, which is what lets [`Msg::case_rule`] answer with
//! a list of the same length for all of them.
//!
//! # Case is the whole rule, and quoting is a second axis of it
//!
//! The rule above is about one thing: whether a name keeps the case the
//! statement wrote. There is a second question a caller can get wrong, and it
//! is about *which characters* were written, not which case: the same name in
//! double quotes is a different message from the same name bare. The oracle
//! says so, and the two are the only place in the family where the quoting
//! reaches the message:
//!
//! ```text
//! SELECT "a+b";           ->  no such column: "a+b" - should this be a string
//!                              literal in single-quotes?
//! SELECT [a+b];           ->  no such column: a+b
//! SELECT `a+b`;           ->  no such column: a+b
//! SELECT t."a+b" FROM t;  ->  no such table: t
//! ```
//!
//! So [`Msg::NoSuchColumnDoubleQuoted`] is a message about the quoting rather
//! than about the name, and no amount of keeping the right case produces it.
//! Three decisions follow, and each is one place rather than a rule spread over
//! call sites:
//!
//! * the tokenizer marks a `"..."` as [`tokenizer::Token::DoubleQuotedIdentifier`]
//!   rather than folding it into `Identifier`, because the quoting is gone by
//!   the time a name fails to resolve and nothing downstream can recover it;
//! * the parser reads a `"..."` wherever a name is read, and answers
//!   `parser::double_quoted_names` for the names a statement wrote that way --
//!   the two DDL shapes out of the text they store, every other shape out of
//!   the range its expressions cover;
//! * the connection collects that list before it runs anything, and the two
//!   places that raise `no such column` -- `Connection::no_such_column_for` and
//!   `aggcheck::name_error` -- ask it before falling back to the plain form.
//!
//! A qualified name is never the quoting's fault, which is the fourth row above
//! and the one exception: `no such column: t.a` is the plain message whatever
//! was written, because the qualifier already says where the name was looked
//! for.
//!
//! # Nothing the engine chose is a name
//!
//! The ordinal at the head of an out-of-range ORDER BY or GROUP BY message is
//! a count, the number in a sub-select column count is a count, and the two
//! numbers in a column-count mismatch are counts. None of them has a case to
//! preserve, and none of them is spelled by a `%T`.
//!
//! # Where each message was checked
//!
//! Every expectation in the tests below was read off the real sqlite3 3.53.4
//! at C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/sqlite3.exe by running
//! the statement named beside it and stripping the shell's own
//! `Parse error in Nth command line argument: ` prefix. The statement is
//! recorded next to every expectation, is a complete statement that creates
//! whatever it needs, and the whole table was replayed against that binary row
//! by row with the recorded text compared to what the binary printed. That
//! replay is what `every_oracle_row_is_replayable` keeps honest.
//!
//! The format strings are also visible in the binary, which is worth knowing
//! because it is how a message can be *checked* rather than merely *run*:
//! `"no such function: %#T"` is there, and so are `"no such table: %s"`,
//! `"misuse of aggregate: %s()"`, `"no such function: %#T"`,
//! `"wrong number of arguments to function %#T()"`, `"misuse of %s function
//! %#T()"`, and `"%r %s BY term out of range - should be between 1 and %d"`.
//! Those are the two families this module documents, visible side by side: `%T`
//! for a token, `%s` for a name already in hand.
//!
//! # Messages the engine has not yet matched
//!
//! Four messages an earlier draft of this catalogue carried do not exist in
//! SQLite 3.53.4. They are gone from [`Msg`] rather than kept as "close
//! enough", because the suite compares the text exactly and a message that is
//! in the catalogue but wrong is worse than one that is missing:
//!
//! * `table t1 has no columns` -- `CREATE TABLE t1()` is a syntax error
//!   (`near ")": syntax error`) in SQLite, so this text is unreachable. The
//!   string is not in the binary either.
//! * `datatype mismatch: 1.5 is not an integer` -- the integer-typed column
//!   that rejects a real says plain `datatype mismatch`, and `abs()` on
//!   `INT64_MIN` says `integer overflow`. The binary carries `datatype
//!   mismatch` and no longer form of it.
//! * `UNIQUE constraint failed: rowid 1` -- a duplicate rowid says
//!   `UNIQUE constraint failed: t.rowid`: the word `rowid`, the table from the
//!   schema, never the value. The binary's format is `UNIQUE constraint failed:
//! %s.%s`.
//! * `near "end": syntax error: CASE requires at least one WHEN` --
//!   `SELECT CASE END;` is `near ";": syntax error`; the string is not in the
//!   binary.
//!
//! A fifth, `index associated with UNIQUE or PRIMARY KEY constraint failed: T.C`,
//! is a real format string in the binary but no input was found that reaches
//! it, so it is not in [`Msg`] either. It was dropped for a different reason
//! than the four above -- the text is genuine, the route to it is not known --
//! and a message the engine cannot raise is not a catalogue entry.
//!
//! What is left that has no oracle is the two `sqlite3_prepare_v2()` API
//! texts, which the CLI never surfaces; see the note above [`Msg`].
//!
//! # Not in here
//!
//! Messages about a file being the wrong shape -- the b-tree, record, pager,
//! and index diagnostics -- are deliberately not catalogue entries. They
//! describe a corrupt database rather than a mistake in a statement, they
//! carry their own wording, and they are not what the suite is comparing when
//! it says "no such function". A catalogued message and a corruption
//! diagnostic should not be confused for one another.
//!
//! The engine's own "not implemented yet" messages *are* in here, under
//! [`Msg::UnsupportedYet`]. They are not SQLite's wording and they will be
//! replaced as each gap closes, but they are produced where a real message
//! belongs, and a temporary message with a permanent home is one that can be
//! found and retired rather than one that scatters across twenty call sites.

use std::fmt;

use crate::error::{Error, ResultCode};

/// The result code a message is raised under, where it is not the default.
///
/// Most messages are `SQLITE_ERROR` (1), which is what `Error::new` with
/// `ResultCode::Error` gives. The two that are not are the ones a caller can
/// branch on without reading the text: a constraint failure is
/// `SQLITE_CONSTRAINT` (19), and a value that does not match the shape a clause
/// demanded is `SQLITE_MISMATCH` (20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgCode {
    /// `SQLITE_ERROR`. The default, and right for a parse error or a name the
    /// engine could not resolve.
    Error,
    /// `SQLITE_CONSTRAINT`.
    Constraint,
    /// `SQLITE_MISMATCH`.
    Mismatch,
}

impl MsgCode {
    /// The [`ResultCode`] this message is raised under.
    pub const fn result_code(self) -> ResultCode {
        match self {
            MsgCode::Error => ResultCode::Error,
            MsgCode::Constraint => ResultCode::Constraint,
            MsgCode::Mismatch => ResultCode::Mismatch,
        }
    }
}

/// One of the holes in a message, and the kind of value that fills it.
///
/// The type is part of the message's contract. [`Arg::Name`] is an identifier,
/// a token, or a clause of SQL, echoed exactly as given; [`Arg::Count`] is a
/// number the engine counted; and [`Arg::Value`] is a value printed the way
/// the engine prints values, which is not a name and does not have a case to
/// preserve. Keeping them apart means a `no such table` cannot be handed a
/// count by accident, which is the kind of mistake that produces a message
/// wrong in a way no test would notice.
///
/// Note what the types do *not* decide. `Arg::Name` is one variant, not two,
/// because whether a name keeps its case is a property of the **message** --
/// see the module docs on schema-sourced versus query-sourced names -- and
/// [`Msg::render`] already encodes which of the two each message wants. Two
/// `Arg::Name` holes in one message can legitimately want opposite behaviour,
/// which is exactly what `no such table: SCHEMA.TABLE` does. [`Arg::as_name`]
/// is how a caller spells "this one is a name" explicitly, and
/// [`Msg::doc::CASE_RULE`] is how the module records which kind each message
/// wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// An identifier, a token, or a clause of SQL, echoed exactly as given.
    Name(String),
    /// A count the engine made, printed in decimal.
    Count(u64),
    /// A value, printed the way this engine prints values.
    Value(String),
}

impl Arg {
    /// The name in this argument, as a borrow.
    ///
    /// Lets a caller spell "this is a name" at a call site instead of relying
    /// on the blanket `From` impls, and lets a test ask which kind a value is.
    pub fn as_name(&self) -> Option<&str> {
        match self {
            Arg::Name(n) => Some(n),
            Arg::Count(_) | Arg::Value(_) => None,
        }
    }

    /// The count in this argument, as a borrow.
    pub fn as_count(&self) -> Option<u64> {
        match self {
            Arg::Count(n) => Some(*n),
            Arg::Name(_) | Arg::Value(_) => None,
        }
    }

    /// The value in this argument, as a borrow.
    pub fn as_value(&self) -> Option<&str> {
        match self {
            Arg::Value(v) => Some(v),
            Arg::Name(_) | Arg::Count(_) => None,
        }
    }
}

impl From<&str> for Arg {
    fn from(v: &str) -> Arg {
        Arg::Name(v.to_string())
    }
}

impl From<String> for Arg {
    fn from(v: String) -> Arg {
        Arg::Name(v)
    }
}

impl From<&String> for Arg {
    fn from(v: &String) -> Arg {
        Arg::Name(v.clone())
    }
}

impl From<u64> for Arg {
    fn from(v: u64) -> Arg {
        count(v)
    }
}

impl From<usize> for Arg {
    fn from(v: usize) -> Arg {
        count(v as u64)
    }
}

impl From<i64> for Arg {
    fn from(v: i64) -> Arg {
        Arg::Value(v.to_string())
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Name(s) | Arg::Value(s) => f.write_str(s),
            Arg::Count(n) => write!(f, "{n}"),
        }
    }
}

/// Convenience constructor: an identifier or token as a message hole.
///
/// Shorter than [`Arg::Name`] at a call site, which is where most holes are,
/// and usable where a short expression reads better than the variant itself.
pub fn name(v: &str) -> Arg {
    Arg::Name(v.to_string())
}

/// Convenience constructor: a value as a message hole, for a message that
/// prints a value rather than a name.
pub fn value(v: &str) -> Arg {
    Arg::Value(v.to_string())
}

/// Convenience constructor: a count as a message hole.
pub const fn count(n: u64) -> Arg {
    Arg::Count(n)
}

/// One message in SQLite's exact wording, with its holes.
///
/// Each variant is one message. The variant's own name is the message's
/// identity, and [`Msg::render`] is the one place the wording and the case rule
/// live, so a message's text is written once no matter how many call sites
/// raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    // --- a name the engine could not resolve -------------------------------
    /// `no such table: NAME`
    ///
    /// Query-sourced: `NAME` keeps the spelling the query used.
    NoSuchTable,
    /// `no such table: SCHEMA.NAME`
    ///
    /// Raised when the schema qualifier names a database the engine has not
    /// got, so the whole reference is echoed. Query-sourced.
    NoSuchTableSchemaQualified,
    /// `no such column: NAME`
    NoSuchColumn,
    /// `no such column: TABLE.COLUMN`
    NoSuchColumnQualified,
    /// `no such column: SCHEMA.TABLE.COLUMN`
    NoSuchColumnSchemaQualified,
    /// `no such column: "TOKEN" - should this be a string literal in
    /// single-quotes?`
    ///
    /// SQLite's hint for a double-quoted string that resolved to nothing. The
    /// hole is the token's text; `Msg::render` supplies the double quotes,
    /// because the quotes are the reason for the hint.
    NoSuchColumnDoubleQuoted,
    /// `ambiguous column name: NAME`
    AmbiguousColumn,
    /// `ambiguous column name: SCHEMA.TABLE.COLUMN`
    ///
    /// Schema-sourced: with `CREATE TABLE T(a)` and `SELECT * FROM T, T` the
    /// message says `main.T.a`, using the name the schema holds.
    AmbiguousColumnStar,
    /// `no such function: NAME`
    NoSuchFunction,
    /// `no such collation sequence: NAME`
    NoSuchCollation,
    /// `no such index: NAME`
    NoSuchIndex,
    /// `wrong number of arguments to function NAME()`
    WrongArgumentCount,

    // --- constraints --------------------------------------------------------
    // Every message in this block is schema-sourced. The table and column come
    // from the schema as it was declared, and the statement's own spelling of
    // them is not consulted.
    /// `NOT NULL constraint failed: TABLE.COLUMN`
    NotNullConstraint,
    /// `UNIQUE constraint failed: TABLE.COLUMN`
    UniqueConstraint,
    /// `UNIQUE constraint failed: TABLE.rowid`
    ///
    /// A duplicate rowid names the word `rowid` and the table from the schema.
    /// It does not name the rowid's value.
    UniqueConstraintRowid,
    /// `CHECK constraint failed: CLAUSE`
    ///
    /// `CLAUSE` is the constraint's own text as written at CREATE TABLE time,
    /// or the constraint's name when it has one.
    CheckConstraint,
    /// `FOREIGN KEY constraint failed`
    ForeignKeyConstraint,
    /// `foreign key mismatch - "CHILD" referencing "PARENT"`
    ForeignKeyMismatch,

    // --- a value that does not match the shape a clause wanted -------------
    /// `datatype mismatch`
    ///
    /// The whole text of SQLITE_MISMATCH. SQLite says exactly this and nothing
    /// more for a real or a string offered to an integer-typed column; it does
    /// not say which value was wrong.
    DatatypeMismatch,
    /// `table TABLE has no column named COLUMN`
    ///
    /// Query-sourced on both sides: with `CREATE TABLE MiXeD(A)` and
    /// `INSERT INTO MIXED(zz) VALUES(1)` the message says
    /// `table MIXED has no column named zz`.
    NoSuchColumnForTable,
    /// `table TABLE has COLS columns but VALS values were supplied`
    ColumnCountMismatch,
    /// `VALS values for COLS columns`
    ///
    /// A different message from [`Msg::ColumnCountMismatch`], used when the
    /// statement named its own column list. `sqlite3` says this for
    /// `INSERT INTO t1(a) VALUES(1,2)`, and it is the SQLITE_MISMATCH
    /// "VALUES list" form rather than the "table" form.
    ValuesForColumnsCount,
    /// `table NAME already exists`
    TableExists,
    /// `there is already an index named NAME`
    IndexExists,
    /// `there is already a table named NAME`
    TableNamedExists,
    /// `there is already an index named NAME`
    ///
    /// The mirror of [`Msg::TableNamedExists`], and the one that comes back
    /// when the index got there first: an index called `i` followed by a table
    /// called `i` says `there is already an index named i`.
    IndexNamedExists,
    /// `duplicate column name: NAME`
    ///
    /// A `CREATE TABLE` that declares the same column twice. Query-sourced:
    /// the name is the one the statement wrote. The binary also carries
    /// `duplicate WITH table name: %s`, which is the same error for a name
    /// already used as a table.
    DuplicateColumnName,
    /// `Cannot add a PRIMARY KEY column`
    ///
    /// An `ALTER TABLE ... ADD COLUMN` whose new column is a primary key. The
    /// restriction is SQLite's, and it is about the *index* a primary key is,
    /// not about the column: measured, `ADD COLUMN c UNIQUE` is refused the
    /// same way even though a unique constraint is equally an index, and
    /// `ADD COLUMN c NOT NULL` is allowed.
    CannotAddPrimaryKeyColumn,
    /// `Cannot add a UNIQUE column`
    ///
    /// The same restriction for a unique constraint. See
    /// [`Msg::CannotAddPrimaryKeyColumn`].
    CannotAddUniqueColumn,
    /// `Cannot add a column to a view`
    ///
    /// A view has no columns of its own to extend. The binary also answers
    /// this for the other objects a view-like name can reach, but a view is
    /// the one this engine can be asked about.
    CannotAddColumnToView,
    /// `Cannot add a NOT NULL column with default value NULL`
    ///
    /// An `ALTER TABLE ... ADD COLUMN c NOT NULL` against a table that already
    /// has a row. The rows on disk are not rewritten by an ALTER, so the new
    /// column would read back as NULL in every one of them; SQLite refuses
    /// rather than write a column its own rows cannot satisfy. The same
    /// statement against an *empty* table is accepted -- measured -- so this
    /// message is raised by the executor rather than by the parser, where the
    /// table's contents are not visible.
    CannotAddNotNullColumn,
    /// `Cannot add a column with non-constant default`
    ///
    /// An `ALTER TABLE ... ADD COLUMN c DEFAULT <expr>` against a table that
    /// already has a row, where `<expr>` is not one of the spellings SQLite
    /// accepts as constant. The rows on disk are not rewritten by an ALTER, so
    /// the default has to be a value the executor can supply later, at read
    /// time; `(1+2)` and `CURRENT_TIMESTAMP` are not.
    ///
    /// It is a *runtime* error, raised by the executor and not the parser, and
    /// only against a table that has rows: measured on 3.53.4, the same ALTER
    /// against an empty table is accepted, and the default is then evaluated
    /// per row as it is inserted. It is a separate message from
    /// [`Msg::DefaultValueNotConstant`], which SQLite raises at parse time,
    /// without looking at the table at all, for the one spelling its grammar
    /// refuses outright.
    CannotAddNonConstantDefault,
    /// `default value of column [NAME] is not constant`
    ///
    /// A column whose `DEFAULT` is a parenthesised *name* -- `DEFAULT (a)`.
    /// SQLite's grammar treats an unparenthesised name as a string literal and
    /// a parenthesised one as a reference, so the same word is a constant in
    /// `DEFAULT a` and a reference in `DEFAULT (a)`, and the reference is
    /// refused here whatever the table holds.
    DefaultValueNotConstant,

    // --- aggregates ---------------------------------------------------------
    /// `misuse of aggregate: NAME()`
    MisuseOfAggregate,
    /// `misuse of aggregate function NAME()`
    MisuseOfAggregateFunction,
    /// `misuse of aliased aggregate NAME`
    MisuseOfAliasedAggregate,
    /// `aggregate functions are not allowed in the GROUP BY clause`
    AggregateNotAllowedInGroupBy,
    /// `HAVING clause on a non-aggregate query`
    HavingOnNonAggregate,
    /// `integer overflow`
    IntegerOverflow,

    // --- ORDER BY and GROUP BY ordinals ------------------------------------
    // The upper bound is the width of the result set, not the width of the
    // table: `CREATE TABLE t(a,b,c); SELECT a,b FROM t GROUP BY 4` says
    // "between 1 and 2", while `SELECT a,b,c FROM t GROUP BY 4` says "1 and 3".
    /// `Nth ORDER BY term out of range - should be between 1 and HI`
    OrderByTermOutOfRange,
    /// `Nth GROUP BY term out of range - should be between 1 and HI`
    GroupByTermOutOfRange,

    // --- compound SELECTs and sub-SELECTs ----------------------------------
    /// `sub-select returns N columns - expected 1`
    SubSelectColumnCount,
    /// `SELECTs to the left and right of OP do not have the same number of
    /// result columns`
    CompoundColumnCount,
    /// `too many terms in compound SELECT`
    TooManyCompoundTerms,
    /// `Nth ORDER BY term does not match any column in the result set`
    OrderByTermNoMatch,

    // --- the syntax error family --------------------------------------------
    /// `near "TOKEN": syntax error`
    SyntaxError,
    /// `incomplete input`
    IncompleteInput,
    /// `unrecognized token: "TEXT"`
    UnrecognizedToken,

    // --- the engine's own "not implemented" answers -------------------------
    /// `X is not supported yet`
    UnsupportedYet,
    /// `REASON: X is not supported yet`
    UnsupportedYetIn,
}

/// The rule for every name hole in a message whose names all came out of the
/// statement.
///
/// A message with two or three names lists one entry for each, so the group
/// below shares this list rather than repeating it. `every_case_rule_entry_is_a_
/// name_hole` is what keeps the shared list the right length: a count here is a
/// count of *name holes*, not of messages in the group.
///
/// Each list below is shared by every message with that many names. Three
/// arities occur, so three lists, and `every_case_rule_entry_is_a_name_hole`
/// holds each of them to the length the render arms say it must be -- a list
/// that is one entry too long would otherwise answer for a message it does not
/// cover without anything noticing.
const NO_NAMES: &[CaseRule] = &[];
const ONE_NAME_QUERY: &[CaseRule] = &[CaseRule::Query];
const ONE_NAME_SCHEMA: &[CaseRule] = &[CaseRule::Schema];
const TWO_NAMES_QUERY: &[CaseRule] = &[CaseRule::Query, CaseRule::Query];
const THREE_NAMES_QUERY: &[CaseRule] = &[CaseRule::Query, CaseRule::Query, CaseRule::Query];
const TWO_NAMES_SCHEMA: &[CaseRule] = &[CaseRule::Schema, CaseRule::Schema];
const THREE_NAMES_SCHEMA: &[CaseRule] = &[CaseRule::Schema, CaseRule::Schema, CaseRule::Schema];

impl Msg {
    /// The result code this message is raised under.
    pub const fn code(self) -> MsgCode {
        match self {
            // A constraint failure is SQLITE_CONSTRAINT. The UNIQUE family
            // included: a duplicate key is a constraint, not a shape mismatch.
            Msg::NotNullConstraint
            | Msg::UniqueConstraint
            | Msg::UniqueConstraintRowid
            | Msg::CheckConstraint
            | Msg::ForeignKeyConstraint => MsgCode::Constraint,
            // SQLITE_MISMATCH's own text is "datatype mismatch" -- see
            // testsuite/src/main.c. A VALUES list whose length does not match
            // the column list is SQLITE_MISMATCH too; that one was checked
            // against the oracle's return code.
            Msg::DatatypeMismatch | Msg::ColumnCountMismatch | Msg::ValuesForColumnsCount => {
                MsgCode::Mismatch
            }
            // Everything else, including the whole syntax error family, is
            // SQLITE_ERROR. A parse error that said Constraint would make a
            // `catch` block in the suite take the wrong branch.
            _ => MsgCode::Error,
        }
    }

    /// The extended result code this message is raised under.
    ///
    /// SQLite's extended codes name the constraint family, so a caller can tell
    /// a NOT NULL from a UNIQUE without parsing the text: 1299 is
    /// `SQLITE_CONSTRAINT_NOTNULL`, 2067 is `SQLITE_CONSTRAINT_UNIQUE`, 275 is
    /// `SQLITE_CONSTRAINT_CHECK`, and 787 is `SQLITE_CONSTRAINT_FOREIGNKEY`. A
    /// message with no extended code is 0 here, and [`Error::extended_code`]
    /// turns that back into the primary code.
    pub const fn extended(self) -> i32 {
        match self {
            Msg::NotNullConstraint => 1299,
            Msg::UniqueConstraint | Msg::UniqueConstraintRowid => 2067,
            Msg::CheckConstraint => 275,
            Msg::ForeignKeyConstraint => 787,
            _ => 0,
        }
    }

    /// Whether each name hole in this message is query-sourced or schema-
    /// sourced, and so whether it keeps the case the engine hands it.
    ///
    /// This is the module's central rule made checkable. It returns one entry
    /// per name hole, in argument order, so a test can assert the rule per
    /// message instead of asserting it in prose, and so the rule is visible to
    /// a caller choosing between two constructors.
    ///
    /// A count or a value is not a name and does not appear. The engine never
    /// lowercases anything: it hands each name the spelling SQLite would use
    /// and this function records *which* spelling that is, so the distinction
    /// that matters lives in one place.
    ///
    /// The list is one entry per **name** hole and nothing else. Three of the
    /// messages below take an operator rather than a name -- the compound-SELECT
    /// operator, the `Nth ORDER BY` ordinal, the `Nth GROUP BY` ordinal -- and
    /// a fourth takes a schema qualifier that is neither query nor schema in
    /// origin but a *literal*: `main` is a keyword, so no query may spell it
    /// any other way. They are grouped with the no-names block for that
    /// reason, and `every_case_rule_entry_is_a_name_hole` keeps the list and
    /// the render arms agreeing about which is which.
    pub const fn case_rule(self) -> &'static [CaseRule] {
        match self {
            // Query-sourced: the name came out of the statement, spelled the
            // way the statement wrote it.
            //
            // Two of these take more than one name, and the counts below are
            // the number of *name* holes each of them has -- not the number of
            // messages in the block. `NoSuchTableSchemaQualified` has two,
            // `NoSuchColumnQualified` two, `NoSuchColumnSchemaQualified` three,
            // `NoSuchColumnForTable` two, `ColumnCountMismatch` one (the other
            // two holes are counts), `AmbiguousColumnStar` three, and
            // `CompoundColumnCount` none.
            Msg::NoSuchTable
            | Msg::NoSuchColumn
            | Msg::NoSuchColumnDoubleQuoted
            | Msg::AmbiguousColumn
            | Msg::NoSuchFunction
            | Msg::NoSuchCollation
            | Msg::NoSuchIndex
            | Msg::WrongArgumentCount
            | Msg::MisuseOfAggregate
            | Msg::MisuseOfAggregateFunction
            | Msg::MisuseOfAliasedAggregate
            | Msg::TableExists
            | Msg::IndexExists
            | Msg::TableNamedExists
            | Msg::IndexNamedExists
            | Msg::DuplicateColumnName
            // One name hole and two count holes, so one entry.
            | Msg::ColumnCountMismatch
            | Msg::SyntaxError
            | Msg::UnrecognizedToken => ONE_NAME_QUERY,
            // Two names: a qualifier and the name it qualifies, or a schema and
            // the table inside it.
            Msg::NoSuchTableSchemaQualified
            | Msg::NoSuchColumnQualified
            | Msg::NoSuchColumnForTable => TWO_NAMES_QUERY,
            // Schema, table, column.
            Msg::NoSuchColumnSchemaQualified => THREE_NAMES_QUERY,
            // No holes at all: the four ALTER TABLE ADD COLUMN refusals are
            // about a column by what it *is* -- a primary key, a unique, a
            // view, a NOT NULL with nothing to default it to -- and not by
            // which one it is. They are grouped with the no-names block for
            // that reason, and `every_case_rule_entry_is_a_name_hole` keeps
            // the list and the render arms agreeing about which is which.
            Msg::CannotAddPrimaryKeyColumn
            | Msg::CannotAddUniqueColumn
            | Msg::CannotAddColumnToView
            | Msg::CannotAddNotNullColumn
            // This one is a fifth ALTER TABLE refusal that carries no name:
            // it is about the *default*, and the column it lands on is named
            // by the statement, not by the complaint.
            | Msg::CannotAddNonConstantDefault => NO_NAMES,
            // The one ALTER-related message that does name a column, because
            // the complaint is about that column's own DEFAULT.
            Msg::DefaultValueNotConstant => ONE_NAME_QUERY,

            // One name: the constraint's own text, or the table of a duplicate
            // rowid. A foreign key names two, and so is below.
            Msg::UniqueConstraintRowid | Msg::CheckConstraint => ONE_NAME_SCHEMA,
            // Two: the table a duplicate rowid belongs to is one, and a foreign
            // key names the child and the parent.
            Msg::ForeignKeyMismatch => TWO_NAMES_SCHEMA,
            // Two names: a table and a column.
            Msg::NotNullConstraint | Msg::UniqueConstraint => TWO_NAMES_SCHEMA,
            // Three: a schema, a table and the column the star collided on.
            Msg::AmbiguousColumnStar => THREE_NAMES_SCHEMA,

            // No names: a fixed phrase, nothing but counts, or a hole that is
            // neither -- an operator, the literal schema name `main`, or a
            // count. None of them is a name, and so none appears above.
            Msg::ForeignKeyConstraint
            | Msg::DatatypeMismatch
            | Msg::AggregateNotAllowedInGroupBy
            | Msg::HavingOnNonAggregate
            | Msg::IntegerOverflow
            | Msg::TooManyCompoundTerms
            | Msg::IncompleteInput
            | Msg::CompoundColumnCount
            | Msg::OrderByTermOutOfRange
            | Msg::OrderByTermNoMatch
            | Msg::GroupByTermOutOfRange
            | Msg::SubSelectColumnCount
            | Msg::ValuesForColumnsCount
            | Msg::UnsupportedYet
            | Msg::UnsupportedYetIn => &[],
        }
    }

    /// Every message in the catalogue, holes and all.
    ///
    /// The completeness tests walk this list, so a message added to [`Msg`]
    /// without a test fails `every_message_has_a_test` rather than sitting
    /// untested in the enum.
    pub const ALL: &'static [Msg] = &[
        Msg::NoSuchTable,
        Msg::NoSuchTableSchemaQualified,
        Msg::NoSuchColumn,
        Msg::NoSuchColumnQualified,
        Msg::NoSuchColumnSchemaQualified,
        Msg::NoSuchColumnDoubleQuoted,
        Msg::AmbiguousColumn,
        Msg::AmbiguousColumnStar,
        Msg::NoSuchFunction,
        Msg::NoSuchCollation,
        Msg::NoSuchIndex,
        Msg::WrongArgumentCount,
        Msg::NotNullConstraint,
        Msg::UniqueConstraint,
        Msg::UniqueConstraintRowid,
        Msg::CheckConstraint,
        Msg::ForeignKeyConstraint,
        Msg::ForeignKeyMismatch,
        Msg::DatatypeMismatch,
        Msg::NoSuchColumnForTable,
        Msg::ColumnCountMismatch,
        Msg::ValuesForColumnsCount,
        Msg::TableExists,
        Msg::IndexExists,
        Msg::TableNamedExists,
        Msg::IndexNamedExists,
        Msg::DuplicateColumnName,
        Msg::MisuseOfAggregate,
        Msg::MisuseOfAggregateFunction,
        Msg::MisuseOfAliasedAggregate,
        Msg::AggregateNotAllowedInGroupBy,
        Msg::HavingOnNonAggregate,
        Msg::IntegerOverflow,
        Msg::OrderByTermOutOfRange,
        Msg::OrderByTermNoMatch,
        Msg::GroupByTermOutOfRange,
        Msg::SubSelectColumnCount,
        Msg::CompoundColumnCount,
        Msg::TooManyCompoundTerms,
        Msg::SyntaxError,
        Msg::IncompleteInput,
        Msg::UnrecognizedToken,
        Msg::UnsupportedYet,
        Msg::UnsupportedYetIn,
    ];
    /// The messages with no holes, in the order the walk test quotes them.
    ///
    /// A message with a hole is deliberately not in this list: the walk test
    /// asserts a fixed message against a literal, and a message with a hole has
    /// no single wording to assert. The messages with holes are asserted
    /// individually, one test per catalogue entry.
    pub const FIXED: &'static [Msg] = &[
        Msg::ForeignKeyConstraint,
        Msg::DatatypeMismatch,
        Msg::AggregateNotAllowedInGroupBy,
        Msg::HavingOnNonAggregate,
        Msg::IntegerOverflow,
        Msg::TooManyCompoundTerms,
        Msg::IncompleteInput,
    ];

    /// Whether this message has a hole in it, and so takes arguments.
    ///
    /// A call site that builds a message from parts it computed can ask this
    /// before it builds one, and the walk test uses it to prove that
    /// [`Msg::ALL`] and [`Msg::FIXED`] agree about the answer.
    pub const fn has_holes(self) -> bool {
        !matches!(
            self,
            Msg::ForeignKeyConstraint
                | Msg::DatatypeMismatch
                | Msg::AggregateNotAllowedInGroupBy
                | Msg::HavingOnNonAggregate
                | Msg::IntegerOverflow
                | Msg::TooManyCompoundTerms
                | Msg::IncompleteInput
        )
    }

    /// Fills in the holes and produces the text sqlite3 would produce.
    ///
    /// No name is ever lowercased here. What each name carries is decided by
    /// where the caller got it from, which [`Msg::case_rule`] records: a
    /// query-sourced name arrives with the spelling the user wrote and goes
    /// into the message unchanged, and a schema-sourced name arrives with the
    /// spelling the schema holds. Both are already in the form SQLite would
    /// print them, so the job here is substitution and nothing else.
    ///
    /// A call with the wrong number of arguments is a mistake in the engine,
    /// not a SQL error, so it panics rather than rendering: a message built
    /// from the wrong parts would otherwise come out as some *other* message,
    /// which is worse than a loud failure.
    pub fn render(self, args: &[Arg]) -> String {
        match (self, args) {
            // A name the engine could not resolve. The name is echoed as
            // written, which is what sqlite3 does for every spelling of a table
            // reference: bare, quoted, and schema-qualified.
            (Msg::NoSuchTable, [Arg::Name(n)]) => format!("no such table: {n}"),
            (Msg::NoSuchTableSchemaQualified, [Arg::Name(db), Arg::Name(n)]) => {
                format!("no such table: {db}.{n}")
            }
            (Msg::NoSuchColumn, [Arg::Name(n)]) => format!("no such column: {n}"),
            (Msg::NoSuchColumnQualified, [Arg::Name(t), Arg::Name(c)]) => {
                format!("no such column: {t}.{c}")
            }
            (Msg::NoSuchColumnSchemaQualified, [Arg::Name(s), Arg::Name(t), Arg::Name(c)]) => {
                format!("no such column: {s}.{t}.{c}")
            }
            // The double quotes go on here: they are the reason for the hint,
            // and the hole is the token's text without them.
            (Msg::NoSuchColumnDoubleQuoted, [Arg::Name(t)]) => format!(
                "no such column: \"{t}\" - should this be a string literal in single-quotes?"
            ),
            (Msg::AmbiguousColumn, [Arg::Name(n)]) => format!("ambiguous column name: {n}"),
            (Msg::AmbiguousColumnStar, [Arg::Name(s), Arg::Name(t), Arg::Name(c)]) => {
                format!("ambiguous column name: {s}.{t}.{c}")
            }
            (Msg::NoSuchFunction, [Arg::Name(n)]) => format!("no such function: {n}"),
            (Msg::NoSuchCollation, [Arg::Name(n)]) => {
                format!("no such collation sequence: {n}")
            }
            (Msg::NoSuchIndex, [Arg::Name(n)]) => format!("no such index: {n}"),
            // The arity error echoes the name too: `SELECT ABS(1,2)` is
            // `wrong number of arguments to function ABS()`.
            (Msg::WrongArgumentCount, [Arg::Name(n)]) => {
                format!("wrong number of arguments to function {n}()")
            }

            // Constraints. Both names come from the schema, fixed when the table
            // was declared: a statement that spells the table differently gets
            // the schema's spelling, not its own.
            (Msg::NotNullConstraint, [Arg::Name(t), Arg::Name(c)]) => {
                format!("NOT NULL constraint failed: {t}.{c}")
            }
            (Msg::UniqueConstraint, [Arg::Name(t), Arg::Name(c)]) => {
                format!("UNIQUE constraint failed: {t}.{c}")
            }
            // The word is `rowid`, never the rowid's value.
            (Msg::UniqueConstraintRowid, [Arg::Name(t)]) => {
                format!("UNIQUE constraint failed: {t}.rowid")
            }
            (Msg::CheckConstraint, [Arg::Name(clause)]) => {
                format!("CHECK constraint failed: {clause}")
            }
            (Msg::ForeignKeyConstraint, []) => "FOREIGN KEY constraint failed".to_string(),
            (Msg::ForeignKeyMismatch, [Arg::Name(child), Arg::Name(parent)]) => {
                format!("foreign key mismatch - \"{child}\" referencing \"{parent}\"")
            }

            // A value that does not match the shape a clause wanted.
            (Msg::DatatypeMismatch, []) => "datatype mismatch".to_string(),
            (Msg::NoSuchColumnForTable, [Arg::Name(t), Arg::Name(c)]) => {
                format!("table {t} has no column named {c}")
            }
            (Msg::ColumnCountMismatch, [Arg::Name(t), Arg::Count(cols), Arg::Count(vals)]) => {
                format!("table {t} has {cols} columns but {vals} values were supplied")
            }
            (Msg::ValuesForColumnsCount, [Arg::Count(vals), Arg::Count(cols)]) => {
                format!("{vals} values for {cols} columns")
            }
            (Msg::TableExists, [Arg::Name(n)]) => format!("table {n} already exists"),
            (Msg::IndexExists, [Arg::Name(n)]) => format!("index {n} already exists"),
            (Msg::TableNamedExists, [Arg::Name(n)]) => {
                format!("there is already a table named {n}")
            }
            (Msg::IndexNamedExists, [Arg::Name(n)]) => {
                format!("there is already an index named {n}")
            }
            (Msg::DuplicateColumnName, [Arg::Name(n)]) => {
                format!("duplicate column name: {n}")
            }
            // The four ALTER TABLE refusals, which carry no name at all: they
            // are about a column by what it is rather than which one it is.
            (Msg::CannotAddPrimaryKeyColumn, []) => "Cannot add a PRIMARY KEY column".to_string(),
            (Msg::CannotAddUniqueColumn, []) => "Cannot add a UNIQUE column".to_string(),
            (Msg::CannotAddColumnToView, []) => "Cannot add a column to a view".to_string(),
            (Msg::CannotAddNotNullColumn, []) => {
                "Cannot add a NOT NULL column with default value NULL".to_string()
            }
            (Msg::CannotAddNonConstantDefault, []) => {
                "Cannot add a column with non-constant default".to_string()
            }
            // The brackets are in the message, not the render: SQLite writes
            // the column's own spelling inside them, so `DEFAULT (a)` on a
            // column named `c` is `default value of column [c] is not
            // constant`.
            (Msg::DefaultValueNotConstant, [Arg::Name(n)]) => {
                format!("default value of column [{n}] is not constant")
            }

            // Aggregates. All three wordings keep the function's case:
            // `SELECT COUNT(a) FROM t WHERE COUNT(a)` is
            // `misuse of aggregate: COUNT()`.
            (Msg::MisuseOfAggregate, [Arg::Name(n)]) => format!("misuse of aggregate: {n}()"),
            (Msg::MisuseOfAggregateFunction, [Arg::Name(n)]) => {
                format!("misuse of aggregate function {n}()")
            }
            (Msg::MisuseOfAliasedAggregate, [Arg::Name(n)]) => {
                format!("misuse of aliased aggregate {n}")
            }
            (Msg::AggregateNotAllowedInGroupBy, []) => {
                "aggregate functions are not allowed in the GROUP BY clause".to_string()
            }
            (Msg::HavingOnNonAggregate, []) => "HAVING clause on a non-aggregate query".to_string(),
            (Msg::IntegerOverflow, []) => "integer overflow".to_string(),

            // An out-of-range ordinal. Both numbers are the engine's own -- the
            // 1-based position of the term and the width of the result set --
            // so neither has a case to preserve. The suffix follows the English
            // rule SQLite uses in its `%r` conversion (testsuite/src/printf.c),
            // which treats the teens as the exception: 11 is `11th`, not
            // `11st`, and 112 is `112th`.
            (Msg::OrderByTermOutOfRange, [Arg::Count(n), Arg::Count(max)]) => format!(
                "{} ORDER BY term out of range - should be between 1 and {max}",
                ordinal(*n)
            ),
            (Msg::GroupByTermOutOfRange, [Arg::Count(n), Arg::Count(max)]) => format!(
                "{} GROUP BY term out of range - should be between 1 and {max}",
                ordinal(*n)
            ),
            (Msg::OrderByTermNoMatch, [Arg::Count(n)]) => format!(
                "{} ORDER BY term does not match any column in the result set",
                ordinal(*n)
            ),

            // Compound and sub-SELECT. The operator is the one the user wrote,
            // so it keeps its case: `UNION ALL`, not `union all`.
            (Msg::SubSelectColumnCount, [Arg::Count(n)]) => {
                format!("sub-select returns {n} columns - expected 1")
            }
            (Msg::CompoundColumnCount, [Arg::Name(op)]) => format!(
                "SELECTs to the left and right of {op} do not have the same number of result columns"
            ),
            (Msg::TooManyCompoundTerms, []) => "too many terms in compound SELECT".to_string(),

            // The syntax error family. The token is echoed as the tokenizer
            // spelled it, so a keyword comes back in the case it was written:
            // `SELECT FROM t` is `near "FROM": syntax error`, not
            // `near "from": syntax error`.
            (Msg::SyntaxError, [Arg::Name(token)]) => format!("near \"{token}\": syntax error"),
            (Msg::IncompleteInput, []) => "incomplete input".to_string(),
            (Msg::UnrecognizedToken, [Arg::Name(text)]) => {
                format!("unrecognized token: \"{text}\"")
            }

            // The engine's own gaps. These are not SQLite's wording and are
            // replaced as each gap closes; a temporary message with a permanent
            // home is one that can be found and retired.
            (Msg::UnsupportedYet, [Arg::Name(what)]) => format!("{what} is not supported yet"),
            (Msg::UnsupportedYetIn, [Arg::Name(reason), Arg::Name(what)]) => {
                format!("{reason}: {what} is not supported yet")
            }

            (_, _) => panic!("wrong number of arguments for {self:?}: got {} parts", args.len()),
        }
    }

    /// The message as an [`Error`], with the holes filled in.
    ///
    /// The result code and the extended code come from the message, not from
    /// the call site, so a constraint failure cannot be raised as a plain error
    /// by a site that forgot which family it was in.
    pub fn error(self, args: &[Arg]) -> Error {
        let text = self.render(args);
        let e = Error::new(self.code().result_code(), text);
        match self.extended() {
            0 => e,
            ext => e.with_extended(ext),
        }
    }
}

/// Where one name hole in a message gets its spelling from.
///
/// The engine never lowercases, so neither of these means "change the case".
/// They record *which* spelling the caller has to hand in, which is the
/// distinction that a uniform rule gets wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseRule {
    /// The name came out of the statement, and carries the spelling the user
    /// wrote: `SELECT * FROM Foo` is `no such table: Foo`.
    Query,
    /// The name came out of the schema, and carries the spelling the table was
    /// created with, whatever the statement said:
    /// `CREATE TABLE tbl(a,bb NOT NULL); INSERT INTO TBL VALUES(1,NULL)` is
    /// `NOT NULL constraint failed: tbl.bb`.
    Schema,
}

impl fmt::Display for Msg {
    /// The rendered message. For a message with holes this panics, because a
    /// message with holes unfilled has no single text; use [`Msg::render`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(&[]))
    }
}

/// A `no such table` error, for the table name as written in the query.
pub fn no_such_table(name: &str) -> Error {
    Msg::NoSuchTable.error(&[name.into()])
}

/// A `no such table: DB.NAME` error, for a reference qualified by a database
/// the engine has not got.
pub fn no_such_table_in_schema(db: &str, table: &str) -> Error {
    Msg::NoSuchTableSchemaQualified.error(&[db.into(), table.into()])
}

/// A `no such column` error, for the column name as written in the query.
pub fn no_such_column(name: &str) -> Error {
    Msg::NoSuchColumn.error(&[name.into()])
}

/// A `no such column: T.C` error, both parts as written in the query.
pub fn no_such_column_qualified(table: &str, column: &str) -> Error {
    Msg::NoSuchColumnQualified.error(&[table.into(), column.into()])
}

/// A `no such column: "T" - should this be a string literal in single-quotes?`
/// error. `token` is the token's text; the message puts the double quotes
/// around it, because the quotes are why sqlite3 is suggesting single quotes.
pub fn no_such_column_double_quoted(token: &str) -> Error {
    Msg::NoSuchColumnDoubleQuoted.error(&[token.into()])
}

/// An `ambiguous column name` error, for the column name as written in the
/// query.
pub fn ambiguous_column(name: &str) -> Error {
    Msg::AmbiguousColumn.error(&[name.into()])
}

/// A `no such function` error, for the function name as written.
///
/// This is the message docs/testing.md 5.3 item 9 is about: sqlite3 answers
/// `SELECT XYZZY(1)` with `no such function: XYZZY`, so the name reaches the
/// message exactly as it was written.
pub fn no_such_function(name: &str) -> Error {
    Msg::NoSuchFunction.error(&[name.into()])
}

/// A `wrong number of arguments to function f()` error, name as written.
pub fn wrong_argument_count(name: &str) -> Error {
    Msg::WrongArgumentCount.error(&[name.into()])
}

/// A `no such collation sequence: c` error, for the name as written.
pub fn no_such_collation(name: &str) -> Error {
    Msg::NoSuchCollation.error(&[name.into()])
}

/// A `table t has no column named c` error, both names as the statement wrote
/// them.
pub fn no_such_column_for_table(table: &str, column: &str) -> Error {
    Msg::NoSuchColumnForTable.error(&[table.into(), column.into()])
}

/// A `table t already exists` error, for the name as the statement wrote it.
pub fn table_exists(name: &str) -> Error {
    Msg::TableExists.error(&[name.into()])
}

/// An `index i already exists` error, for the name as the statement wrote it.
pub fn index_exists(name: &str) -> Error {
    Msg::IndexExists.error(&[name.into()])
}

/// A `no such index: i` error, for the name as written in the query.
pub fn no_such_index(name: &str) -> Error {
    Msg::NoSuchIndex.error(&[name.into()])
}

/// A `NOT NULL constraint failed: T.C` error.
///
/// `table` and `column` are the **schema's** spellings. A statement that writes
/// them differently still gets these: `CREATE TABLE tbl(a,bb NOT NULL)` and
/// `INSERT INTO TBL VALUES(1,NULL)` say `NOT NULL constraint failed: tbl.bb`.
pub fn not_null_constraint(table: &str, column: &str) -> Error {
    Msg::NotNullConstraint.error(&[table.into(), column.into()])
}

/// A `UNIQUE constraint failed: T.C` error, both names schema-sourced.
pub fn unique_constraint(table: &str, column: &str) -> Error {
    Msg::UniqueConstraint.error(&[table.into(), column.into()])
}

/// The `ALTER TABLE ... ADD COLUMN` refusals, which name no column.
///
/// They are grouped here rather than left as `Error::new` calls at the two
/// sites that raise them, because the result code is the part that is easy to
/// get wrong by hand and because a message with no name hole is still a
/// message, not a bare string.
pub mod cannot_add {
    use super::Msg;
    use crate::error::Error;

    pub fn primary_key() -> Error {
        Msg::CannotAddPrimaryKeyColumn.error(&[])
    }
    pub fn unique() -> Error {
        Msg::CannotAddUniqueColumn.error(&[])
    }
    pub fn to_view() -> Error {
        Msg::CannotAddColumnToView.error(&[])
    }
    pub fn not_null() -> Error {
        Msg::CannotAddNotNullColumn.error(&[])
    }
    /// A default that is not one of SQLite's constant spellings, against a
    /// table that already holds a row.
    pub fn non_constant_default() -> Error {
        Msg::CannotAddNonConstantDefault.error(&[])
    }
    /// A `DEFAULT (name)`, which SQLite's grammar refuses at parse time. The
    /// name is the column's, not the name that was written as the default.
    pub fn default_not_constant(column: &str) -> Error {
        Msg::DefaultValueNotConstant.error(&[super::name(column)])
    }
}

/// A `UNIQUE constraint failed: T.rowid` error, for a duplicate rowid.
///
/// The message names the word `rowid` and the table, never the rowid's value.
pub fn unique_constraint_rowid(table: &str) -> Error {
    Msg::UniqueConstraintRowid.error(&[table.into()])
}

/// A `CHECK constraint failed: CLAUSE` error.
///
/// `clause` is the constraint's text as written at CREATE TABLE time, or its
/// name when the constraint has one.
pub fn check_constraint(clause: &str) -> Error {
    Msg::CheckConstraint.error(&[clause.into()])
}

/// The `FOREIGN KEY constraint failed` error, which names nothing at all.
pub fn foreign_key_constraint() -> Error {
    Msg::ForeignKeyConstraint.error(&[])
}

/// A `foreign key mismatch - "CHILD" referencing "PARENT"` error, both names
/// schema-sourced.
pub fn foreign_key_mismatch(child: &str, parent: &str) -> Error {
    Msg::ForeignKeyMismatch.error(&[child.into(), parent.into()])
}

/// The `datatype mismatch` error, which is the whole text of SQLITE_MISMATCH.
pub fn datatype_mismatch() -> Error {
    Msg::DatatypeMismatch.error(&[])
}

/// A `misuse of aggregate: f()` error, name as written.
pub fn misuse_of_aggregate(name: &str) -> Error {
    Msg::MisuseOfAggregate.error(&[name.into()])
}

/// A `misuse of aggregate function f()` error, name as written.
pub fn misuse_of_aggregate_function(name: &str) -> Error {
    Msg::MisuseOfAggregateFunction.error(&[name.into()])
}

/// A `misuse of aliased aggregate a` error, alias as written.
pub fn misuse_of_aliased_aggregate(alias: &str) -> Error {
    Msg::MisuseOfAliasedAggregate.error(&[alias.into()])
}

/// A `near "TOKEN": syntax error` error, token as written.
///
/// A statement that simply ran out has no token to name, and SQLite says
/// [`incomplete_input`] for that instead. Which of the two a stopped statement
/// gets is a decision the caller makes by looking at what the cursor is on; see
/// `parser::syntax_error_here`.
pub fn syntax_error(token: &str) -> Error {
    Msg::SyntaxError.error(&[token.into()])
}

/// An `incomplete input` error: a statement that stopped before it was finished,
/// with no token left to blame.
///
/// Measured against sqlite3 3.53.4, and the whole text -- SQLite's own comment
/// on the format string in `parse.y` says so:
///
/// ```text
/// SELECT * FROM;            ->  near ";": syntax error
/// SELECT * FROM             ->  incomplete input
/// SELECT 1 ORDER BY         ->  incomplete input
/// SELECT CASE END;          ->  near ";": syntax error
/// ```
///
/// So the two are not interchangeable, and a caller that reached this one has
/// already decided the statement ran out rather than hit a token it could not
/// use.
pub fn incomplete_input() -> Error {
    Msg::IncompleteInput.error(&[])
}

/// A `Nth ORDER BY term out of range` error.
///
/// `n` is 1-based and `max` is the width of the result set, as the engine
/// counts it. This is the message docs/testing.md 5.3 item 5 quotes:
/// `SELECT * FROM t5 ORDER BY 3` is
/// `1st ORDER BY term out of range - should be between 1 and 2`.
pub fn order_by_term_out_of_range(n: usize, max: usize) -> Error {
    Msg::OrderByTermOutOfRange.error(&[count(n as u64), count(max as u64)])
}

/// A `Nth GROUP BY term out of range` error.
///
/// `max` is the width of the result set and not the width of the table:
/// `CREATE TABLE t(a,b,c); SELECT a,b FROM t GROUP BY 4` says "between 1 and
/// 2", while the same table with `SELECT a,b,c` says "between 1 and 3".
pub fn group_by_term_out_of_range(n: usize, max: usize) -> Error {
    Msg::GroupByTermOutOfRange.error(&[count(n as u64), count(max as u64)])
}

/// A `sub-select returns N columns - expected 1` error.
pub fn sub_select_column_count(n: usize) -> Error {
    Msg::SubSelectColumnCount.error(&[count(n as u64)])
}

/// A `Nth ORDER BY term does not match any column in the result set` error.
///
/// The counterpart to [`order_by_term_out_of_range`], and raised only by a
/// compound SELECT. A compound has no FROM for an expression key to read, so
/// `ORDER BY y+1` on `SELECT 1 AS x UNION SELECT 2 AS y` names nothing the
/// result set can answer for. Measured against sqlite3 3.53.4, which says
/// `1st ORDER BY term does not match any column in the result set` for every
/// such term, including one whose name is not a result column at all.
pub fn order_by_term_no_match(n: usize) -> Error {
    Msg::OrderByTermNoMatch.error(&[count(n as u64)])
}

/// A `too many terms in compound SELECT` error.
///
/// SQLite's limit is `SQLITE_MAX_COMPOUND_SELECT`, which is 500 in the build
/// under test: 500 UNIONed arms parse and 501 do not.
pub fn too_many_compound_terms() -> Error {
    Msg::TooManyCompoundTerms.error(&[])
}

/// An `X is not supported yet` error, for a gap in this engine.
pub fn unsupported_yet(what: &str) -> Error {
    Msg::UnsupportedYet.error(&[what.into()])
}

/// A `REASON: X is not supported yet` error, for a gap in this engine.
pub fn unsupported_yet_in(reason: &str, what: &str) -> Error {
    Msg::UnsupportedYetIn.error(&[reason.into(), what.into()])
}

/// A count as an English ordinal: 1st, 2nd, 3rd, 4th, 11th, 21st, 112th.
///
/// SQLite puts this at the head of `Nth ORDER BY term out of range`, so the
/// suffix has to follow the English rule rather than a simple mod-10 one: 11 is
/// `11th`, not `11st`, and 112 is `112th`, not `112nd`. SQLite's own `%r`
/// conversion special-cases the tens digit for the same reason; see
/// testsuite/src/printf.c.
pub fn ordinal(n: u64) -> String {
    if (11..=13).contains(&(n % 100)) {
        return format!("{n}th");
    }
    let suffix = match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// A note on the texts in this module that have no oracle, kept as prose
/// because there are none to keep.
///
/// The rule this module holds itself to is that a catalogue entry is a
/// *verified* message, or it is not an entry. So anything that could not be run
/// against the real sqlite3 is described here rather than asserted as if it had
/// been, and nothing in [`Msg`] is unverified.
///
/// Two texts were wanted and could not be obtained, and are the reason this
/// note exists at all:
///
/// - `no statement found` is what `sqlite3_prepare_v2()` produces when the
///   input holds no statement. The CLI never surfaces it: `sqlite3 :memory:
///   '   '` prints nothing at all and exits 0. The string is in no sqlite3
///   binary on this machine and in no file under `testsuite/`, and the
///   environment has no C compiler, so `sqlite3_prepare_v2` could not be called
///   directly to read it out of the library.
/// - `more than one statement in the input` is the companion, raised when a
///   caller asks `sqlite3_prepare_v2` for one statement and the input holds
///   more. Same story: the CLI accepts `SELECT 1; SELECT 2;` happily, so the
///   text cannot be provoked from the shell.
///
/// Both are in the SQLite API surface but not in the CLI's, and neither is among
/// the messages the official test suite compares for a *statement*, so they are
/// not in [`Msg`]. An earlier draft of this module did carry them, in
/// [`Msg::FIXED`], with prose where the `sql` field belonged -- which is exactly
/// how the four invented messages got in, and why this note is here instead.
///
/// When a caller needs one of the two, it should take the text from the
/// library's own headers or a C probe, add a [`Msg`] variant, and give it a
/// case in the table whose `sql` is whatever that probe established. Not a
/// guess in a comment.
///
/// A third family has no oracle by nature rather than by accident: the engine's
/// own `X is not supported yet` messages describe *this* engine's gaps, and
/// their rows are marked `nsqlite only:` so they are never confused with
/// SQLite's wording.

// ---------------------------------------------------------------------------
// The tests.
//
// Every expected string below was read off the real sqlite3 3.53.4 at
// C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/sqlite3.exe by running the
// statement named beside it and taking the first line of its output with the
// shell's own `Parse error in Nth command line argument: ` prefix stripped. The
// statement is named beside every expectation so an expectation can be
// re-derived rather than trusted, and the shapes sqlite3's own source confirms
// are cited to the line that formats them.
//
// The `sql` field is not documentation. Every row's statement is *executable*,
// it is self-contained -- it creates whatever tables it needs, because a
// statement that only works against a schema set up elsewhere is a statement
// that can silently stop producing the message it claims -- and the whole table
// was replayed against the binary above, row by row, with the expected text
// compared to what the binary actually printed. That replay is what
// `every_oracle_row_is_replayable` keeps honest, and it is the reason the four
// messages this module used to carry that do not exist in SQLite are gone
// rather than asserted: nothing in here can claim to be oracle output unless
// the oracle was seen to produce it.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The binary every expectation in this file was taken from.
    const ORACLE: &str = "sqlite3 3.53.4";

    /// The keywords a row's statement is allowed to begin with.
    ///
    /// Used by `every_oracle_row_is_replayable` to tell a statement from prose.
    /// Deliberately not exhaustive: it covers the forms this table uses, and a
    /// new one has to be added here, which is the intended friction.
    const SQL_KEYWORDS: &[&str] = &[
        "SELECT", "CREATE", "DROP", "INSERT", "DELETE", "UPDATE", "PRAGMA", "WITH", "REINDEX",
        "ANALYZE", "EXPLAIN", "VALUES", "REPLACE", "BEGIN", "COMMIT", "ROLLBACK", "ALTER",
        "VACUUM",
    ];

    /// A 501-arm compound SELECT, named by a recipe rather than written out.
    ///
    /// The literal is 9,508 characters, which no one can review, so the row
    /// names how to build it instead. `every_oracle_row_is_replayable` skips
    /// exactly this row, and the count of skipped rows is asserted, so a second
    /// one cannot be added by accident.
    const COMPOUND_501: &str =
        "SELECT 1 UNION ALL SELECT 1 (repeated until there are 501 arms in all)";

    /// One catalogued message: the parts it is given, the statement that was
    /// run to get the expected text, and the text itself.
    ///
    /// A `Case` in the table below is a message the oracle has been asked
    /// about and has answered. Each `Case` also gets a generated `#[test]`,
    /// keyed by an index rather than by `(msg, text)`, so two cases that happen
    /// to render the same text are still two independent tests rather than one
    /// test run twice. See `a_case_and_its_test_are_one_to_one`.
    struct Case {
        msg: Msg,
        args: Vec<Arg>,
        /// The statement that was run against the oracle. `nsqlite only:`
        /// marks the ones that are this engine's own gaps rather than
        /// SQLite's wording.
        sql: &'static str,
        /// The first line of the oracle's output, prefix stripped.
        text: &'static str,
    }

    /// Builds a case out of parts, so the table below reads as data.
    fn c(msg: Msg, args: Vec<Arg>, sql: &'static str, text: &'static str) -> Case {
        Case {
            msg,
            args,
            sql,
            text,
        }
    }

    /// Every message in the catalogue, with the oracle's answer for it.
    /// The table itself. It holds owned [`Arg`]s, which a `static` cannot, so
    /// it is built once per test thread and then leaked so a test can hold a
    /// `&'static` reference to it. The leak is one table per test thread and
    /// the table is read-only, so it costs a few kilobytes for the run and
    /// buys a table that is written as plain data.
    fn cases() -> &'static [Case] {
        thread_local! {
            static CASES: std::cell::RefCell<Option<&'static [Case]>> =
                const { std::cell::RefCell::new(None) };
        }
        CASES.with(|cell| {
            if cell.borrow().is_none() {
                let leaked: &'static mut Vec<Case> = Box::leak(Box::new(build_cases()));
                // SAFETY-adjacent reasoning, done the safe way: `leaked` is a
                // `&'static mut`, so the slice derived from it is `&'static`
                // and is never written to again.
                let slice: &'static [Case] = &*leaked;
                *cell.borrow_mut() = Some(slice);
            }
            cell.borrow().expect("just set")
        })
    }

    fn build_cases() -> Vec<Case> {
        let cases: Vec<Case> = vec![
        // --- names the engine could not resolve ----------------------------
        c(Msg::NoSuchTable, vec![name("Foo")], "SELECT * FROM Foo;", "no such table: Foo"),
        c(Msg::NoSuchTable, vec![name("AAAAAA")], "SELECT * FROM AAAAAA;", "no such table: AAAAAA"),
        c(Msg::NoSuchTable, vec![name("Folder")], "SELECT * FROM Folder;", "no such table: Folder"),
        c(Msg::NoSuchTable, vec![name("main.ab")], "SELECT * FROM main.ab;", "no such table: main.ab"),
        c(Msg::NoSuchTable, vec![name("Main.NOSUCH")], "SELECT * FROM Main.NOSUCH;", "no such table: Main.NOSUCH"),
        c(Msg::NoSuchTable, vec![name("xyzzy")], "SELECT * FROM xyzzy;", "no such table: xyzzy"),
        // The unquoted spelling is the one the message echoes. This is the row
        // the roadmap called out: a function name keeps its case, and so does
        // a table name -- neither is folded, and the rows above with a
        // lower-case spelling are the counter-examples, not the rule.
        c(Msg::NoSuchTable, vec![name("XYZZY")], "SELECT * FROM XYZZY;", "no such table: XYZZY"),
        c(Msg::NoSuchTable, vec![name("XyZzY")], "SELECT * FROM XyZzY;", "no such table: XyZzY"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "SELECT * FROM MiXeD;", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "DELETE FROM MiXeD;", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "UPDATE MiXeD SET a=1;", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "INSERT INTO MiXeD VALUES(1);", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "DROP TABLE MiXeD;", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("MiXeD")], "ALTER TABLE MiXeD RENAME TO Other;", "no such table: MiXeD"),
        c(Msg::NoSuchTable, vec![name("Main.NOSUCH")], "SELECT * FROM Main.NOSUCH;", "no such table: Main.NOSUCH"),
        // A bracketed or backticked name comes back bare, and `Foo` is its
        // first row's spelling too -- which is why this case exists: without
        // it, the quoted row would be a silent duplicate of the first.
        c(Msg::NoSuchTable, vec![name("Foo")], "SELECT * FROM [Foo];", "no such table: Foo"),
        c(Msg::NoSuchTable, vec![name("Foo")], "SELECT * FROM \"Foo\";", "no such table: Foo"),
        c(Msg::NoSuchTable, vec![name("Foo")], "SELECT * FROM `Foo`;", "no such table: Foo"),
        c(Msg::NoSuchTableSchemaQualified, vec![name("xyz"), name("i")], "DROP TABLE xyz.i;", "no such table: xyz.i"),
        c(Msg::NoSuchTableSchemaQualified, vec![name("xyz"), name("i")], "SELECT * FROM xyz.i;", "no such table: xyz.i"),
        c(Msg::NoSuchTableSchemaQualified, vec![name("main"), name("i")], "DROP TABLE main.i;", "no such table: main.i"),
        // The same two names reach this message by two routes and the routes
        // disagree about qualifying. The index DDL qualifies with the schema
        // the index was to be made in; the table DDL echoes the name bare.
        c(Msg::NoSuchTableSchemaQualified, vec![name("main"), name("MiXeD")], "CREATE INDEX i ON MiXeD(a);", "no such table: main.MiXeD"),
        c(Msg::NoSuchColumn, vec![name("nosuchcol")], "SELECT nosuchcol;", "no such column: nosuchcol"),
        c(Msg::NoSuchColumn, vec![name("NOSUCHCOL")], "SELECT NOSUCHCOL;", "no such column: NOSUCHCOL"),
        c(Msg::NoSuchColumn, vec![name("rowid_")], "SELECT rowid_;", "no such column: rowid_"),
        c(Msg::NoSuchColumn, vec![name("LEFT")], "SELECT [LEFT];", "no such column: LEFT"),
        c(Msg::NoSuchColumn, vec![name("XyZzY")], "SELECT XyZzY;", "no such column: XyZzY"),
        c(Msg::NoSuchColumn, vec![name("MiXeD")], "SELECT MiXeD;", "no such column: MiXeD"),
        c(Msg::NoSuchColumn, vec![name("nosuchcol")], "SELECT nosuchcol FROM (SELECT 1 AS x);", "no such column: nosuchcol"),
        c(Msg::NoSuchColumnQualified, vec![name("t"), name("NOSUCHCOL")], "CREATE TABLE t(a); SELECT t.NOSUCHCOL FROM t;", "no such column: t.NOSUCHCOL"),
        c(Msg::NoSuchColumnQualified, vec![name("alias"), name("b")], "CREATE TABLE t(a); SELECT alias.b FROM t alias;", "no such column: alias.b"),
        // A qualified name echoes both halves the way the statement wrote
        // them, and an alias spelled in a different case from the row it
        // matched still comes back as the statement wrote it.
        c(Msg::NoSuchColumnQualified, vec![name("t"), name("FoO")], "SELECT t.FoO FROM (SELECT 1 AS x) AS t;", "no such column: t.FoO"),
        c(Msg::NoSuchColumnQualified, vec![name("T"), name("FoO")], "SELECT T.FoO FROM (SELECT 1 AS x) AS t;", "no such column: T.FoO"),
        c(Msg::NoSuchColumnSchemaQualified, vec![name("main"), name("docs"), name("x")], "SELECT main.docs.x;", "no such column: main.docs.x"),
        c(Msg::NoSuchColumnSchemaQualified, vec![name("Main"), name("T1"), name("nosuchcol")], "CREATE TABLE t1(a); SELECT Main.T1.nosuchcol FROM t1;", "no such column: Main.T1.nosuchcol"),
        c(Msg::NoSuchColumnSchemaQualified, vec![name("temp"), name("t"), name("a")], "CREATE TABLE t(a); SELECT temp.t.a FROM t;", "no such column: temp.t.a"),
        c(Msg::NoSuchColumnSchemaQualified, vec![name("main"), name("t"), name("FoO")], "SELECT main.t.FoO FROM (SELECT 1 AS x) AS t;", "no such column: main.t.FoO"),
        c(Msg::NoSuchColumnSchemaQualified, vec![name("main"), name("T"), name("FoO")], "SELECT main.T.FoO FROM (SELECT 1 AS x) AS t;", "no such column: main.T.FoO"),
        // SQLite's hint for a double-quoted string that resolved to nothing. The
        // hole is the token's text, and `Msg::render` puts the double quotes
        // around it, because the quotes are the reason for the hint.
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("a+b")], "SELECT \"a+b\";", "no such column: \"a+b\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("a b")], "SELECT \"a b\";", "no such column: \"a b\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("zzz")], "SELECT \"zzz\";", "no such column: \"zzz\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("select")], "SELECT \"select\";", "no such column: \"select\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("a+b")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE \"a+b\"=1;", "no such column: \"a+b\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("a+b")], "CREATE TABLE t(a); SELECT 1 FROM t ORDER BY \"a+b\";", "no such column: \"a+b\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("NOSUCHCOL")], "SELECT \"NOSUCHCOL\";", "no such column: \"NOSUCHCOL\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("a+b")], "CREATE TABLE t(a); SELECT 1 FROM t GROUP BY \"a+b\";", "no such column: \"a+b\" - should this be a string literal in single-quotes?"),
        c(Msg::NoSuchColumnDoubleQuoted, vec![name("zz")], "SELECT 1 WHERE \"zz\"=1;", "no such column: \"zz\" - should this be a string literal in single-quotes?"),
        c(Msg::AmbiguousColumn, vec![name("x")], "CREATE TABLE p1(x); CREATE TABLE p2(x); SELECT x FROM p1, p2;", "ambiguous column name: x"),
        c(Msg::AmbiguousColumn, vec![name("X")], "CREATE TABLE p1(X); CREATE TABLE p2(X); SELECT X FROM p1, p2;", "ambiguous column name: X"),
        c(Msg::AmbiguousColumn, vec![name("A.f1")], "CREATE TABLE test1(f1); SELECT A.f1, f1 FROM test1 as A, test1 as A ORDER BY f2;", "ambiguous column name: A.f1"),
        c(Msg::AmbiguousColumn, vec![name("rowid")], "CREATE TABLE t1(a); CREATE TABLE t2(a); SELECT rowid FROM t1, t2;", "ambiguous column name: rowid"),
        // The star form is schema-sourced, which is the one place in the
        // resolution family where the query's spelling is not what is printed.
        c(Msg::AmbiguousColumnStar, vec![name("main"), name("t"), name("a")], "CREATE TABLE t(a); SELECT * FROM t, t;", "ambiguous column name: main.t.a"),
        c(Msg::AmbiguousColumnStar, vec![name("main"), name("T"), name("a")], "CREATE TABLE T(a); SELECT * FROM T, T;", "ambiguous column name: main.T.a"),
        c(Msg::AmbiguousColumnStar, vec![name("main"), name("MiXeD"), name("a")], "CREATE TABLE MiXeD(a); SELECT * FROM MiXeD, mixed;", "ambiguous column name: main.MiXeD.a"),
        // The schema spelling, not the query's: the statement below says `t`
        // twice and the schema says `T`, and `T` is what comes back. This is
        // the one name-resolution message the query does not decide, and it
        // is on the schema side of the line for exactly that reason.
        c(Msg::AmbiguousColumnStar, vec![name("main"), name("T"), name("a")], "CREATE TABLE T(a); SELECT * FROM t, t;", "ambiguous column name: main.T.a"),
        c(Msg::AmbiguousColumnStar, vec![name("main"), name("t"), name("a")], "CREATE TABLE t(a); SELECT * FROM T, T;", "ambiguous column name: main.t.a"),
        c(Msg::NoSuchFunction, vec![name("XYZZY")], "SELECT XYZZY(1);", "no such function: XYZZY"),
        c(Msg::NoSuchFunction, vec![name("xyzzy")], "SELECT xyzzy(1);", "no such function: xyzzy"),
        c(Msg::NoSuchFunction, vec![name("XyZzY")], "SELECT XyZzY(1);", "no such function: XyZzY"),
        c(Msg::NoSuchFunction, vec![name("AbC")], "SELECT AbC(1);", "no such function: AbC"),
        c(Msg::NoSuchFunction, vec![name("nosuchfn")], "SELECT nosuchfn(1);", "no such function: nosuchfn"),
        c(Msg::NoSuchFunction, vec![name("XYZ")], "CREATE TABLE t(a CHECK(XYZ(1)));", "no such function: XYZ"),
        c(Msg::NoSuchFunction, vec![name("XYZ")], "CREATE TABLE t(a); SELECT 1 FROM t ORDER BY XYZ(1);", "no such function: XYZ"),
        c(Msg::NoSuchCollation, vec![name("nosuchcoll")], "CREATE TABLE t(a); SELECT a FROM t ORDER BY a COLLATE nosuchcoll;", "no such collation sequence: nosuchcoll"),
        c(Msg::NoSuchCollation, vec![name("XYZ")], "CREATE TABLE t(a TEXT COLLATE XYZ);", "no such collation sequence: XYZ"),
        c(Msg::NoSuchCollation, vec![name("xyz")], "CREATE TABLE t(a TEXT COLLATE xyz);", "no such collation sequence: xyz"),
        c(Msg::NoSuchCollation, vec![name("nosuchcoll")], "CREATE TABLE t(a); CREATE INDEX i ON t(a COLLATE nosuchcoll);", "no such collation sequence: nosuchcoll"),
        c(Msg::NoSuchIndex, vec![name("nosuch")], "DROP INDEX nosuch;", "no such index: nosuch"),
        c(Msg::NoSuchIndex, vec![name("MyIdx")], "DROP INDEX \"MyIdx\";", "no such index: MyIdx"),
        c(Msg::NoSuchIndex, vec![name("MiXeD")], "DROP INDEX MiXeD;", "no such index: MiXeD"),
        c(Msg::WrongArgumentCount, vec![name("abs")], "SELECT abs(1,2);", "wrong number of arguments to function abs()"),
        c(Msg::WrongArgumentCount, vec![name("ABS")], "SELECT ABS(1,2);", "wrong number of arguments to function ABS()"),
        c(Msg::WrongArgumentCount, vec![name("Abs")], "SELECT Abs(1,2);", "wrong number of arguments to function Abs()"),
        c(Msg::WrongArgumentCount, vec![name("count")], "SELECT count(1,2);", "wrong number of arguments to function count()"),
        c(Msg::WrongArgumentCount, vec![name("count")], "CREATE TABLE t(a,b); SELECT count(a,b) FROM t;", "wrong number of arguments to function count()"),
        c(Msg::WrongArgumentCount, vec![name("coalesce")], "SELECT coalesce();", "wrong number of arguments to function coalesce()"),
        c(Msg::WrongArgumentCount, vec![name("group_concat")], "SELECT group_concat();", "wrong number of arguments to function group_concat()"),
        c(Msg::WrongArgumentCount, vec![name("ifnull")], "SELECT ifnull();", "wrong number of arguments to function ifnull()"),
        // The misuse family: a function the engine does not have as an
        // aggregate is named as the statement wrote it, which is the same
        // rule the unknown-function message follows.
        c(Msg::MisuseOfAggregateFunction, vec![name("Total")], "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT 1 FROM t WHERE Total(a);", "misuse of aggregate function Total()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("Total")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE Total(a);", "misuse of aggregate function Total()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("count")], "SELECT 1 WHERE count(1);", "misuse of aggregate function count()"),
        c(Msg::MisuseOfAggregate, vec![name("COUNT")], "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3); SELECT COUNT(a) FROM t WHERE COUNT(a);", "misuse of aggregate: COUNT()"),
        c(Msg::MisuseOfAggregate, vec![name("SUM")], "CREATE TABLE t(a); SELECT COUNT(a) FROM t WHERE SUM(a);", "misuse of aggregate: SUM()"),
        // --- constraints ---------------------------------------------------
        // The pair below is the heart of the case rule, so both directions are
        // quoted: the schema decides, and the query's spelling is ignored.
        c(Msg::NotNullConstraint, vec![name("MiXeD"), name("B")], "CREATE TABLE MiXeD(a,B NOT NULL); INSERT INTO MiXeD VALUES(1,NULL);", "NOT NULL constraint failed: MiXeD.B"),
        c(Msg::NotNullConstraint, vec![name("Tbl"), name("Bb")], "CREATE TABLE Tbl(a,Bb NOT NULL); INSERT INTO tbl VALUES(1,NULL);", "NOT NULL constraint failed: Tbl.Bb"),
        c(Msg::NotNullConstraint, vec![name("tbl"), name("bb")], "CREATE TABLE tbl(a,bb NOT NULL); INSERT INTO TBL VALUES(1,NULL);", "NOT NULL constraint failed: tbl.bb"),
        c(Msg::NotNullConstraint, vec![name("TBL"), name("BB")], "CREATE TABLE TBL(a, BB NOT NULL); INSERT INTO tbl VALUES(1,NULL);", "NOT NULL constraint failed: TBL.BB"),
        c(Msg::NotNullConstraint, vec![name("t1"), name("b")], "CREATE TABLE t1(a,b NOT NULL); INSERT INTO t1 VALUES(1,NULL);", "NOT NULL constraint failed: t1.b"),
        // A UNIQUE failure needs the second value to collide, not merely to be
        // a second row.
        c(Msg::UniqueConstraint, vec![name("U"), name("A")], "CREATE TABLE U(A UNIQUE); INSERT INTO U VALUES(1); INSERT INTO U VALUES(1);", "UNIQUE constraint failed: U.A"),
        c(Msg::UniqueConstraint, vec![name("t2"), name("b")], "CREATE TABLE t2(a,b UNIQUE); INSERT INTO t2 VALUES(1,1); INSERT INTO t2 VALUES(2,1);", "UNIQUE constraint failed: t2.b"),
        c(Msg::UniqueConstraint, vec![name("MiXeD"), name("B")], "CREATE TABLE MiXeD(A,B UNIQUE); INSERT INTO MiXeD VALUES(1,1); INSERT INTO mixed VALUES(2,1);", "UNIQUE constraint failed: MiXeD.B"),
        // A duplicate rowid names the word `rowid` and the table, never the
        // rowid's value.
        c(Msg::UniqueConstraintRowid, vec![name("t2")], "CREATE TABLE t2(a); INSERT INTO t2(rowid,a) VALUES(1,1); INSERT INTO t2(rowid,a) VALUES(1,2);", "UNIQUE constraint failed: t2.rowid"),
        c(Msg::UniqueConstraintRowid, vec![name("MiXeD")], "CREATE TABLE MiXeD(a); INSERT INTO MiXeD(rowid,a) VALUES(1,1); INSERT INTO MiXeD(rowid,a) VALUES(1,2);", "UNIQUE constraint failed: MiXeD.rowid"),
        c(Msg::CheckConstraint, vec![name("b>0")], "CREATE TABLE t3(a,b CHECK(b>0)); INSERT INTO t3 VALUES(1,-1);", "CHECK constraint failed: b>0"),
        c(Msg::CheckConstraint, vec![name("a<10")], "CREATE TABLE t3(a CHECK(a<10)); INSERT INTO t3 VALUES(99);", "CHECK constraint failed: a<10"),
        c(Msg::CheckConstraint, vec![name("A>0")], "CREATE TABLE t3(a CHECK(A>0)); INSERT INTO T3 VALUES(-1);", "CHECK constraint failed: A>0"),
        c(Msg::CheckConstraint, vec![name("a > 10")], "CREATE TABLE t3(a CHECK(a > 10)); INSERT INTO t3 VALUES(1);", "CHECK constraint failed: a > 10"),
        c(Msg::CheckConstraint, vec![name("ck")], "CREATE TABLE t3(a, CONSTRAINT ck CHECK(a<10)); INSERT INTO t3 VALUES(99);", "CHECK constraint failed: ck"),
        c(Msg::CheckConstraint, vec![name("Ck")], "CREATE TABLE t3(a, CONSTRAINT Ck CHECK(a>0)); INSERT INTO t3 VALUES(-1);", "CHECK constraint failed: Ck"),
        c(Msg::ForeignKeyConstraint, vec![], "PRAGMA foreign_keys=ON; CREATE TABLE p(x PRIMARY KEY); CREATE TABLE Ch(y REFERENCES p(x)); INSERT INTO Ch VALUES(9);", "FOREIGN KEY constraint failed"),
        c(Msg::ForeignKeyMismatch, vec![name("chi"), name("par")], "PRAGMA foreign_keys=ON; CREATE TABLE par(x PRIMARY KEY); CREATE TABLE chi(y REFERENCES par(zzz)); INSERT INTO chi VALUES(1);", "foreign key mismatch - \"chi\" referencing \"par\""),
        c(Msg::ForeignKeyMismatch, vec![name("Ch"), name("PaReNt")], "PRAGMA foreign_keys=ON; CREATE TABLE PaReNt(x PRIMARY KEY); CREATE TABLE Ch(y REFERENCES PaReNt(Zzz)); INSERT INTO Ch VALUES(1);", "foreign key mismatch - \"Ch\" referencing \"PaReNt\""),
        // --- a value that does not match the shape a clause wanted ---------
        c(Msg::DatatypeMismatch, vec![], "CREATE TABLE t(a); SELECT 1 FROM t LIMIT 'x';", "datatype mismatch"),
        c(Msg::DatatypeMismatch, vec![], "CREATE TABLE t(a); SELECT 1 FROM t LIMIT 1.5;", "datatype mismatch"),
        // An integer-typed column that rejects a real says the bare phrase. It
        // does not say which value was wrong.
        c(Msg::DatatypeMismatch, vec![], "CREATE TABLE t4(a INTEGER PRIMARY KEY); INSERT INTO t4 VALUES(1.5);", "datatype mismatch"),
        c(Msg::DatatypeMismatch, vec![], "CREATE TABLE t4(a INTEGER PRIMARY KEY); INSERT INTO t4 VALUES('x');", "datatype mismatch"),
        c(Msg::DatatypeMismatch, vec![], "CREATE TABLE t4(a INTEGER PRIMARY KEY); INSERT INTO t4 VALUES(x'01');", "datatype mismatch"),
        c(Msg::NoSuchColumnForTable, vec![name("MiXeD"), name("zz")], "CREATE TABLE MiXeD(A); INSERT INTO MiXeD(zz) VALUES(1);", "table MiXeD has no column named zz"),
        // Both sides are the query's spelling here, not the schema's.
        c(Msg::NoSuchColumnForTable, vec![name("MIXED"), name("zz")], "CREATE TABLE MiXeD(A); INSERT INTO MIXED(zz) VALUES(1);", "table MIXED has no column named zz"),
        c(Msg::NoSuchColumnForTable, vec![name("t1"), name("nocol")], "CREATE TABLE t1(a); INSERT INTO t1(nocol) VALUES(1);", "table t1 has no column named nocol"),
        c(Msg::ColumnCountMismatch, vec![name("MiXeD"), count(3), count(4)], "CREATE TABLE MiXeD(a,b,c); INSERT INTO MiXeD VALUES(1,2,3,4);", "table MiXeD has 3 columns but 4 values were supplied"),
        c(Msg::ColumnCountMismatch, vec![name("mixed"), count(3), count(4)], "CREATE TABLE MiXeD(a,b,c); INSERT INTO mixed VALUES(1,2,3,4);", "table mixed has 3 columns but 4 values were supplied"),
        c(Msg::ColumnCountMismatch, vec![name("MIXED"), count(3), count(1)], "CREATE TABLE MiXeD(a,b,c); INSERT INTO MIXED VALUES(1);", "table MIXED has 3 columns but 1 values were supplied"),
        c(Msg::ColumnCountMismatch, vec![name("t1"), count(2), count(1)], "CREATE TABLE t1(a,b); INSERT INTO t1 VALUES(1);", "table t1 has 2 columns but 1 values were supplied"),
        // A statement that named its own column list gets a different message.
        c(Msg::ValuesForColumnsCount, vec![count(2), count(1)], "CREATE TABLE t1(a,b); INSERT INTO t1(a) VALUES(1,2);", "2 values for 1 columns"),
        c(Msg::ValuesForColumnsCount, vec![count(1), count(2)], "CREATE TABLE t1(a,b); INSERT INTO t1(a,b) VALUES(1);", "1 values for 2 columns"),
        c(Msg::TableExists, vec![name("MiXeD")], "CREATE TABLE MiXeD(A); CREATE TABLE MiXeD(B);", "table MiXeD already exists"),
        c(Msg::TableExists, vec![name("mixed")], "CREATE TABLE MiXeD(A); CREATE TABLE mixed(B);", "table mixed already exists"),
        c(Msg::TableExists, vec![name("t1")], "CREATE TABLE t1(a); CREATE TABLE t1(b);", "table t1 already exists"),
        c(Msg::IndexExists, vec![name("I")], "CREATE TABLE t(a); CREATE INDEX i ON t(a); CREATE INDEX I ON t(a);", "index I already exists"),
        c(Msg::IndexExists, vec![name("i1")], "CREATE TABLE t(a); CREATE INDEX i1 ON t(a); CREATE INDEX i1 ON t(a);", "index i1 already exists"),
        // The two name-clash messages are not interchangeable: the object that
        // already exists is the one that gets named. An index called `i` and a
        // table called `i` cannot coexist, and which message comes back depends
        // on which one the schema got first.
        c(Msg::TableNamedExists, vec![name("i")], "CREATE TABLE i(b); CREATE TABLE t(a); CREATE INDEX i ON t(a);", "there is already a table named i"),
        c(Msg::IndexNamedExists, vec![name("i")], "CREATE TABLE t(a); CREATE INDEX i ON t(a); CREATE TABLE i(b);", "there is already an index named i"),
        c(Msg::DuplicateColumnName, vec![name("B")], "CREATE TABLE MiXeD(a,b,B);", "duplicate column name: B"),
        c(Msg::DuplicateColumnName, vec![name("XyZzY")], "CREATE TABLE t(XyZzY, XyZzY);", "duplicate column name: XyZzY"),
        // The name compared is the schema's, so `A` and `a` are one name, and
        // the name printed is the *later* of the two -- the one that collided,
        // not the one already in the table. `CREATE TABLE t1(a,A)` says `A`
        // and `CREATE TABLE t1(A,a)` says `a`, which is the only thing that
        // tells the two apart.
        c(Msg::DuplicateColumnName, vec![name("A")], "CREATE TABLE t1(a,A);", "duplicate column name: A"),
        c(Msg::DuplicateColumnName, vec![name("a")], "CREATE TABLE t1(A,a);", "duplicate column name: a"),
        c(Msg::DuplicateColumnName, vec![name("A")], "CREATE TABLE t1(\"a\",A);", "duplicate column name: A"),
        c(Msg::DuplicateColumnName, vec![name("a")], "CREATE TABLE t1(a,b,a);", "duplicate column name: a"),
        // --- aggregates ------------------------------------------------------
        c(Msg::MisuseOfAggregate, vec![name("count")], "CREATE TABLE t(a); SELECT count(a) FROM t WHERE count(a);", "misuse of aggregate: count()"),
        c(Msg::MisuseOfAggregate, vec![name("COUNT")], "CREATE TABLE t(a); SELECT COUNT(a) FROM t WHERE COUNT(a);", "misuse of aggregate: COUNT()"),
        c(Msg::MisuseOfAggregate, vec![name("sum")], "CREATE TABLE t(a); SELECT sum(a) FROM t WHERE sum(a);", "misuse of aggregate: sum()"),
        c(Msg::MisuseOfAggregate, vec![name("AVG")], "CREATE TABLE t(a); SELECT AVG(a) FROM t WHERE AVG(a);", "misuse of aggregate: AVG()"),
        // The occurrence that is echoed is the one in the WHERE clause, not the
        // one in the select list.
        c(Msg::MisuseOfAggregate, vec![name("count")], "CREATE TABLE t(a); SELECT COUNT(a) FROM t WHERE count(a);", "misuse of aggregate: count()"),
        c(Msg::MisuseOfAggregate, vec![name("COUNT")], "CREATE TABLE t(a); SELECT count(a) FROM t WHERE COUNT(a);", "misuse of aggregate: COUNT()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("count")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE count(a);", "misuse of aggregate function count()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("COUNT")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE COUNT(a);", "misuse of aggregate function COUNT()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("min")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE min(a);", "misuse of aggregate function min()"),
        c(Msg::MisuseOfAggregateFunction, vec![name("max")], "CREATE TABLE t(a); SELECT 1 FROM t WHERE max(a);", "misuse of aggregate function max()"),
        c(Msg::MisuseOfAliasedAggregate, vec![name("m")], "CREATE TABLE t(a); SELECT min(a) AS m FROM t GROUP BY a HAVING max(m)<1;", "misuse of aliased aggregate m"),
        c(Msg::MisuseOfAliasedAggregate, vec![name("M")], "CREATE TABLE t(a); SELECT min(a) AS M FROM t GROUP BY a HAVING max(M)<1;", "misuse of aliased aggregate M"),
        c(Msg::MisuseOfAliasedAggregate, vec![name("cn")], "CREATE TABLE t(a); SELECT min(a) AS cn FROM t GROUP BY a HAVING max(cn)<1;", "misuse of aliased aggregate cn"),
        c(Msg::AggregateNotAllowedInGroupBy, vec![], "CREATE TABLE t(a); SELECT count(*) FROM t GROUP BY count(a);", "aggregate functions are not allowed in the GROUP BY clause"),
        c(Msg::HavingOnNonAggregate, vec![], "CREATE TABLE t(a); SELECT a FROM t HAVING count(a)>1;", "HAVING clause on a non-aggregate query"),
        c(Msg::IntegerOverflow, vec![], "SELECT abs(-9223372036854775808);", "integer overflow"),
        // --- ordinals --------------------------------------------------------
        // The upper bound is the width of the result set, never the width of
        // the table, and both directions are quoted because that is the part
        // worth pinning.
        c(Msg::OrderByTermOutOfRange, vec![count(1), count(1)], "SELECT 1 ORDER BY 2;", "1st ORDER BY term out of range - should be between 1 and 1"),
        c(Msg::OrderByTermOutOfRange, vec![count(1), count(1)], "CREATE TABLE t(a,b); SELECT a AS z FROM t ORDER BY 3;", "1st ORDER BY term out of range - should be between 1 and 1"),
        c(Msg::OrderByTermOutOfRange, vec![count(2), count(1)], "CREATE TABLE t(a,b); SELECT a AS z FROM t ORDER BY 1,3;", "2nd ORDER BY term out of range - should be between 1 and 1"),
        c(Msg::OrderByTermOutOfRange, vec![count(1), count(2)], "CREATE TABLE t(a,b); SELECT a,b AS z FROM t ORDER BY 3;", "1st ORDER BY term out of range - should be between 1 and 2"),
        c(Msg::OrderByTermOutOfRange, vec![count(1), count(2)], "CREATE TABLE t5(a,b); SELECT * FROM t5 ORDER BY 3;", "1st ORDER BY term out of range - should be between 1 and 2"),
        // A compound has no FROM for an expression key to read, so a term that
        // is not a result column's name has nothing the result set can answer
        // for. Measured: even a name that is not a result column at all gets
        // the same wording, naming the term by its ordinal.
        c(Msg::OrderByTermNoMatch, vec![count(1)], "SELECT 1 AS x UNION SELECT 2 AS y ORDER BY y+1;", "1st ORDER BY term does not match any column in the result set"),
        c(Msg::GroupByTermOutOfRange, vec![count(1), count(1)], "SELECT 1 GROUP BY 2;", "1st GROUP BY term out of range - should be between 1 and 1"),
        c(Msg::GroupByTermOutOfRange, vec![count(1), count(1)], "CREATE TABLE t(a,b); SELECT a FROM t GROUP BY 4;", "1st GROUP BY term out of range - should be between 1 and 1"),
        c(Msg::GroupByTermOutOfRange, vec![count(1), count(2)], "CREATE TABLE t(a,b); SELECT a,b FROM t GROUP BY 4;", "1st GROUP BY term out of range - should be between 1 and 2"),
        c(Msg::GroupByTermOutOfRange, vec![count(1), count(3)], "CREATE TABLE t(a,b,c); SELECT a,b,c FROM t GROUP BY 4;", "1st GROUP BY term out of range - should be between 1 and 3"),
        c(Msg::GroupByTermOutOfRange, vec![count(1), count(2)], "CREATE TABLE t(a,b,c); SELECT a,b FROM t GROUP BY 4;", "1st GROUP BY term out of range - should be between 1 and 2"),
        c(Msg::GroupByTermOutOfRange, vec![count(2), count(2)], "CREATE TABLE t(a,b,c); SELECT a,b FROM t GROUP BY 1,4;", "2nd GROUP BY term out of range - should be between 1 and 2"),
        // --- compound and sub-select ------------------------------------------
        c(Msg::SubSelectColumnCount, vec![count(2)], "SELECT (SELECT 1,2);", "sub-select returns 2 columns - expected 1"),
        c(Msg::SubSelectColumnCount, vec![count(3)], "CREATE TABLE t(a); SELECT 1 FROM t WHERE (SELECT 1,2,3);", "sub-select returns 3 columns - expected 1"),
        c(Msg::SubSelectColumnCount, vec![count(2)], "SELECT 1 IN (SELECT 1,2);", "sub-select returns 2 columns - expected 1"),
        c(Msg::CompoundColumnCount, vec![name("UNION")], "SELECT 1 UNION SELECT 1,2;", "SELECTs to the left and right of UNION do not have the same number of result columns"),
        c(Msg::CompoundColumnCount, vec![name("UNION ALL")], "SELECT 1 UNION ALL SELECT 1,2;", "SELECTs to the left and right of UNION ALL do not have the same number of result columns"),
        c(Msg::CompoundColumnCount, vec![name("INTERSECT")], "SELECT 1 INTERSECT SELECT 1,2;", "SELECTs to the left and right of INTERSECT do not have the same number of result columns"),
        c(Msg::CompoundColumnCount, vec![name("EXCEPT")], "SELECT 1 EXCEPT SELECT 1,2;", "SELECTs to the left and right of EXCEPT do not have the same number of result columns"),
        // --- the syntax error family ------------------------------------------
        c(Msg::SyntaxError, vec![name("FROM")], "SELECT FROM t;", "near \"FROM\": syntax error"),
        c(Msg::SyntaxError, vec![name("from")], "SELECT from t;", "near \"from\": syntax error"),
        c(Msg::SyntaxError, vec![name("SELEC")], "SELEC 1;", "near \"SELEC\": syntax error"),
        c(Msg::SyntaxError, vec![name(";")], "SELECT * FROM;", "near \";\": syntax error"),
        c(Msg::SyntaxError, vec![name("t3")], "SELECT * FROM t t2 t3;", "near \"t3\": syntax error"),
        c(Msg::SyntaxError, vec![name(")")], "CREATE TABLE t1();", "near \")\": syntax error"),
        c(Msg::SyntaxError, vec![name("XYZZY")], "SELECT 1 WHERE a XYZZY;", "near \"XYZZY\": syntax error"),
        // A CASE with no WHEN is the same syntax error as anything else, and
        // names the token that stopped the parse.
        c(Msg::SyntaxError, vec![name(";")], "SELECT CASE END;", "near \";\": syntax error"),
        c(Msg::SyntaxError, vec![name(";")], "SELECT CASE WHEN 1 THEN 2;", "near \";\": syntax error"),
        c(Msg::SyntaxError, vec![name(")")], "SELECT )1(", "near \")\": syntax error"),
        c(Msg::IncompleteInput, vec![], "CREATE TABLE t(a); SELECT 1 FROM t WHERE", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT 1 ORDER BY", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT 1 GROUP BY", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT a FROM", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT (1", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT 1,", "incomplete input"),
        c(Msg::IncompleteInput, vec![], "SELECT 1 IN", "incomplete input"),
        // --- ALTER TABLE ... ADD COLUMN -------------------------------------
        // The four refusals, each with the exact text sqlite3 3.53.4 printed.
        // None of them names a column, which is what makes them zero-argument
        // messages rather than a new name hole.
        //
        // The NOT NULL one is a runtime decision, not a parse refusal: the same
        // statement against an *empty* table is accepted, so the case below
        // needs a row in the table for the answer to be the message.
        c(Msg::CannotAddPrimaryKeyColumn, vec![], "CREATE TABLE t(a,b); ALTER TABLE t ADD COLUMN c PRIMARY KEY;", "Cannot add a PRIMARY KEY column"),
        c(Msg::CannotAddUniqueColumn, vec![], "CREATE TABLE t(a,b); ALTER TABLE t ADD COLUMN c UNIQUE;", "Cannot add a UNIQUE column"),
        c(Msg::CannotAddNotNullColumn, vec![], "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); ALTER TABLE t ADD COLUMN c NOT NULL;", "Cannot add a NOT NULL column with default value NULL"),
        // The non-constant one is a runtime decision for the same reason: the
        // ALTER needs a row in the table before SQLite will raise it, and the
        // empty-table form of the very same statement is accepted.
        c(Msg::CannotAddNonConstantDefault, vec![], "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); ALTER TABLE t ADD COLUMN c DEFAULT (1+2);", "Cannot add a column with non-constant default"),
        c(Msg::CannotAddNonConstantDefault, vec![], "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); ALTER TABLE t ADD COLUMN c DEFAULT CURRENT_TIMESTAMP;", "Cannot add a column with non-constant default"),
        // This one names the column, and it is a *parse* refusal: it is raised
        // against an empty table too, because it is about the spelling rather
        // than about anything the table holds.
        c(Msg::DefaultValueNotConstant, vec![name("c")], "CREATE TABLE t(a,b); ALTER TABLE t ADD COLUMN c DEFAULT (a);", "default value of column [c] is not constant"),
        c(Msg::UnrecognizedToken, vec![name("\"abc")], "SELECT \"abc", "unrecognized token: \"\"abc\""),
        c(Msg::UnrecognizedToken, vec![name("'abc")], "SELECT 'abc", "unrecognized token: \"'abc\""),
        c(Msg::UnrecognizedToken, vec![name("`abc")], "SELECT `abc", "unrecognized token: \"`abc\""),
        c(Msg::UnrecognizedToken, vec![name("[abc")], "SELECT [abc", "unrecognized token: \"[abc\""),
        c(Msg::UnrecognizedToken, vec![name("x'abc")], "SELECT x'abc", "unrecognized token: \"x'abc\""),
        c(Msg::UnrecognizedToken, vec![name("X'abc")], "SELECT X'abc", "unrecognized token: \"X'abc\""),
        // SQLITE_MAX_COMPOUND_SELECT is 500 in the build under test: 500 UNIONed
        // arms parse and 501 do not. The statement is the one row that cannot be
        // written out inline -- it is 501 arms and 9,508 characters -- so it is
        // given as a recipe and re-derived with:
        //
        //     sqlite3 -batch :memory: "SELECT 1$(printf ' UNION ALL SELECT 1%.0s' $(seq 1 500))"
        //
        // which prints `too many terms in compound SELECT`. With 499 instead of
        // 500 repetitions it prints `1` and succeeds, so 500 really is the bound.
        c(Msg::TooManyCompoundTerms, vec![], COMPOUND_501, "too many terms in compound SELECT"),
        // --- the engine's own gaps ---------------------------------------------
        // These have no oracle, and say so: they describe this engine's
        // missing features, not SQLite's wording.
        c(Msg::UnsupportedYet, vec![name("Union")], "nsqlite only: SELECT 1 UNION SELECT 1", "Union is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("DISTINCT")], "nsqlite only: SELECT DISTINCT 1", "DISTINCT is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("WITHOUT ROWID")], "nsqlite only: CREATE TABLE t(a) WITHOUT ROWID", "WITHOUT ROWID is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("INSERT ... SELECT")], "nsqlite only: INSERT INTO a SELECT x FROM b", "INSERT ... SELECT is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("CREATE INDEX")], "nsqlite only: CREATE INDEX i ON t(a)", "CREATE INDEX is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("JSON")], "nsqlite only: SELECT json('{}')", "JSON is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("binding parameter i")], "nsqlite only: SELECT ?", "binding parameter i is not supported yet"),
        c(Msg::UnsupportedYet, vec![name("a compound select")], "nsqlite only: SELECT 1 UNION SELECT 2", "a compound select is not supported yet"),
        // The engine raises the subquery-in-FROM gap without a reason prefix,
        // so the unprefixed form is the one that is pinned here.
        c(Msg::UnsupportedYet, vec![name("a subquery in FROM")], "nsqlite only: SELECT * FROM (SELECT 1)", "a subquery in FROM is not supported yet"),
        // --- the reasoned form, for a gap that has a reason to give ----------
        c(Msg::UnsupportedYetIn, vec![name("RETURNING"), name("INSERT ... RETURNING")], "nsqlite only: INSERT INTO t VALUES(1) RETURNING a", "RETURNING: INSERT ... RETURNING is not supported yet"),
        c(Msg::UnsupportedYetIn, vec![name("the sub-select names a table this engine cannot yet read"), name("a correlated sub-select")], "nsqlite only: SELECT (SELECT a FROM t LIMIT 1) FROM t", "the sub-select names a table this engine cannot yet read: a correlated sub-select is not supported yet"),
        ];
        cases
    }

    /// The indices of [`build_cases`]'s rows, as a macro can walk them.
    ///
    /// Written out rather than generated so that adding a row to the table
    /// means adding its index here, and forgetting to do so is a compile error
    /// rather than a test that quietly stops covering the new row.
    macro_rules! case_indices {
        ($m:ident) => {
            $m! {
                n0 = 0, n1 = 1, n2 = 2, n3 = 3,
                n4 = 4, n5 = 5, n6 = 6, n7 = 7,
                n8 = 8, n9 = 9, n10 = 10, n11 = 11,
                n12 = 12, n13 = 13, n14 = 14, n15 = 15,
                n16 = 16, n17 = 17, n18 = 18, n19 = 19,
                n20 = 20, n21 = 21, n22 = 22, n23 = 23,
                n24 = 24, n25 = 25, n26 = 26, n27 = 27,
                n28 = 28, n29 = 29, n30 = 30, n31 = 31,
                n32 = 32, n33 = 33, n34 = 34, n35 = 35,
                n36 = 36, n37 = 37, n38 = 38, n39 = 39,
                n40 = 40, n41 = 41, n42 = 42, n43 = 43,
                n44 = 44, n45 = 45, n46 = 46, n47 = 47,
                n48 = 48, n49 = 49, n50 = 50, n51 = 51,
                n52 = 52, n53 = 53, n54 = 54, n55 = 55,
                n56 = 56, n57 = 57, n58 = 58, n59 = 59,
                n60 = 60, n61 = 61, n62 = 62, n63 = 63,
                n64 = 64, n65 = 65, n66 = 66, n67 = 67,
                n68 = 68, n69 = 69, n70 = 70, n71 = 71,
                n72 = 72, n73 = 73, n74 = 74, n75 = 75,
                n76 = 76, n77 = 77, n78 = 78, n79 = 79,
                n80 = 80, n81 = 81, n82 = 82, n83 = 83,
                n84 = 84, n85 = 85, n86 = 86, n87 = 87,
                n88 = 88, n89 = 89, n90 = 90, n91 = 91,
                n92 = 92, n93 = 93, n94 = 94, n95 = 95,
                n96 = 96, n97 = 97, n98 = 98, n99 = 99,
                n100 = 100, n101 = 101, n102 = 102, n103 = 103,
                n104 = 104, n105 = 105, n106 = 106, n107 = 107,
                n108 = 108, n109 = 109, n110 = 110, n111 = 111,
                n112 = 112, n113 = 113, n114 = 114, n115 = 115,
                n116 = 116, n117 = 117, n118 = 118, n119 = 119,
                n120 = 120, n121 = 121, n122 = 122, n123 = 123,
                n124 = 124, n125 = 125, n126 = 126, n127 = 127,
                n128 = 128, n129 = 129, n130 = 130, n131 = 131,
                n132 = 132, n133 = 133, n134 = 134, n135 = 135,
                n136 = 136, n137 = 137, n138 = 138, n139 = 139,
                n140 = 140, n141 = 141, n142 = 142, n143 = 143,
                n144 = 144, n145 = 145, n146 = 146, n147 = 147,
                n148 = 148, n149 = 149, n150 = 150, n151 = 151,
                n152 = 152, n153 = 153, n154 = 154, n155 = 155,
                n156 = 156, n157 = 157, n158 = 158, n159 = 159,
                n160 = 160, n161 = 161, n162 = 162, n163 = 163,
                n164 = 164, n165 = 165, n166 = 166, n167 = 167,
                n168 = 168, n169 = 169, n170 = 170, n171 = 171,
                n172 = 172, n173 = 173, n174 = 174, n175 = 175,
                n176 = 176, n177 = 177, n178 = 178, n179 = 179,
                n180 = 180, n181 = 181, n182 = 182,
                n183 = 183, n184 = 184, n185 = 185, n186 = 186,
                n187 = 187, n188 = 188, n189 = 189, n190 = 190,
                n191 = 191, n192 = 192, n193 = 193, n194 = 194,
                n195 = 195, n196 = 196, n197 = 197, n198 = 198,
                n199 = 199, n200 = 200, n201 = 201,
                n202 = 202, n203 = 203, n204 = 204,
            }
        };
    }

    /// The number of rows [`build_cases`] builds, and the number of generated
    /// tests there are. They have to be equal: one test per row, each reaching
    /// its own row and no other.
    const CASE_COUNT: usize = 205;

    /// The case at `index`, or a panic that says the table and the index
    /// disagree.
    ///
    /// The index comes from the test name, which is written next to the case in
    /// the table, so a test can only ever reach its own row. Two cases that
    /// render the same text are therefore still two independent tests, which is
    /// what the table's duplicate renderings are there to demonstrate.
    fn case_at(index: usize) -> &'static Case {
        let table = cases();
        table
            .get(index)
            .unwrap_or_else(|| panic!("the case table has {index} rows, not {}", index + 1))
    }

    /// One `#[test]` per row of the table above, generated from the indices.
    ///
    /// Each asserts the rendered message against the literal the oracle
    /// produced, so a change to any message is a visible test failure rather
    /// than a silent disagreement with the suite. The tests are generated by
    /// index rather than by `(msg, text)` on purpose: a key on the pair would
    /// collapse the rows that legitimately render the same text into one, and
    /// the later-named test would quietly re-run the earlier one.
    macro_rules! oracle_tests {
        ($($n:ident = $i:literal),* $(,)?) => {
            $(
                #[test]
                #[doc = concat!("renders case ", stringify!($n))]
                fn $n() {
                    let case = case_at($i);
                    assert_eq!(
                        case.msg.render(&case.args),
                        case.text,
                        "{ORACLE} says {:?} for `{}`, and the catalogue says something else",
                        case.text,
                        case.sql
                    );
                }
            )*
        };
    }

    case_indices!(oracle_tests);

    /// The generated tests are one-to-one with the table.
    ///
    /// A test is generated for every row and indexes exactly that row, so a
    /// test can only re-run an earlier row if two rows are literally the same
    /// entry. The table deliberately contains rows that render the same text --
    /// `SELECT * FROM Foo`, `SELECT * FROM [Foo]`, `SELECT * FROM "Foo"` and
    /// `SELECT * FROM `Foo`` all say `no such table: Foo` -- and each still has
    /// its own statement recorded, so the transcript is the evidence and the
    /// test is the check. That is the defect this replaces: a lookup keyed on
    /// `(msg, text)` returned the first of those four for all four, and the
    /// three later-named tests asserted the first one instead of their own.
    #[test]
    fn a_case_and_its_test_are_one_to_one() {
        let table = cases();
        // Every index the generator walks exists, and the table has no row the
        // generator missed.
        assert_eq!(
            CASE_COUNT,
            table.len(),
            "the case table has {} rows but the generator walks {CASE_COUNT}",
            table.len()
        );
        for i in 0..CASE_COUNT {
            let case = case_at(i);
            // Each row must have come out of the table at its own index.
            assert!(
                core::ptr::eq(case, &table[i]),
                "row {i} is not at its own index"
            );
            assert!(
                !case.sql.is_empty() && case.sql != "an input with nothing in it",
                "row {i} ({:?}) has no statement to re-derive it from",
                case.msg
            );
            // And the row must render to the text it claims, which is what
            // the generated test at this index asserts.
            assert_eq!(
                case.msg.render(&case.args),
                case.text,
                "row {i} does not render its own text"
            );
        }
        // And the table must have at least one row per message.
        for msg in Msg::ALL {
            assert!(
                table.iter().any(|c| c.msg == *msg),
                "{msg:?} is in Msg::ALL but no test covers it"
            );
        }
    }

    /// Every fixed message, walked and asserted against a literal.
    ///
    /// This is the test that makes a change to a message a visible failure
    /// rather than a silent disagreement with the suite: the list is walked by
    /// value, so a message added to [`Msg::FIXED`] without saying what it says
    /// fails here, and changing a message's wording fails the entry that quotes
    /// the old wording.
    #[test]
    fn a_fixed_message_says_what_sqlite3_says() {
        let expected: &[(Msg, &str)] = &[
            (Msg::ForeignKeyConstraint, "FOREIGN KEY constraint failed"),
            (Msg::DatatypeMismatch, "datatype mismatch"),
            (
                Msg::AggregateNotAllowedInGroupBy,
                "aggregate functions are not allowed in the GROUP BY clause",
            ),
            (
                Msg::HavingOnNonAggregate,
                "HAVING clause on a non-aggregate query",
            ),
            (Msg::IntegerOverflow, "integer overflow"),
            (
                Msg::TooManyCompoundTerms,
                "too many terms in compound SELECT",
            ),
            (Msg::IncompleteInput, "incomplete input"),
        ];
        assert_eq!(
            expected.len(),
            Msg::FIXED.len(),
            "Msg::FIXED and this list disagree about how many fixed messages there are"
        );
        for (msg, text) in expected {
            assert!(
                Msg::FIXED.contains(msg),
                "{msg:?} is quoted by the walk but is not in Msg::FIXED"
            );
            assert_eq!(msg.render(&[]), *text, "{msg:?} does not match {ORACLE}");
        }
    }

    /// Every message in the catalogue, asserted against a literal.
    ///
    /// This is the test that makes a change to a message a visible failure
    /// rather than a silent disagreement with the suite. It walks
    /// [`Msg::ALL`] -- not [`Msg::FIXED`] -- and asserts each message twice:
    ///
    /// * against a literal that is its *fixed shape*: the message a caller gets
    ///   from [`Msg::render`] with the holes filled by [`name`], [`count`] and
    ///   [`value`]. Every message has one, and it is the whole sentence rather
    ///   than one row of a table, so a wording change fails here whatever the
    ///   holes are.
    /// * against the oracle rows for that message, which are the same sentence
    ///   seen on statements the real sqlite3 was run on.
    ///
    /// The first is the walk the module promises; the second is what the module
    /// is *for*. Both are keyed on the enum, so a message added without a shape
    /// or without a row fails the test that names it.
    #[test]
    fn every_message_says_what_sqlite3_says() {
        for msg in Msg::ALL {
            assert_eq!(
                msg.render(&fixed_shape(*msg)),
                shape_text(*msg),
                "{msg:?} does not say what {ORACLE} says"
            );
            assert!(
                cases().iter().any(|c| c.msg == *msg),
                "{msg:?} is in Msg::ALL but no oracle row covers it"
            );
        }
    }

    /// The holes a message's fixed shape is filled with.
    ///
    /// One per hole, in argument order, so a message with three names gets
    /// three names. The names are distinctive enough that a render arm taking
    /// them in the wrong order is a visible difference rather than the same
    /// word twice, and the counts are the smallest numbers that are not zero,
    /// so a swapped count shows up too.
    fn fixed_shape(msg: Msg) -> Vec<Arg> {
        let one = || vec![name("Table")];
        let two = || vec![name("Table"), name("Column")];
        let three = || vec![name("Schema"), name("Table"), name("Column")];
        match msg {
            Msg::NoSuchTable
            | Msg::NoSuchColumn
            | Msg::NoSuchColumnDoubleQuoted
            | Msg::AmbiguousColumn
            | Msg::NoSuchFunction
            | Msg::NoSuchCollation
            | Msg::NoSuchIndex
            | Msg::WrongArgumentCount
            | Msg::MisuseOfAggregate
            | Msg::MisuseOfAggregateFunction
            | Msg::MisuseOfAliasedAggregate
            | Msg::TableExists
            | Msg::IndexExists
            | Msg::TableNamedExists
            | Msg::IndexNamedExists
            | Msg::DuplicateColumnName
            | Msg::SyntaxError
            | Msg::UnrecognizedToken
            | Msg::UniqueConstraintRowid
            | Msg::CheckConstraint => one(),
            Msg::NoSuchTableSchemaQualified
            | Msg::NoSuchColumnQualified
            | Msg::NoSuchColumnForTable
            | Msg::NotNullConstraint
            | Msg::UniqueConstraint
            | Msg::ForeignKeyMismatch => two(),
            Msg::NoSuchColumnSchemaQualified | Msg::AmbiguousColumnStar => three(),
            Msg::ColumnCountMismatch => vec![name("Table"), count(2), count(3)],
            Msg::ValuesForColumnsCount => vec![count(2), count(3)],
            Msg::OrderByTermOutOfRange | Msg::GroupByTermOutOfRange => vec![count(1), count(2)],
            // One count: the offending term, named by its ordinal.
            Msg::OrderByTermNoMatch => vec![count(1)],
            Msg::SubSelectColumnCount => vec![count(2)],
            Msg::CompoundColumnCount => vec![name("UNION")],
            Msg::UnsupportedYet => vec![name("Widget")],
            Msg::UnsupportedYetIn => vec![name("Reason"), name("Widget")],
            Msg::ForeignKeyConstraint
            | Msg::DatatypeMismatch
            | Msg::AggregateNotAllowedInGroupBy
            | Msg::HavingOnNonAggregate
            | Msg::IntegerOverflow
            | Msg::TooManyCompoundTerms
            | Msg::IncompleteInput
            | Msg::CannotAddPrimaryKeyColumn
            | Msg::CannotAddUniqueColumn
            | Msg::CannotAddColumnToView
            | Msg::CannotAddNotNullColumn
            | Msg::CannotAddNonConstantDefault => Vec::new(),
            Msg::DefaultValueNotConstant => vec![name("c")],
        }
    }

    /// What [`fixed_shape`] renders to, written out in full.
    ///
    /// The literal the walk above asserts against. It is written here rather
    /// than derived from the render arms, so a change to [`Msg::render`] and a
    /// change to this list are two changes and the one that is wrong fails.
    fn shape_text(msg: Msg) -> String {
        match msg {
            Msg::NoSuchTable => "no such table: Table".to_string(),
            Msg::NoSuchTableSchemaQualified => "no such table: Table.Column".to_string(),
            Msg::NoSuchColumn => "no such column: Table".to_string(),
            Msg::NoSuchColumnQualified => "no such column: Table.Column".to_string(),
            Msg::NoSuchColumnSchemaQualified => {
                "no such column: Schema.Table.Column".to_string()
            }
            Msg::NoSuchColumnDoubleQuoted => {
                "no such column: \"Table\" - should this be a string literal in single-quotes?"
                    .to_string()
            }
            Msg::AmbiguousColumn => "ambiguous column name: Table".to_string(),
            Msg::AmbiguousColumnStar => {
                "ambiguous column name: Schema.Table.Column".to_string()
            }
            Msg::NoSuchFunction => "no such function: Table".to_string(),
            Msg::NoSuchCollation => "no such collation sequence: Table".to_string(),
            Msg::NoSuchIndex => "no such index: Table".to_string(),
            Msg::WrongArgumentCount => {
                "wrong number of arguments to function Table()".to_string()
            }
            Msg::NotNullConstraint => "NOT NULL constraint failed: Table.Column".to_string(),
            Msg::UniqueConstraint => "UNIQUE constraint failed: Table.Column".to_string(),
            Msg::UniqueConstraintRowid => "UNIQUE constraint failed: Table.rowid".to_string(),
            Msg::CheckConstraint => "CHECK constraint failed: Table".to_string(),
            Msg::ForeignKeyConstraint => "FOREIGN KEY constraint failed".to_string(),
            Msg::ForeignKeyMismatch => {
                "foreign key mismatch - \"Table\" referencing \"Column\"".to_string()
            }
            Msg::DatatypeMismatch => "datatype mismatch".to_string(),
            Msg::NoSuchColumnForTable => {
                "table Table has no column named Column".to_string()
            }
            Msg::ColumnCountMismatch => {
                "table Table has 2 columns but 3 values were supplied".to_string()
            }
            Msg::ValuesForColumnsCount => "2 values for 3 columns".to_string(),
            Msg::TableExists => "table Table already exists".to_string(),
            Msg::IndexExists => "index Table already exists".to_string(),
            Msg::TableNamedExists => "there is already a table named Table".to_string(),
            Msg::IndexNamedExists => "there is already an index named Table".to_string(),
            Msg::DuplicateColumnName => "duplicate column name: Table".to_string(),
            Msg::MisuseOfAggregate => "misuse of aggregate: Table()".to_string(),
            Msg::MisuseOfAggregateFunction => {
                "misuse of aggregate function Table()".to_string()
            }
            Msg::MisuseOfAliasedAggregate => "misuse of aliased aggregate Table".to_string(),
            Msg::AggregateNotAllowedInGroupBy => {
                "aggregate functions are not allowed in the GROUP BY clause".to_string()
            }
            Msg::HavingOnNonAggregate => {
                "HAVING clause on a non-aggregate query".to_string()
            }
            Msg::IntegerOverflow => "integer overflow".to_string(),
            Msg::OrderByTermOutOfRange => {
                "1st ORDER BY term out of range - should be between 1 and 2".to_string()
            }
            Msg::GroupByTermOutOfRange => {
                "1st GROUP BY term out of range - should be between 1 and 2".to_string()
            }
            Msg::OrderByTermNoMatch => {
                "1st ORDER BY term does not match any column in the result set".to_string()
            }
            Msg::SubSelectColumnCount => "sub-select returns 2 columns - expected 1".to_string(),
            Msg::CompoundColumnCount => {
                "SELECTs to the left and right of UNION do not have the same number of result columns"
                    .to_string()
            }
            Msg::TooManyCompoundTerms => "too many terms in compound SELECT".to_string(),
            Msg::SyntaxError => "near \"Table\": syntax error".to_string(),
            Msg::IncompleteInput => "incomplete input".to_string(),
            Msg::CannotAddPrimaryKeyColumn => "Cannot add a PRIMARY KEY column".to_string(),
            Msg::CannotAddUniqueColumn => "Cannot add a UNIQUE column".to_string(),
            Msg::CannotAddColumnToView => "Cannot add a column to a view".to_string(),
            Msg::CannotAddNotNullColumn => {
                "Cannot add a NOT NULL column with default value NULL".to_string()
            }
            Msg::CannotAddNonConstantDefault => {
                "Cannot add a column with non-constant default".to_string()
            }
            Msg::DefaultValueNotConstant => {
                "default value of column [c] is not constant".to_string()
            }
            Msg::UnrecognizedToken => "unrecognized token: \"Table\"".to_string(),
            Msg::UnsupportedYet => "Widget is not supported yet".to_string(),
            Msg::UnsupportedYetIn => "Reason: Widget is not supported yet".to_string(),
        }
    }

    /// Every message with a hole, rendered and asserted against a literal.
    ///
    /// This is the complement of the fixed walk: a message with a hole has no
    /// single wording, so each shape is checked here against what the oracle
    /// said for the statement that produces it. It reads the same table the
    /// per-message tests read, so a case added for one is automatically covered
    /// by the other.
    #[test]
    fn a_message_with_holes_renders_as_sqlite3_renders_it() {
        for case in cases() {
            if !case.msg.has_holes() {
                continue;
            }
            assert_eq!(
                case.msg.render(&case.args),
                case.text,
                "{ORACLE} says {:?} for `{}`",
                case.text,
                case.sql
            );
        }
    }

    /// Every message in the catalogue is covered by a test.
    ///
    /// A message added to [`Msg::ALL`] without a case has no `oracle_tests!`
    /// line, so this fails and says which one.
    #[test]
    fn every_message_has_a_test() {
        for msg in Msg::ALL {
            assert!(
                cases().iter().any(|c| c.msg == *msg),
                "{msg:?} is in Msg::ALL but no test covers it"
            );
        }
    }

    /// Every oracle row is a runnable statement, and the ones that claim to be
    /// oracle output are replayable rather than described.
    ///
    /// The `sql` field is the only thing tying an expectation to sqlite3, so
    /// leaving it inert is the hole four invented messages walked through. A row
    /// that is not a statement cannot be re-derived, so it is refused here:
    ///
    /// * the eleven `nsqlite only:` rows are this engine's own "not supported
    ///   yet" text, which describes a gap in *this* engine and so has no oracle
    ///   to replay against -- they are marked, and this test counts them;
    /// * every other row must be a statement, and must end up in the same shape
    ///   the oracle was seen to produce.
    ///
    /// The companion check is a replay itself, which cannot run in `cargo test`
    /// because it needs the oracle binary. It is run by hand:
    ///
    /// ```text
    /// sqlite3 -batch :memory: "<the row's sql>"
    /// ```
    ///
    /// and its output, with the shell's `Parse error in Nth command line
    /// argument: ` prefix stripped, must equal the row's `text`. The whole
    /// table was replayed that way, and every row matched.
    #[test]
    fn every_oracle_row_is_replayable() {
        let mut nsqlite_only = 0;
        let mut recipe = 0;
        let mut oracle = 0;
        for case in cases() {
            if case.sql.starts_with("nsqlite only:") {
                nsqlite_only += 1;
                assert!(
                    case.msg == Msg::UnsupportedYet || case.msg == Msg::UnsupportedYetIn,
                    "{} claims to be nsqlite's own gap but is a SQLite message",
                    case.sql
                );
                continue;
            }
            // The one row that is a recipe rather than a statement, because the
            // statement is 9,508 characters long.
            if case.sql == COMPOUND_501 {
                recipe += 1;
                assert_eq!(
                    case.msg,
                    Msg::TooManyCompoundTerms,
                    "the compound-arms recipe is only used for that message"
                );
                continue;
            }
            oracle += 1;
            assert!(
                !case.sql.is_empty(),
                "{:?} has an empty `sql`, so nothing to re-derive its text from",
                case.msg
            );
            // A runnable statement starts with a SQL keyword. Prose does not,
            // which is the whole difference: the two rows this replaced said
            // "an input with nothing in it" and "a bound parameter list with
            // two entries", and both are English. The one exception is
            // deliberate -- a row whose subject is a misspelled keyword starts
            // with the misspelling, so those are named rather than pattern-
            // matched around.
            let head = case.sql.split_whitespace().next().unwrap_or("");
            let keyword = head.trim_end_matches(';').to_ascii_uppercase();
            assert!(
                case.msg == Msg::SyntaxError || SQL_KEYWORDS.contains(&keyword.as_str()),
                "row is not a statement: `{}` claims {:?} / {:?}",
                case.sql,
                case.msg,
                case.text
            );
        }
        // Both families are present, so the split is real rather than the whole
        // table having quietly drifted to one side.
        assert!(
            nsqlite_only > 0 && oracle > 0,
            "expected both oracle rows and nsqlite-only rows, got {oracle} and {nsqlite_only}"
        );
        // And exactly one row is a recipe, which is the point of counting them:
        // a second one would be a second unreplayable expectation.
        assert_eq!(
            recipe, 1,
            "expected exactly one recipe row, the 501-arm compound SELECT"
        );
    }

    /// `Msg::ALL` and `Msg::FIXED` agree about which messages are fixed, so
    /// the two lists partition the catalogue rather than overlapping or
    /// leaving a gap.
    #[test]
    fn all_and_fixed_partition_the_catalogue() {
        for msg in Msg::ALL {
            assert_eq!(
                !msg.has_holes(),
                Msg::FIXED.contains(msg),
                "{msg:?} disagrees about whether it is fixed"
            );
        }
        for msg in Msg::FIXED {
            assert!(
                Msg::ALL.contains(msg),
                "{msg:?} is in Msg::FIXED but not in Msg::ALL"
            );
        }
    }

    /// `Msg::ALL` is the whole catalogue, with no repeats.
    #[test]
    fn all_lists_every_message_once() {
        for (i, msg) in Msg::ALL.iter().enumerate() {
            assert!(!Msg::ALL[..i].contains(msg), "{msg:?} is in Msg::ALL twice");
        }
    }

    /// The case rule is checkable, and the two halves of it disagree.
    ///
    /// This is the module's central claim: a name-resolution message echoes the
    /// query's spelling, and a constraint message echoes the schema's. So the
    /// rule table has to say `Query` for the first and `Schema` for the second,
    /// and each has to hold when the engine is handed a name of the other kind,
    /// because the engine does not transform it on the way in.
    #[test]
    fn the_case_rule_depends_on_where_the_name_came_from() {
        use CaseRule::{Query, Schema};

        // Name resolution: the query's spelling, case kept.
        for n in ["XYZZY", "XyZzY", "MiXeD", "t", "A", "main.t"] {
            assert_eq!(no_such_table(n).message, format!("no such table: {n}"));
            assert_eq!(no_such_column(n).message, format!("no such column: {n}"));
            assert_eq!(
                no_such_function(n).message,
                format!("no such function: {n}")
            );
            assert_eq!(
                wrong_argument_count(n).message,
                format!("wrong number of arguments to function {n}()")
            );
            assert_eq!(
                misuse_of_aggregate_function(n).message,
                format!("misuse of aggregate function {n}()")
            );
            assert_eq!(
                misuse_of_aggregate(n).message,
                format!("misuse of aggregate: {n}()")
            );
            assert_eq!(
                misuse_of_aliased_aggregate(n).message,
                format!("misuse of aliased aggregate {n}")
            );
            assert_eq!(
                ambiguous_column(n).message,
                format!("ambiguous column name: {n}")
            );
            assert_eq!(
                no_such_collation(n).message,
                format!("no such collation sequence: {n}")
            );
            assert_eq!(no_such_index(n).message, format!("no such index: {n}"));
            assert_eq!(
                syntax_error(n).message,
                format!("near \"{n}\": syntax error")
            );
            assert_eq!(
                no_such_column_qualified(n, "C").message,
                format!("no such column: {n}.C")
            );
            assert_eq!(
                Msg::NoSuchColumnSchemaQualified
                    .error(&["s".into(), n.into(), "C".into()])
                    .message,
                format!("no such column: s.{n}.C")
            );
            assert_eq!(
                Msg::NoSuchTableSchemaQualified
                    .error(&["db".into(), n.into()])
                    .message,
                format!("no such table: db.{n}")
            );
            assert_eq!(
                Msg::NoSuchColumnDoubleQuoted.error(&[n.into()]).message,
                format!(
                    "no such column: \"{n}\" - should this be a string literal in single-quotes?"
                )
            );
            assert_eq!(
                Msg::CompoundColumnCount.error(&[n.into()]).message,
                format!("SELECTs to the left and right of {n} do not have the same number of result columns")
            );
            assert_eq!(
                Msg::UnrecognizedToken.error(&[n.into()]).message,
                format!("unrecognized token: \"{n}\"")
            );
        }

        // Constraints: the schema's spelling, whatever the statement said. The
        // engine does not lower anything; the point is that the *caller* has
        // to hand in the schema spelling, and the rule table says so, so a
        // caller who passes the query spelling can be caught.
        for n in ["MiXeD", "Tbl", "tbl", "TBL"] {
            assert_eq!(
                not_null_constraint(n, "Bb").message,
                format!("NOT NULL constraint failed: {n}.Bb")
            );
            assert_eq!(
                unique_constraint(n, "B").message,
                format!("UNIQUE constraint failed: {n}.B")
            );
            assert_eq!(
                unique_constraint_rowid(n).message,
                format!("UNIQUE constraint failed: {n}.rowid")
            );
            assert_eq!(
                check_constraint(n).message,
                format!("CHECK constraint failed: {n}")
            );
            assert_eq!(
                foreign_key_mismatch(n, "P").message,
                format!("foreign key mismatch - \"{n}\" referencing \"P\"")
            );
            assert_eq!(
                Msg::AmbiguousColumnStar
                    .error(&["main".into(), n.into(), "a".into()])
                    .message,
                format!("ambiguous column name: main.{n}.a")
            );
        }

        // The table says which is which, per message, and the messages that
        // name a table in a DML position are query-sourced while the ones
        // raised by the constraint machinery are not.
        for msg in [
            Msg::NoSuchTable,
            Msg::NoSuchColumn,
            Msg::NoSuchFunction,
            Msg::WrongArgumentCount,
            Msg::MisuseOfAggregate,
            Msg::TableExists,
            Msg::IndexExists,
            Msg::IndexNamedExists,
            Msg::DuplicateColumnName,
            Msg::NoSuchColumnForTable,
            Msg::ColumnCountMismatch,
        ] {
            assert!(
                msg.case_rule().iter().all(|r| *r == Query),
                "{msg:?} is query-sourced, and says so"
            );
        }
        for msg in [
            Msg::NotNullConstraint,
            Msg::UniqueConstraint,
            Msg::UniqueConstraintRowid,
            Msg::CheckConstraint,
            Msg::ForeignKeyMismatch,
            Msg::AmbiguousColumnStar,
        ] {
            assert!(
                msg.case_rule().iter().all(|r| *r == Schema),
                "{msg:?} is schema-sourced, and says so"
            );
        }

        // And the counts are not names, so they are not in the table.
        for msg in [
            Msg::IntegerOverflow,
            Msg::DatatypeMismatch,
            Msg::SubSelectColumnCount,
            Msg::GroupByTermOutOfRange,
            Msg::ValuesForColumnsCount,
        ] {
            assert!(
                msg.case_rule().is_empty(),
                "{msg:?} names nothing, and says so"
            );
        }
    }

    /// The schema-sourced and query-sourced rules are genuinely different, so
    /// a rule that treated them alike would fail here.
    ///
    /// With a `Tbl`/`Bb` schema, sqlite3 answers a statement that spells the
    /// table `tbl` with `NOT NULL constraint failed: Tbl.Bb`, and answers a
    /// statement that names a table that is not there at all with
    /// `no such table: Foo` -- the query's spelling, not a lowercased one. A
    /// module that lowercased every name, or uppercased every schema name,
    /// cannot produce both from one rule.
    #[test]
    fn the_two_case_rules_cannot_be_confused() {
        // A name that does not resolve is reported as the query wrote it.
        assert_eq!(
            no_such_table("Foo").message,
            "no such table: Foo",
            "a name that does not resolve keeps the query's case"
        );
        // The same name, once it is in the schema, is reported as the schema
        // holds it -- which is a different rule, and the one the DML family
        // follows.
        assert_eq!(
            not_null_constraint("Foo", "Bb").message,
            "NOT NULL constraint failed: Foo.Bb",
            "a constraint reports the schema's case"
        );
        // And the schema can disagree with the query in the other direction
        // too: the statement said `TBL`, the schema said `tbl`, and the
        // message followed the schema.
        assert_eq!(
            not_null_constraint("tbl", "bb").message,
            "NOT NULL constraint failed: tbl.bb"
        );
    }

    /// Every query-sourced name is the spelling the statement wrote, and the
    /// engine reaches the message with that spelling rather than the token's.
    ///
    /// The rule is one sentence because the oracle makes it one sentence, and
    /// this is the pair of families the roadmap held up as the inconsistency:
    /// `no such function: XYZZY` keeps the case and `no such table: XYZZY`
    /// keeps it too. What differed was not the rule but the *caller* -- the
    /// function name was read back off the source by span (`parser::
    /// written_name`) while every other name was read off the folded token, so
    /// the same statement folded in one place and not the other.
    ///
    /// Both directions are quoted, because a rule that only ever kept the case
    /// would pass the first half and be wrong about a name that is already
    /// lower case.
    #[test]
    fn a_query_sourced_name_is_the_spelling_the_statement_wrote() {
        use crate::msg::CaseRule::Query;

        // What sqlite3 says, for these spellings. Every one was run against
        // 3.53.4; the transcript is the table above and the case rows in it.
        for written in ["XYZZY", "XyZzY", "MiXeD", "FOO", "t", "A"] {
            let spelled = format!("no such table: {written}");
            assert_eq!(
                Msg::NoSuchTable.render(&[written.into()]),
                spelled,
                "a table name is the spelling the statement wrote"
            );
            assert_eq!(
                Msg::NoSuchFunction.render(&[written.into()]),
                format!("no such function: {written}"),
                "a function name is the spelling the statement wrote, not a folded one"
            );
            assert_eq!(
                Msg::NoSuchColumn.render(&[written.into()]),
                format!("no such column: {written}"),
                "a column name is the spelling the statement wrote"
            );
            assert_eq!(
                Msg::WrongArgumentCount.render(&[written.into()]),
                format!("wrong number of arguments to function {written}()"),
                "an arity error echoes the function's own spelling"
            );
        }
        // And a name that is already folded comes back folded, which is what
        // makes the rule a spelling rule rather than a "never fold" one.
        assert_eq!(no_such_table("foo").message, "no such table: foo");
        assert_eq!(no_such_function("foo").message, "no such function: foo");

        // The two families agree about where the name came from, so a caller
        // cannot pick one and get the other's rule.
        for msg in [Msg::NoSuchTable, Msg::NoSuchFunction, Msg::NoSuchColumn] {
            assert!(
                !msg.case_rule().is_empty() && msg.case_rule().iter().all(|r| *r == Query),
                "{msg:?} is query-sourced, so it keeps the statement's spelling"
            );
        }
    }

    /// `Msg::case_rule` has exactly one entry per **name** hole.
    ///
    /// The list is documented as one entry per name, and a list of the wrong
    /// length is a claim about the message that nothing else checks: the
    /// render arms are tested against the oracle, and this function is tested
    /// against its own documentation. So the two are compared directly, and
    /// the messages whose holes are *not* names are named here -- the compound
    /// operator, the two ordinals, and the literal `main` a schema-qualified
    /// `no such column` carries. Those three are the ones a count-by-eye gets
    /// wrong, and they are the ones this test exists for.
    #[test]
    fn every_case_rule_entry_is_a_name_hole() {
        // The name holes, and only those, per the render arms. A hole is a name
        // when the render arm matches `Arg::Name` for it.
        fn name_holes(msg: Msg) -> usize {
            match msg {
                Msg::NoSuchTable
                | Msg::NoSuchColumn
                | Msg::NoSuchColumnDoubleQuoted
                | Msg::AmbiguousColumn
                | Msg::NoSuchFunction
                | Msg::NoSuchCollation
                | Msg::NoSuchIndex
                | Msg::WrongArgumentCount
                | Msg::MisuseOfAggregate
                | Msg::MisuseOfAggregateFunction
                | Msg::MisuseOfAliasedAggregate
                | Msg::TableExists
                | Msg::IndexExists
                | Msg::TableNamedExists
                | Msg::IndexNamedExists
                | Msg::DuplicateColumnName => 1,
                Msg::NoSuchTableSchemaQualified
                | Msg::NoSuchColumnQualified
                | Msg::NoSuchColumnForTable => 2,
                Msg::NoSuchColumnSchemaQualified | Msg::AmbiguousColumnStar => 3,
                Msg::ColumnCountMismatch => 1,
                // One hole, and it is the table; the other two are counts.
                Msg::CheckConstraint | Msg::UniqueConstraintRowid => 1,
                // The token a parse stopped on. It is a name the message
                // echoes as written, so it gets a rule, but it is not a hole
                // the grammar asked for a name -- `render` supplies the quotes
                // itself. The distinction the two `*_SCHEMA` lists above lean
                // on is the same one.
                Msg::SyntaxError | Msg::UnrecognizedToken => 1,
                Msg::NotNullConstraint | Msg::UniqueConstraint | Msg::ForeignKeyMismatch => 2,
                // No name hole at all.
                _ => 0,
            }
        }
        for msg in Msg::ALL {
            assert_eq!(
                msg.case_rule().len(),
                name_holes(*msg),
                "{msg:?} has {} rule entries but {msg:?} has {} name holes",
                msg.case_rule().len(),
                name_holes(*msg)
            );
        }
        // The three that a count by eye gets wrong, called out so the reason
        // they are empty is on the record next to the assertion that they are.
        for msg in [
            Msg::CompoundColumnCount,
            Msg::OrderByTermOutOfRange,
            Msg::GroupByTermOutOfRange,
        ] {
            assert!(
                msg.case_rule().is_empty(),
                "{msg:?} has a hole, but it is an operator or a count rather than                  a name, and a name is what the list is about"
            );
        }
        // And a message with a name hole and a count hole is listed for the
        // name alone, which is the case a whole-message count would get wrong.
        assert_eq!(Msg::ColumnCountMismatch.case_rule().len(), 1);
    }

    /// Building a message with the wrong number of parts is a mistake in the
    /// engine, and is reported as one rather than silently rendering a
    /// different message.
    #[test]
    #[should_panic(expected = "wrong number of arguments for")]
    fn a_message_built_with_the_wrong_number_of_parts_panics() {
        let _ = Msg::NoSuchTable.render(&[name("t"), name("extra")]);
    }

    /// A count hole does not satisfy a name hole, and the other way round.
    ///
    /// The types are part of the contract: a `no such table` cannot be handed a
    /// count, and a `GROUP BY` bound cannot be handed a name.
    #[test]
    #[should_panic(expected = "wrong number of arguments for")]
    fn a_count_cannot_fill_a_name_hole() {
        let _ = Msg::NoSuchTable.render(&[count(1)]);
    }

    /// The three kinds of hole are distinguishable, so a test can ask which
    /// kind a value is.
    #[test]
    fn the_hole_kinds_are_distinguishable() {
        assert_eq!(name("x").as_name(), Some("x"));
        assert_eq!(name("x").as_count(), None);
        assert_eq!(name("x").as_value(), None);
        assert_eq!(count(3).as_count(), Some(3));
        assert_eq!(count(3).as_name(), None);
        assert_eq!(value("1.5").as_value(), Some("1.5"));
        assert_eq!(value("1.5").as_name(), None);
        assert_eq!(value("1.5").as_count(), None);
    }

    /// The result code each message is raised under, which is what a caller
    /// branches on before it ever looks at the text.
    #[test]
    fn result_codes_match_sqlite() {
        assert_eq!(no_such_table("t").code(), 1);
        assert_eq!(no_such_column("c").code(), 1);
        assert_eq!(syntax_error("x").code(), 1);
        assert_eq!(misuse_of_aggregate_function("count").code(), 1);
        assert_eq!(no_such_collation("x").code(), 1);
        assert_eq!(not_null_constraint("t", "c").code(), 19);
        assert_eq!(unique_constraint("t", "c").code(), 19);
        assert_eq!(unique_constraint_rowid("t").code(), 19);
        assert_eq!(check_constraint("x").code(), 19);
        assert_eq!(foreign_key_constraint().code(), 19);
        assert_eq!(datatype_mismatch().code(), 20);
        assert_eq!(
            Msg::ColumnCountMismatch
                .error(&[name("t"), 2u64.into(), 3u64.into()])
                .code(),
            20
        );
        assert_eq!(
            Msg::ValuesForColumnsCount
                .error(&[count(3), 2u64.into()])
                .code(),
            20
        );
    }

    /// The extended codes name the constraint family, so a caller can tell a
    /// NOT NULL from a UNIQUE without reading the text.
    #[test]
    fn extended_codes_name_the_constraint_family() {
        assert_eq!(not_null_constraint("t", "c").extended_code(), 1299);
        assert_eq!(unique_constraint("t", "c").extended_code(), 2067);
        assert_eq!(unique_constraint_rowid("t").extended_code(), 2067);
        assert_eq!(check_constraint("x").extended_code(), 275);
        assert_eq!(foreign_key_constraint().extended_code(), 787);
        // A message with no extended code reports its primary code.
        assert_eq!(no_such_table("t").extended_code(), 1);
        assert_eq!(datatype_mismatch().extended_code(), 20);
    }

    /// The ordinal at the head of an out-of-range message follows the English
    /// rule SQLite uses, including the teens a last-digit rule gets wrong.
    #[test]
    fn ordinals_follow_the_english_rule() {
        for (n, text) in [
            (1u64, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (10, "10th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (14, "14th"),
            (20, "20th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (100, "100th"),
            (101, "101st"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
            (121, "121st"),
        ] {
            assert_eq!(ordinal(n), text, "ordinal({n}) is not {text}");
        }
    }
}

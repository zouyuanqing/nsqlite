//! `INSERT INTO t SELECT ...`: running a query and inserting each of its rows.
//!
//! SQLite treats a SELECT as an insert source the same way it treats a
//! `VALUES` clause, with one addition that changes everything: the rows are
//! produced while they are being inserted, so a naive loop reads back its own
//! writes. This module materialises the whole result set *before* it inserts
//! anything, which is what SQLite's own two-pass code does — `sqlite3Insert` in
//! `insert.c` codes the SELECT once and runs it twice, once to learn how many
//! columns it produces and once to produce the rows.
//!
//! # Snapshotting, and why it is not optional
//!
//! Running the SELECT to completion first is the mechanism that makes
//! `INSERT INTO t SELECT ... FROM t` terminate. Verified against sqlite3
//! 3.53.4: with three rows already in `t`, `INSERT INTO t SELECT a+10 FROM t`
//! leaves exactly six rows, the three new ones being `11 12 13` — the
//! originals, not an ever-growing frontier. This engine produces the same six
//! rows. An implementation that re-reads the table on each iteration either
//! loops forever or stops at some arbitrary point, and both are observable
//! differences from sqlite3. The same snapshot is what makes
//! `SELECT count(*)` see the pre-insert count:
//!
//! ```text
//! sqlite3> INSERT INTO t VALUES(1),(2),(3);
//! sqlite3> INSERT INTO t SELECT count(*) FROM t;
//! sqlite3> SELECT count(*) FROM t;   -->  4
//! ```
//!
//! It also makes the column count known before the first insert, which is how
//! sqlite3 can report a mismatch for a query that matches *no* rows at all:
//!
//! ```text
//! sqlite3> CREATE TABLE t(a,b); CREATE TABLE s(x);
//! sqlite3> INSERT INTO t(a,b) SELECT x FROM s;   -- s is empty
//! Parse error: 1 values for 2 columns
//! ```
//!
//! # What the snapshot does not do
//!
//! It is not a transaction. It makes a self-insert terminate; it does not make
//! a failed statement leave nothing behind. A constraint failure part way
//! through the write loop leaves the rows written before it in the b-tree,
//! visible for the rest of the session. See [`ATOMICITY`], which measures that
//! rather than assuming it.
//!
//! # Wiring
//!
//! The engine calls into a database, and the connection owns the b-tree, so
//! this module does not reach into it. It defines [`InsertTarget`], the five
//! things an insert needs from whatever is holding the rows, and is generic
//! over it. `Connection` implements it in `insert_select_hook.rs`, a child
//! module of `connection`; the hook that wires this up is a single `mod` line
//! there, and the arm in `Connection::insert` that used to return
//! `INSERT ... SELECT is not supported yet` now calls [`insert_select`].

use crate::affinity::apply as apply_affinity;
use crate::catalog::Table;
use crate::connection::{Outcome, Row};
use crate::error::{Error, Result, ResultCode};
use crate::eval::{eval, EvalCtx};
use crate::parser::Select;
use crate::value::Value;

/// What an `INSERT ... SELECT` needs from whatever holds the rows.
///
/// Implemented by `Connection` in `connection.rs`. It is four methods rather
/// than a direct dependency so that this module owns all the logic that decides
/// *what* to write, and the connection owns only *how* a row reaches the file.
pub trait InsertTarget {
    /// Resolves the destination table by the name the statement wrote.
    ///
    /// A schema qualifier is stripped for the lookup but kept for the error
    /// message, because that is what the statement wrote: `INSERT INTO
    /// main.t` reports `main.t`, not `t`.
    fn target_table(&self, written_name: &str) -> Result<Table>;

    /// Runs the source query and returns every row it produced, in query order,
    /// together with the width of its projection.
    ///
    /// The width is reported separately from the rows because the two answer
    /// different questions. The rows say what to insert; the width says whether
    /// the shape is right at all, and a query that matched *nothing* still has a
    /// width. That is what lets sqlite3 report `1 values for 2 columns` for a
    /// source with no rows in it — checked against 3.53.4, where `s` is empty
    /// and the statement is still an error. Taking the width from the first row
    /// would report `0 values` there instead, which is a different error.
    ///
    /// The contract is also that this completes before any row is written. A
    /// caller that streams rows instead reintroduces the self-insert loop; see
    /// the module comment. A second consequence is that every error the source
    /// query can raise is raised *before* the first insert, not part way
    /// through it -- measured on this engine: `INSERT INTO t SELECT abs(x,2)
    /// FROM s` fails with `wrong number of arguments to function abs()` and
    /// leaves 0 rows, the same rows an ordinary `VALUES` insert of the same
    /// broken expression leaves.
    fn run_source(&mut self, select: &Select) -> Result<Source>;

    /// The rowid for the next row, given the values that are about to be
    /// stored: the INTEGER PRIMARY KEY's own value when the statement supplied
    /// one, and otherwise one past the largest the table already holds.
    fn next_rowid(&mut self, table: &Table, values: &[Value]) -> Result<i64>;

    /// Writes one row at a given rowid.
    fn write_row(&mut self, table: &Table, rowid: i64, values: Vec<Value>) -> Result<()>;

    /// Publishes the statement's effect once every row is written: the change
    /// count, the last insert rowid, and the flush.
    fn finish(&mut self, changed: usize, last_rowid: Option<i64>) -> Result<()>;
}

/// What a source query produced: its rows, and how wide its projection is.
///
/// A result set with no rows still has a width, which is why these are two
/// fields rather than a `Vec` whose length would have to stand for both.
pub struct Source {
    pub rows: Vec<Row>,
    /// The number of columns the query projected, whether or not it matched
    /// anything. This is what the target count is checked against.
    pub width: usize,
}

/// A prepared `INSERT ... SELECT`: where each supplied value goes, and the rows.
struct Prepared {
    table: Table,
    /// The name as the statement wrote it, which is what an error quotes.
    written_name: String,
    /// Whether the statement named its targets. This, not the count, is what
    /// decides the wording of a count error.
    has_column_list: bool,
    /// Where each supplied value goes: one entry per named target, or one per
    /// table column when there was no column list.
    targets: Vec<usize>,
    rows: Vec<Vec<Value>>,
}

/// Runs a SELECT and inserts each of its rows, applying the same rules an
/// ordinary INSERT does.
///
/// The column list decides the targets, an unsupplied column takes its DEFAULT,
/// affinity is applied, NOT NULL is enforced, and the rowid is assigned the
/// way an ordinary INSERT assigns it. That is not a shortcut: sqlite3 applies
/// the identical rules to both forms, so a row that went in through `VALUES`
/// has to go in through `SELECT` unchanged.
///
/// A mismatch between the query's column count and the number of targets is an
/// error, and it is raised before any row is written — see
/// [`check_column_count`] for the two wordings sqlite3 uses.
pub fn insert_select<T: InsertTarget>(
    target: &mut T,
    written_name: &str,
    columns: Option<&[String]>,
    select: &Select,
) -> Result<Outcome> {
    let prepared = prepare(target, written_name, columns, select)?;
    insert_prepared(target, prepared)
}

/// Resolves the target, runs the query, and materialises its rows.
///
/// The whole result set is in hand before the caller writes anything, which is
/// the snapshot the module comment is about.
fn prepare<T: InsertTarget>(
    target: &mut T,
    written_name: &str,
    columns: Option<&[String]>,
    select: &Select,
) -> Result<Prepared> {
    // The target is resolved before the source query runs. sqlite3 reports
    // `no such table:` and `table t has no column named x` for the *target*
    // even when the source is also broken, so an unknown target wins: checked
    // against sqlite3 3.53.4, `INSERT INTO t(nosuch) SELECT nosuchcol FROM s`
    // reports the target column, not the source column.
    //
    // The reverse is *not* true and is worth being precise about, because it is
    // a real difference. When the target is fine and the source query is
    // broken, this module reports the source error, not a column count -- but
    // only because the SELECT runs first. That ordering is load-bearing for
    // error reporting, and the reason is specific: this engine resolves column
    // names only when the FROM table has rows, so `SELECT nosuchcol FROM s` on
    // an empty `s` does not fail at all, it returns a single column *named*
    // nosuchcol (a pre-existing gap, item 4 in docs/testing.md 5.3). With `s`
    // populated the same statement raises `no such column: nosuchcol` and this
    // module passes that error straight through, which is what sqlite3 does.
    let table = target.target_table(written_name)?;
    let has_column_list = columns.is_some();
    let targets = match columns {
        Some(names) => {
            let mut v = Vec::with_capacity(names.len());
            for n in names {
                v.push(table.column_index(n).ok_or_else(|| {
                    Error::new(
                        ResultCode::Error,
                        format!("table {written_name} has no column named {n}"),
                    )
                })?);
            }
            v
        }
        None => (0..table.len()).collect(),
    };

    let source = target.run_source(select)?;

    // Only now, with the result set in hand, is the shape checked. It is
    // against the *projection* width, so `SELECT *` is counted by the columns
    // the star expanded to rather than being special-cased, and a query that
    // matched nothing is still measured.
    check_column_count(written_name, has_column_list, targets.len(), source.width)?;

    Ok(Prepared {
        table,
        written_name: written_name.to_owned(),
        has_column_list,
        targets,
        rows: source.rows.into_iter().map(|r| r.values).collect(),
    })
}

/// Writes a materialised result set.
fn insert_prepared<T: InsertTarget>(target: &mut T, prepared: Prepared) -> Result<Outcome> {
    let Prepared {
        table,
        written_name,
        has_column_list,
        targets,
        rows,
    } = prepared;

    let mut inserted = 0usize;
    let mut last_rowid = None;
    for values in rows {
        // The width was checked once against the projection before anything was
        // written. Re-checking per row catches a result set whose rows are
        // ragged, which the current executor cannot produce but a future one
        // might, and costs nothing.
        if values.len() != targets.len() {
            return Err(count_error(
                &written_name,
                has_column_list,
                values.len(),
                targets.len(),
            ));
        }
        let full = build_row(&table, &targets, &values)?;
        let rowid = target.next_rowid(&table, &full)?;
        target.write_row(&table, rowid, full)?;
        last_rowid = Some(rowid);
        inserted += 1;
    }
    // `last_rowid` stays None for a query that matched no rows, which leaves
    // the connection's `last_insert_rowid()` alone. sqlite3 agrees: inserting
    // zero rows after one that set it to 1 still reports 1, and reports 0 on a
    // database that has never inserted.
    target.finish(inserted, last_rowid)?;
    Ok(Outcome::Changed(inserted))
}

/// Builds the stored row: the supplied values at their targets, and the DEFAULT
/// for every column the statement did not name.
///
/// The order is the one an ordinary INSERT uses, and each step has a
/// consequence:
///
/// 1. defaults are filled in, evaluated with no row in scope;
/// 2. NOT NULL is checked on the *finished* row, so a NOT NULL column the
///    statement never mentioned is caught too;
/// 3. affinity is applied last, which is why `'123'` into an INTEGER column
///    stores the integer.
fn build_row(table: &Table, targets: &[usize], values: &[Value]) -> Result<Vec<Value>> {
    let mut full = vec![Value::Null; table.len()];
    let mut named: Vec<bool> = vec![false; table.len()];
    for (i, pos) in targets.iter().enumerate() {
        // A target is resolved against the table before the loop starts, so it
        // is in range; the guard is here because the alternative is an index
        // panic on a malformed table.
        let Some(pos) = full.get(*pos).map(|_| *pos) else {
            return Err(Error::new(
                ResultCode::Error,
                format!("table {} has no column at position {}", table.name, pos),
            ));
        };
        // The first value wins when a column is named twice. sqlite3 agrees,
        // and it is not an error: `INSERT INTO t(a,a) SELECT x,y FROM s` with
        // x=1, y=2 stores a=1. Checked against 3.53.4, and the ordinary VALUES
        // path in this engine already behaves the same way.
        if named[pos] {
            continue;
        }
        full[pos] = values[i].clone();
        named[pos] = true;
    }
    let params: Vec<Value> = Vec::new();
    let ctx = EvalCtx::empty(&params);
    for (i, col) in table.columns.iter().enumerate() {
        if named[i] {
            continue;
        }
        full[i] = match &col.default {
            Some(e) => eval(e, &ctx)?,
            None => Value::Null,
        };
    }
    check_not_null(table, &full)?;
    for (i, col) in table.columns.iter().enumerate() {
        full[i] = apply_affinity(&full[i], col.affinity);
    }
    Ok(full)
}

/// NOT NULL enforcement, with sqlite3's wording and extended code.
///
/// ```text
/// sqlite3> CREATE TABLE t(a NOT NULL); CREATE TABLE s(x);
/// sqlite3> INSERT INTO s VALUES(1),(NULL);
/// sqlite3> INSERT INTO t SELECT x FROM s;
/// Error: NOT NULL constraint failed: t.a
/// ```
///
/// The extended code is 1299 (`SQLITE_CONSTRAINT_NOTNULL`), read off the C
/// API rather than recalled.
fn check_not_null(table: &Table, values: &[Value]) -> Result<()> {
    for (i, col) in table.columns.iter().enumerate() {
        if col.not_null && values.get(i).map(|v| v.is_null()).unwrap_or(true) {
            return Err(Error::new(
                ResultCode::Constraint,
                format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
            )
            .with_extended(1299));
        }
    }
    Ok(())
}

/// Checks that the query produced as many columns as the statement has targets.
///
/// sqlite3 has two wordings and what selects between them is the *written
/// column list*, not the counts. Verified against 3.53.4, with a table `t(a,b)`
/// and a source column `x`:
///
/// | statement                             | message                                          |
/// |---------------------------------------|--------------------------------------------------|
/// | `INSERT INTO t(a,b) SELECT x`         | `1 values for 2 columns`                         |
/// | `INSERT INTO t(a,b) SELECT x,x,x`     | `3 values for 2 columns`                         |
/// | `INSERT INTO t(a,b,c) SELECT x,x`     | `2 values for 3 columns`                         |
/// | `INSERT INTO t        SELECT x`       | `table t has 2 columns but 1 values were supplied` |
/// | `INSERT INTO t        SELECT x,x`     | `table t has 1 columns but 2 values were supplied` |
/// | `INSERT INTO t        SELECT * FROM s3col` | `table t has 2 columns but 3 values were supplied` |
///
/// The named form counts the named columns, the bare form counts the table's
/// columns, and the `*` case follows the bare form because no column list was
/// written. Both are `SQLITE_ERROR` — primary code 1, confirmed through the C
/// API via Python's `sqlite3`, not recalled.
fn check_column_count(
    written_name: &str,
    has_column_list: bool,
    target_count: usize,
    source_count: usize,
) -> Result<()> {
    if target_count == source_count {
        return Ok(());
    }
    Err(count_error(
        written_name,
        has_column_list,
        source_count,
        target_count,
    ))
}

/// The count error in whichever of sqlite3's two wordings applies.
fn count_error(
    written_name: &str,
    has_column_list: bool,
    source_count: usize,
    target_count: usize,
) -> Error {
    if has_column_list {
        Error::new(
            ResultCode::Error,
            format!("{source_count} values for {target_count} columns"),
        )
    } else {
        Error::new(
            ResultCode::Error,
            format!(
                "table {written_name} has {target_count} columns but {source_count} values were supplied"
            ),
        )
    }
}

/// What a caller has to know about statement-level atomicity.
///
/// A constant rather than only prose so a test can assert the promise is still
/// being made in the same words, and so the integration hook has something to
/// point at.
///
/// Every claim below is a measurement of *this* engine, not a description of
/// sqlite3, and where the two differ the difference is stated. Read the
/// "Honest summary" first: the short version is that this engine does not have
/// statement-level atomicity at all, and no arrangement of caller statements
/// gives it one.
pub const ATOMICITY: &str = "\
An INSERT ... SELECT that fails part way has to leave nothing behind. This
engine does not do that, and the honest answer is that there is nothing the
caller can do to make a single statement atomic, short of not using it.

  * A failure part way through is NOT undone. The rows written before the
    failure stay in the b-tree. They are visible to every later statement in
    the same session, and they are gone only because the pager never flushes on
    the error path, so the dirty pages are discarded when the connection drops.
    Both halves of that were measured on this engine, through Connection rather
    than through the CLI:

      - INSERT INTO t SELECT x FROM s, where s holds (1),(2),(NULL),(4) and t.a
        is NOT NULL, fails with `NOT NULL constraint failed: t.a`. In the same
        session, SELECT count(*) FROM t returns 2 and SELECT sum(a) FROM t
        returns 3 -- the rows before the NULL are really there. After close and
        reopen, the same count returns 0.

    sqlite3 returns 0 in *both* cases, because every statement runs inside an
    implicit transaction. The reopen is doing this engine a favour, not
    describing it: the file is correct, the running session is not.

  * A failure in the source query itself happens before any row is written,
    because this module runs the query to completion before it writes anything.
    That is a consequence of the snapshot, not atomicity: it covers errors the
    SELECT raises, and nothing else. A constraint failure is raised while the
    rows are being written, and is not covered.

What was checked and found NOT to be a partial write: the claim that a large
insert leaves a fraction of its rows on disk. It does not. A single
INSERT ... VALUES holding 2499 rows followed by (1,1),(NULL,2) into
t(a NOT NULL, b) leaves 0 rows on disk at N = 300, 1000, 2499, 5000, 20000 and
60000, matching sqlite3 at every size. There is no size at which a partial
write reaches the file; the discarded pages are discarded in full. (Reading
those rows back *within* the session reports a smaller number than were
written, at 442 of 2499 -- but that is a read-side bug in this engine that
reproduces identically on a fully successful insert, so it is not an atomicity
effect.)

Why the rollback journal does not help here. There is one, and it works: a
transaction that fails and is rolled back leaves nothing behind. But it needs
BEGIN, and BEGIN cannot be relied on. A database file that does not yet exist
cannot be journalled at all: Pager::open takes its fresh branch for a
zero-length file and sets the pager's path to None, so begin_journal refuses
with `an in-memory database cannot be journalled` for the entire lifetime of
that connection. Measured, on this engine:

  - open a path that does not exist, BEGIN            -> an in-memory database
                                                         cannot be journalled
  - ... run CREATE TABLE in the same connection       -> succeeds
  - ... BEGIN again in the same connection            -> still fails
  - close, reopen the now-existing file, BEGIN        -> succeeds

So the recipe below works only on a database that already has a file, and the
file existing is not something the caller can assume.

    BEGIN;                                   -- only on an existing file
    INSERT INTO t SELECT ... FROM s;
    COMMIT;                                  -- only reached if it succeeded

On a fresh file, or in an in-memory database, the caller has no option that
gives it atomicity, and should report the statement as having failed with rows
possibly left behind rather than as having failed cleanly. This engine has a
rollback journal but no savepoints, so there is no way to bracket a single
statement the way sqlite3's implicit transaction does.

One claim that could not be exercised at all, recorded so it is not mistaken
for a tested one: an integer overflow mid-statement. This engine does not raise
one -- SELECT 9223372036854775807+1 silently returns the float
9.223372036854776e+18, where sqlite3 returns `integer overflow` -- so the
failure mode is unreachable here regardless of atomicity.";

#[cfg(test)]
#[path = "insert_select_tests.rs"]
mod tests;

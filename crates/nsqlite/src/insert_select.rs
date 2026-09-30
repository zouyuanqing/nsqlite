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
use crate::parser::{ConflictAction, Select};
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

    /// The text of the first CHECK on `table` that `values` fails, or `None`.
    ///
    /// A trait method rather than a check here because this module is generic
    /// over its target and does not evaluate expressions: the engine has to
    /// hand in the answer, or the predicate and its evaluation would have to be
    /// built twice. `None` means every CHECK passed, which includes a NULL
    /// result -- a CHECK is satisfied unless it is false, and unknown is not
    /// false.
    fn failing_check_text(&mut self, table: &Table, values: &[Value]) -> Result<Option<String>>;

    /// Decides what a UNIQUE conflict on `values` means for this statement, and
    /// carries out the part of the action that touches storage.
    ///
    /// `None` means the row is fine and should be written. `Err` is a refusal
    /// that ends the statement. The actions that are not refusals are handled
    /// here rather than by the caller because each one needs something only the
    /// target can do: IGNORE has to skip the row, and REPLACE has to delete the
    /// row it collides with before the new one goes in.
    ///
    /// The rules, all measured against sqlite3 3.53.4: a key holding a NULL
    /// never conflicts, so any number of NULLs is legal; the comparison is
    /// `Value::compare` and not `==`, which is what makes the integer 1 and the
    /// real 1.0 collide while the text '1' does not; and OR IGNORE covers
    /// conflicts only, so a NOT NULL violation beside it still aborts the
    /// statement.
    fn write_unique(
        &mut self,
        table: &Table,
        values: &[Value],
        conflict: ConflictAction,
        pending: &[(Vec<usize>, Vec<Value>)],
    ) -> Result<UniqueOutcome>;

    /// The uniqueness keys a row that has just been written should be recorded
    /// under, so that a later row of the *same statement* is checked against it.
    ///
    /// A key holding a NULL is left out: such a row can never conflict with
    /// anything, so keeping it would cost a comparison and find nothing.
    fn written_keys(&self, table: &Table, values: &[Value]) -> Vec<Vec<usize>>;

    /// Every uniqueness constraint the table has, as groups of column indices.
    ///
    /// Separate from [`InsertTarget::written_keys`] because it answers a
    /// different question: not "which keys does this row occupy" but "which keys
    /// exist at all", which is what the pre-pass needs to know whether a
    /// statement can conflict with anything before writing a row.
    fn unique_keys(&self, table: &Table) -> Vec<Vec<usize>>;

    /// The first of `keys` that a row about to be written collides with, judged
    /// against the rows the table already holds.
    ///
    /// Separate from [`InsertTarget::write_unique`] because the pre-pass must
    /// decide a *refusal* without performing one, and because the write loop
    /// needs the conflict's identity either way. `exclude` is the row being
    /// updated, which an UPDATE has already taken out and which would otherwise
    /// collide with itself.
    fn stored_unique_conflict(
        &mut self,
        table: &Table,
        keys: &[Vec<usize>],
        values: &[Value],
        exclude: Option<i64>,
    ) -> Result<Option<usize>>;

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
    ///
    /// An entry is a column's position, or `None` for a target that is not a
    /// column at all. There is exactly one such name and it is a row's key
    /// rather than a value the record holds: `INSERT INTO t(rowid, a) VALUES
    /// (77, 5)` writes the row under key 77. Measured against sqlite3 3.53.4,
    /// which accepts it on a table that has no such column and refuses it
    /// nowhere -- the key is not a column, so the column-list lookup misses it
    /// and the rowid rule below has to answer.
    targets: Vec<Option<usize>>,
    rows: Vec<Vec<Value>>,
}

/// What a `write_unique` check decided about one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniqueOutcome {
    /// No conflict, or a conflict OR REPLACE resolved by deleting the row it
    /// collided with. Either way the row is written.
    Write,
    /// A conflict that OR IGNORE swallowed. Skip the row and carry on with the
    /// rest of the statement -- this is per ROW, not per statement.
    Skip,
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

/// What a write does when it hits a UNIQUE or PRIMARY KEY conflict.
pub fn insert_select<T: InsertTarget>(
    target: &mut T,
    written_name: &str,
    columns: Option<&[String]>,
    select: &Select,
    conflict: ConflictAction,
) -> Result<Outcome> {
    let prepared = prepare(target, written_name, columns, select)?;
    insert_prepared(target, prepared, conflict)
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
                v.push(build_target(&table, n, written_name)?);
            }
            v
        }
        None => (0..table.len()).map(Some).collect(),
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
///
/// The rows are *built and checked* first and written second, and the split is
/// the point. See [`ATOMICITY`] for the full statement; in short, this engine
/// has a rollback journal but no savepoints, so a failure part way through a
/// write loop cannot undo itself. A row that fails a constraint is therefore
/// discovered before any of them reaches the b-tree, because every check that
/// can be made without the b-tree is made first:
///
/// - a DEFAULT that does not evaluate,
/// - NOT NULL on the finished row,
/// - a value for the INTEGER PRIMARY KEY that is not an integer, which
///   `next_rowid` raises before it reads the tree.
///
/// What is *not* checkable without writing is left to the write loop: a
/// duplicate rowid, and UNIQUE and CHECK constraints the b-tree itself
/// enforces. Those still leave the earlier rows behind, which is the honest
/// limit of what this engine can do and is why the note says so.
fn insert_prepared<T: InsertTarget>(
    target: &mut T,
    prepared: Prepared,
    conflict: ConflictAction,
) -> Result<Outcome> {
    let Prepared {
        table,
        written_name,
        has_column_list,
        targets,
        rows,
    } = prepared;

    // Every row is built before any is written. Building is where the
    // fallible checks that do not need the b-tree live, so doing it up front
    // turns "half of this statement is now in the table" into "none of it is".
    //
    // Building the whole set first does not change what gets stored. sqlite3
    // evaluates a column's DEFAULT once per row, not once per statement --
    // checked against 3.53.4, where `INSERT INTO t(a) VALUES(1),(2),(3)` on
    // `t(a,b DEFAULT abs(random()))` gives three *different* b values -- so
    // building each row in its own turn is the same number of evaluations of
    // the same expressions, in the same order.
    let mut built: Vec<(Vec<Value>, Option<i64>)> = Vec::with_capacity(rows.len());
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
        built.push(build_row(&table, &targets, &values)?);
    }

    // A value for the INTEGER PRIMARY KEY that is not an integer is refused
    // here, in the checking pass, rather than by the write. The check is pure --
    // it reads the table's definition and the row's values and nothing else --
    // so it can be made for every row before any of them is written. That is
    // what keeps the statement from leaving rows behind: resolved in the write
    // loop, the rows ahead of it would already be in the b-tree, and this engine
    // has no way to take them back out. Checked against sqlite3 3.53.4, which
    // leaves 0 rows in the same case.
    for (full, _) in &built {
        check_rowid_value(&table, full)?;
    }

    // CHECK, in the same pre-pass and for the same reason: resolved in the
    // write loop it would leave the rows ahead of the failing one in the
    // b-tree, which this engine cannot undo.
    let mut check_failed: Vec<bool> = Vec::new();
    if !table.checks.is_empty() {
        for (full, _) in &built {
            let failed = target.failing_check_text(&table, full)?;
            if let Some(text) = failed {
                if !matches!(conflict, ConflictAction::Ignore) {
                    return Err(crate::msg::check_constraint(&text));
                }
                check_failed.push(true);
                continue;
            }
            check_failed.push(false);
        }
    }

    // The uniqueness check is pure in the same way, and needs the same pre-pass
    // for the same reason. Resolved in the write loop it would leave the rows
    // ahead of the failing one in the b-tree, because this engine has no way to
    // take them back out: `INSERT INTO u VALUES(1),(1)` would leave 1 row where
    // sqlite3 leaves 0. A key holding a NULL is exempt, and the rows are
    // compared to each other in order as well as to the table, so the duplicate
    // inside one statement is found here too.
    //
    // Only the refusal is hoisted. OR IGNORE and OR REPLACE are not refusals --
    // one skips a row, the other deletes one -- so they stay in the write loop,
    // where the target can carry them out.
    let keys = target.unique_keys(&table);
    if !keys.is_empty() {
        let refusing = !matches!(conflict, ConflictAction::Ignore | ConflictAction::Replace);
        let mut seen: Vec<(Vec<usize>, Vec<Value>)> = Vec::new();
        for (full, _) in &built {
            let hit = match crate::connection::find_unique_conflict(&keys, full, &seen) {
                Some(i) => Some(i),
                None if refusing => {
                    target.stored_unique_conflict(&table, &keys, full, None)?
                }
                None => None,
            };
            if let Some(i) = hit {
                if refusing {
                    return Err(crate::index_ddl::unique_violation(
                        &table.name,
                        &crate::connection::key_names(&table, &keys[i]),
                    ));
                }
            }
            seen.extend(
                target
                    .written_keys(&table, full)
                    .into_iter()
                    .map(|key| (key, full.clone())),
            );
        }
    }

    // The rowid itself is resolved in the write loop, and that is deliberate
    // rather than an oversight. sqlite3 hands out one past the largest rowid
    // *as the statement progresses*, so a three-row insert into a table holding
    // one row gets 2, 3 and 4 rather than 2, 2 and 2. Measured against 3.53.4:
    //   CREATE TABLE u(a,b); INSERT INTO u VALUES(1,2);
    //   INSERT INTO u SELECT 5,5 UNION ALL SELECT 6,6;
    //   SELECT rowid,a FROM u;  -->  (1,1) (2,5) (3,6)
    // Resolving them all against the tree before writing would hand out the
    // same rowid three times and overwrite the first two rows with the third.
    let mut inserted = 0usize;
    let mut last_rowid = None;
    // The rows this statement has already written, so a duplicate *within* one
    // statement is caught like any other. `INSERT INTO u SELECT x FROM s` with
    // s holding (1),(2),(1) leaves 0 rows in the reference, not 1, and this is
    // what makes it 0 here.
    let mut pending: Vec<(Vec<usize>, Vec<Value>)> = Vec::new();
    for (index, (full, explicit)) in built.into_iter().enumerate() {
        // A row OR IGNORE skipped for failing a CHECK, decided in the pass
        // above. Like the UNIQUE skip below, it has to be acted on here: the
        // pass only records the verdict, because a `continue` in the pass
        // would skip the check rather than the write.
        if check_failed.get(index).copied().unwrap_or(false) {
            continue;
        }
        // A key the statement named wins over the one this engine would hand
        // out, and a key it named as NULL is the same as not naming it at all.
        let rowid = match explicit {
            Some(v) => v,
            None => target.next_rowid(&table, &full)?,
        };
        // Checked before the write, which is what leaves nothing behind when it
        // refuses. `write_unique` also carries out OR IGNORE and OR REPLACE,
        // the two actions that are not refusals.
        if target.write_unique(&table, &full, conflict, &pending)? == UniqueOutcome::Skip {
            continue;
        }
        target.write_row(&table, rowid, full.clone())?;
        for key in target.written_keys(&table, &full) {
            pending.push((key, full.clone()));
        }
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

/// Refuses a row whose value for the INTEGER PRIMARY KEY is not an integer.
///
/// This is the same check `Connection::next_rowid` makes, split out so that it
/// can be run over every row before the first one is written. It reads the
/// table's definition and the row's own values and nothing else, so it is a
/// pure test and hoisting it changes nothing but *when* the statement refuses.
///
/// sqlite3 3.53.4 words it `datatype mismatch`, bare, with `SQLITE_MISMATCH`
/// (20) -- confirmed through Python's `sqlite3`, which reports that code for
/// this error and for nothing else in this family. This engine says the same
/// thing with the offending value named, which is more information rather than
/// less, and that wording is the `VALUES` path's and is shared by both.
fn check_rowid_value(table: &Table, values: &[Value]) -> Result<()> {
    let Some(i) = table.rowid_alias else {
        return Ok(());
    };
    match values.get(i) {
        // An integer takes its own value, and a NULL or an absent column takes
        // one past the largest; both are fine and both are decided by
        // `next_rowid` against the live tree.
        Some(Value::Integer(_)) | Some(Value::Null) | None => Ok(()),
        Some(other) => Err(Error::new(
            ResultCode::Mismatch,
            format!("datatype mismatch: {other} is not an integer"),
        )),
    }
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
fn build_row(
    table: &Table,
    targets: &[Option<usize>],
    values: &[Value],
) -> Result<(Vec<Value>, Option<i64>)> {
    let mut full = vec![Value::Null; table.len()];
    let mut named: Vec<bool> = vec![false; table.len()];
    let mut rowid: Option<i64> = None;
    for (i, target) in targets.iter().enumerate() {
        // A target that is not a column is the row's key, and the row does not
        // hold it: it is the b-tree cell the record is written under, so it is
        // taken out of the row and handed back separately. The value is checked
        // by the same `check_rowid_value` that reads the alias column, so
        // `INSERT INTO t(rowid,a) VALUES('x',1)` is refused the same way a
        // non-integer INTEGER PRIMARY KEY is.
        let Some(pos) = *target else {
            match &values[i] {
                Value::Integer(v) => rowid = Some(*v),
                Value::Null => {}
                _ => return Err(crate::msg::datatype_mismatch()),
            }
            continue;
        };
        // A target is resolved against the table before the loop starts, so it
        // is in range; the guard is here because the alternative is an index
        // panic on a malformed table.
        let Some(pos) = full.get(pos).map(|_| pos) else {
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
    Ok((full, rowid))
}

/// Where one named INSERT target goes.
///
/// A column is found by name, and the three rowid names are the one case where
/// the lookup misses on purpose: a key is not a column, so
/// `INSERT INTO t(rowid, a) VALUES(77, 5)` names something the record does not
/// hold. Measured against sqlite3 3.53.4, which accepts it and writes the row
/// under key 77 -- so it is answered with `None` and `build_row` takes the key
/// out of the row.
///
/// A real column of that name wins, in that order, because `column_index` is
/// tried first. `CREATE TABLE u(b, rowid, c)` really does have a column called
/// `rowid`, and `INSERT INTO u(rowid) VALUES(9)` writes 9 into it -- the
/// pseudo-column does not shadow a real one any more here than it does in a
/// SELECT.
///
/// A name that is neither is the error it always was, and a WITHOUT ROWID table
/// has no key to name: its DDL is refused upstream, so this is not reachable
/// for one, and leaving the rowid answer unconditional rather than guarding it
/// on `table.without_rowid` would be answering for a table kind that cannot be
/// built.
pub fn build_target(table: &Table, name: &str, written_name: &str) -> Result<Option<usize>> {
    if let Some(pos) = table.column_index(name) {
        return Ok(Some(pos));
    }
    if crate::join::is_rowid_name(name) {
        return Ok(None);
    }
    Err(Error::new(
        ResultCode::Error,
        format!("table {written_name} has no column named {name}"),
    ))
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
///
/// Public to the crate because the ordinary `VALUES` path raises the same
/// error and has to raise it in the same words with the same result code. It
/// used to compose its own, and the two drifted: `VALUES` always took the
/// `table ... has ... columns` wording even when the statement named its
/// columns, and it used `SQLITE_MISMATCH` (20) where sqlite3 uses
/// `SQLITE_ERROR` (1). One constructor is the fix, and it is the same
/// argument the module makes about rows: two forms of INSERT that build the
/// error separately are two forms that will disagree eventually.
///
/// Verified against sqlite3 3.53.4 for both forms, and through Python's
/// `sqlite3` for the code: every count mismatch, in either form, with or
/// without a written column list, is `OperationalError` carrying
/// `SQLITE_ERROR`.
pub fn count_error(
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
/// "Honest summary" first: the short version is that this engine still does not
/// have statement-level atomicity, because it cannot undo a write, but it now
/// refuses rather than half-applying for every failure it can see before it
/// writes. The one it cannot see is named below.
pub const ATOMICITY: &str = "\
An INSERT ... SELECT that fails part way has to leave nothing behind. This
engine cannot promise that in general, because it has a rollback journal but no
savepoints and no implicit transaction, so a write that has happened cannot be
taken back. What it does instead is refuse: every failure it can detect before
writing anything is raised before the first row is written, so the statement
either stores all of its rows or none of them.

  * REFUSED, leaving 0 rows, in this session and on disk. Every row is built
    and checked before the first one is written, so a check that fails is
    failed before the b-tree is touched. Measured on this engine, and the same
    0 is what sqlite3 3.53.4 returns in each case:

      - NOT NULL on a column the statement did not name, or named and supplied
        NULL. INSERT INTO t SELECT x,y FROM s, where s holds (1,1),(NULL,2),
        (3,3) and t.a is NOT NULL, fails with `NOT NULL constraint failed: t.a`
        and SELECT count(*) FROM t returns 0, before and after a reopen. This
        used to return 2 in the same session: the rows ahead of the NULL were
        written first and nothing could take them back. The build-then-write
        split is what changed that.
      - a value for the INTEGER PRIMARY KEY that is not an integer
        (`datatype mismatch`), which is also 0 here and 0 in sqlite3.
      - an error in the source query, which was always before the first write
        because the query is run to completion before anything is inserted.

  * NOT UNDONE, leaving the earlier rows behind. A duplicate rowid cannot be
    found without writing: the row has to reach the b-tree for the b-tree to
    discover that its rowid is taken, and nothing here can roll that back. So
    INSERT INTO t SELECT x,y FROM s, with t(a INTEGER PRIMARY KEY) and s
    holding (1,'p'),(2,'q'),(1,'r'), fails with `UNIQUE constraint failed:
    t.a` and SELECT count(*) FROM t returns 2 in this session. sqlite3 returns
    0. The same is true of UNIQUE and CHECK constraints the b-tree itself
    enforces, and of any failure raised by a DEFAULT expression that this
    engine accepts but sqlite3 would not -- see the note on non-constant
    DEFAULTs in the module docs. Closing this gap needs savepoints or an
    implicit transaction, neither of which this engine has.

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

Why the rollback journal does not close the remaining gap. There is one, and it
works: a transaction that fails and is rolled back leaves nothing behind. But it
needs BEGIN, and BEGIN cannot be relied on. A database file that does not yet
exist cannot be journalled at all: Pager::open takes its fresh branch for a
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
gives it atomicity for the remaining cases, and should report the statement as
having failed with rows possibly left behind rather than as having failed
cleanly. This engine has a rollback journal but no savepoints, so there is no
way to bracket a single statement the way sqlite3's implicit transaction does.
The refusals listed first need no such bracket, and that is why a NOT NULL
failure is safe on any database and a duplicate rowid is not.

One claim that could not be exercised at all, recorded so it is not mistaken
for a tested one: an integer overflow mid-statement. This engine does not raise
one -- SELECT 9223372036854775807+1 silently returns the float
9.223372036854776e+18, where sqlite3 returns `integer overflow` -- so the
failure mode is unreachable here regardless of atomicity.";

#[cfg(test)]
#[path = "insert_select_tests.rs"]
mod tests;

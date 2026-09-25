//! The `Connection` half of `INSERT ... SELECT`.
//!
//! [`crate::insert_select`] owns every decision about *what* to write: which
//! column each supplied value goes to, which columns take their DEFAULT,
//! affinity, NOT NULL, the rowid, and the wording of every error. What it
//! deliberately does not own is *how* a row reaches the file, because that
//! needs the pager, the b-tree and the connection's own change counters, all of
//! which are private to `connection.rs`.
//!
//! This file is that other half, and it is a child module of `connection` so
//! that it can reach those private items without `connection.rs` having to grow
//! an inline impl. Declaring it is a one-line hook:
//!
//! ```text
//! #[path = "insert_select_hook.rs"]
//! mod insert_select_hook;
//! ```
//!
//! placed inside `connection.rs` next to the other `mod` lines. That is the
//! entire integration: the arm in `Connection::insert` that used to say
//! `INSERT ... SELECT is not supported yet` now calls
//! [`crate::insert_select::insert_select`], and every method below exists only
//! to answer that call.
//!
//! Each method is a thin forward to the private function of the same name that
//! already did this job for the `VALUES` path. Reusing them rather than
//! reimplementing is the point: the two forms of INSERT have to produce
//! byte-identical rows, byte-identical rowids and byte-identical errors, and
//! the only way to guarantee that is for them to call the same code.

use crate::catalog::Table;
use crate::connection::{Connection, Outcome};
use crate::error::{Error, Result, ResultCode};
use crate::insert_select::{InsertTarget, Source};
use crate::parser::Select;
use crate::value::Value;

impl InsertTarget for Connection {
    /// The destination, resolved by name exactly as the `VALUES` path resolves
    /// it, so a schema qualifier and an unknown table both behave the same in
    /// either form.
    fn target_table(&self, written_name: &str) -> Result<Table> {
        self.table(written_name).cloned()
    }

    /// Runs the source query with the engine's own SELECT path.
    ///
    /// A consequence worth stating, because it is what makes the module's
    /// atomicity note what it is: the query runs to completion here, before the
    /// caller's write loop starts. Every error the query can raise -- an
    /// unknown table, an unknown column, a function arity error, an unresolvable
    /// aggregate -- is therefore raised *before* any row is written, not part
    /// way through the insert. Checked against sqlite3 3.53.4: the same
    /// statements fail with the same messages, and on this engine a failing
    /// source leaves the target with 0 rows.
    ///
    /// `width` comes from the projection's own column names rather than from
    /// the first row, which is what lets a query that matched nothing still
    /// report a shape mismatch -- the `1 values for 2 columns` case.
    fn run_source(&mut self, select: &Select) -> Result<Source> {
        match self.select(select)? {
            Outcome::Query { columns, rows } => Ok(Source {
                width: columns.len(),
                rows,
            }),
            other => Err(Error::new(
                ResultCode::Error,
                format!("a SELECT source returned {other:?} rather than rows"),
            )),
        }
    }

    /// The rowid for the next row: the INTEGER PRIMARY KEY's own value when the
    /// statement supplied one, and otherwise one past the largest the table
    /// already holds.
    ///
    /// Note what this reads: the *b-tree*, not a running maximum of the rows
    /// this statement has written. That is what makes `INSERT INTO t SELECT ...
    /// FROM t` terminate -- the rowid does not depend on the statement's own
    /// output, so writing a row cannot change where the next one goes. The
    /// fake in the unit tests derives it differently, and the difference is
    /// the whole self-insert hazard.
    fn next_rowid(&mut self, table: &Table, values: &[Value]) -> Result<i64> {
        Connection::next_rowid(self, table, values)
    }

    /// Writes one row at a given rowid, including the UNIQUE wording a
    /// duplicate rowid alias gets.
    fn write_row(&mut self, table: &Table, rowid: i64, values: Vec<Value>) -> Result<()> {
        self.insert_row(table, rowid, values)
    }

    /// Publishes the statement's effect: the change count, the last insert
    /// rowid, and the flush.
    ///
    /// `last_rowid` is `None` when the query matched no rows, which leaves
    /// `last_insert_rowid()` alone rather than clearing it. sqlite3 agrees:
    /// inserting zero rows after one that set it to 1 still reports 1, and
    /// reports 0 on a database that has never inserted. Verified against
    /// 3.53.4, both from a fresh database and after a prior insert.
    ///
    /// The flush happens only here, which is why a statement that fails part
    /// way leaves its dirty pages unwritten. That is not the same as undoing
    /// them -- they are still in the b-tree and still visible for the rest of
    /// the session. See [`crate::insert_select::ATOMICITY`], which says so.
    fn finish(&mut self, changed: usize, last_rowid: Option<i64>) -> Result<()> {
        self.changes = changed;
        if let Some(rowid) = last_rowid {
            self.last_insert_rowid = rowid;
        }
        self.pager.flush()?;
        Ok(())
    }
}

//! `CREATE INDEX` and `DROP INDEX`: the DDL that puts an index b-tree in the
//! file and keeps it in step with the table it indexes.
//!
//! Everything below the schema table already exists — [`crate::index`] holds
//! the key record and the leaf page, [`crate::index_interior`] the interior
//! layer and the splitting — and both are byte-compatible with files the real
//! sqlite3 writes. What was missing is everything *above* them: reading the
//! table to produce the entries, writing and removing the row in
//! `sqlite_schema` that names the tree, releasing the pages when the index
//! goes away, and the three write paths that keep an existing index current.
//!
//! # The schema row
//!
//! An index is recorded in `sqlite_schema` exactly as a table is, with `type`
//! set to `index` and `name` and `tbl_name` both present — `tbl_name` naming
//! the *table*, which is the one place an index row differs from a table row.
//! Asked of sqlite3 3.53.4 directly:
//!
//! ```text
//! CREATE TABLE t(a,b,c);
//! CREATE INDEX i1 ON t(a);
//! CREATE UNIQUE INDEX i2 ON t(a,b);
//!
//! type|name|tbl_name|rootpage|sql
//! table|t     |t     |2|CREATE TABLE t(a,b,c)
//! index|i1    |t     |3|CREATE INDEX i1 ON t(a)
//! index|i2    |t     |4|CREATE UNIQUE INDEX i2 ON t(a,b)
//! ```
//!
//! The `sql` column holds the statement from the word `CREATE` to the last
//! character before the terminating semicolon. It is **not** a verbatim copy,
//! and it is not a re-rendering either — the reference rewrites only the
//! *prefix* and copies the rest, which is the distinction that decides what has
//! to be stored. Asked of sqlite3 3.53.4 directly, storing each statement and
//! reading `sql` back:
//!
//! | written                                            | stored                                                 |
//! |----------------------------------------------------|--------------------------------------------------------|
//! | `CREATE    INDEX   i1   ON t(a)`                   | `CREATE INDEX i1   ON t(a)`                             |
//! | `CREATE INDEX i1 ON t(a)  ;`                       | `CREATE INDEX i1 ON t(a)  ` — the spaces stay, the `;` goes |
//! | `CREATE INDEX IF NOT EXISTS i1 ON t(a)`            | `CREATE INDEX i1 ON t(a)`                               |
//! | `CREATE    INDEX   main.q2   ON   t(a)`            | `CREATE INDEX q2   ON   t(a)`                           |
//! | `CREATE INDEX i1 ON t(a , b)`                      | `CREATE INDEX i1 ON t(a , b)`                           |
//! | `CREATE INDEX i9\nON t(a DESC, b COLLATE NOCASE)`  | unchanged, newline and all                              |
//! | `CREATE UNIQUE INDEX i10 ON t(b ASC) WHERE a>1`    | unchanged                                               |
//! | `CREATE INDEX iz ON t(a) /* c */`                  | `CREATE INDEX iz ON t(a) /* c */`                       |
//!
//! The rule is that everything up to and including the index *name* is
//! normalised — `CREATE`, an optional `UNIQUE`, `INDEX`, and the name, each
//! separated by one space, with `IF NOT EXISTS` and any schema qualifier on the
//! name dropped — and everything from the next token onwards is copied byte for
//! byte out of the source. The `main.q2` row is the one that proves where the
//! boundary is: the qualifier did not survive in the stored prefix, and the
//! three spaces before `ON` did survive in the tail, so a re-renderer that
//! normalised the whole statement would have got that row wrong. Nothing is
//! trimmed at the end either: the space run in front of the semicolon is part of
//! the tail, which is why `CREATE INDEX q3 ON t(a)   ;` stores as 26 bytes and
//! not 22.
//!
//! [`split_create_index_text`] performs that split on the raw statement text,
//! and [`IndexSpec::schema_sql`] is a fallback for a caller that has only the
//! parsed pieces rather than the source. It joins the columns with a bare `,`
//! because that is what a source the parser consumed would contain; see the doc
//! on [`IndexSpec::schema_sql`] for why it is not the preferred path.
//!
//! # Two things the reference does that this module does not
//!
//! Both are gaps in layers this module reads rather than writes, so they are
//! named here and pinned by `#[ignore]`d tests rather than fixed:
//!
//! * **A `DESC` column orders the cells.** The reference stores the key
//!   unchanged and writes the cells in descending order;
//!   `a_descending_index_is_written_in_descending_cell_order` records the bytes.
//!   This module's ordering comes from [`index::compare`](crate::index::compare),
//!   which consults no direction, so it writes both indexes ascending.
//! * **A `COLLATE` clause orders and matches the key.** The reference's index
//!   over a `NOCASE` column is stored in case-insensitive order, so a BINARY
//!   lookup both misses the row and reads the wrong part of the tree. The
//!   clause is parsed and then discarded by `parser.rs`, so there is nowhere for
//!   this module to put it, and the *semantics* are lost along with the text —
//!   not only the stored string.
//!
//! # Errors
//!
//! Every message here was read off sqlite3 3.53.4 rather than recalled, and
//! the result code matters as much as the text: `UNIQUE constraint failed` is
//! `SQLITE_CONSTRAINT` with extended code 2067 (`SQLITE_CONSTRAINT_UNIQUE`),
//! and a `DROP INDEX` of something that is not there is a plain `SQLITE_ERROR`
//! even though sqlite3's CLI labels it "Parse error".
//!
//! # What the connection has to call
//!
//! Nothing here touches [`crate::connection`], and the executor still has to be
//! wired. Each entry point names its call site: [`build_index`],
//! [`drop_index`], [`index_insert`], [`index_delete`] and [`index_update`].

use crate::catalog::{Catalog, Index, Table};
use crate::error::{Error, Result, ResultCode};
use crate::index::{IndexEntry, IndexLeaf};
use crate::index_interior::{IndexInterior, IndexTree};
use crate::page::page_type;
use crate::pager::Pager;
use crate::table_tree::TableTree;
use crate::value::Value;

/// `SQLITE_CONSTRAINT_UNIQUE`, the extended code the reference reports for
/// every `UNIQUE constraint failed`.
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

/// A statement's shape, before it has been checked against the catalog.
///
/// This is the parsed form plus the text to store, which is all the DDL needs
/// and keeps the catalog out of the free functions' signatures — a test can
/// build a spec without a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub name: String,
    pub table: String,
    /// The indexed columns in key order, paired with whether each ascends.
    pub columns: Vec<(String, bool)>,
    pub unique: bool,
    /// The text to store in `sqlite_schema`, already normalised the way the
    /// reference stores it. See [`IndexSpec::schema_sql`].
    pub sql: String,
}

impl IndexSpec {
    /// Builds a spec from a parsed `CREATE INDEX`.
    ///
    /// `name` is the name the statement gave, or the empty string when it gave
    /// none and the caller has to derive one. `sql` is the rendered text, which
    /// a caller may build with [`IndexSpec::schema_sql`] or supply itself when
    /// the statement carried a `COLLATE` or a `WHERE` the parser does not keep.
    pub fn new(
        name: &str,
        table: &str,
        columns: &[(String, bool)],
        unique: bool,
        sql: &str,
    ) -> IndexSpec {
        IndexSpec {
            name: name.to_owned(),
            table: table.to_owned(),
            columns: columns.to_vec(),
            unique,
            sql: sql.to_owned(),
        }
    }

    /// Renders the `sql` text the reference stores, from parsed pieces alone.
    ///
    /// This is the *fallback* path, for a caller that never had the statement's
    /// source text. [`sql_tail_after_name`] is the one to prefer, because the
    /// reference does not re-render the statement: it normalises the prefix
    /// through the index name and copies everything after that byte for byte,
    /// so a renderer that guesses at the tail will differ from the reference
    /// wherever the source spelled it differently. `CREATE INDEX i1 ON t(a , b)`
    /// is stored with that space, and this returns `t(a,b)`.
    ///
    /// The words come back in this order, each separated by one space, with the
    /// name and the table in whatever spelling the statement used and the
    /// columns joined by a bare comma — checked against 3.53.4, which stores
    /// `CREATE INDEX i ON t(a,b,c)` with no space after any of the commas.
    ///
    /// ```
    /// # use nsqlite::index_ddl::IndexSpec;
    /// let cols = vec![("a".to_string(), true)];
    /// assert_eq!(
    ///     IndexSpec::schema_sql("i1", "t", &cols, false),
    ///     "CREATE INDEX i1 ON t(a)"
    /// );
    /// assert_eq!(
    ///     IndexSpec::schema_sql("i2", "t", &cols, true),
    ///     "CREATE UNIQUE INDEX i2 ON t(a)"
    /// );
    /// ```
    pub fn schema_sql(name: &str, table: &str, columns: &[(String, bool)], unique: bool) -> String {
        let mut out = String::from("CREATE ");
        if unique {
            out.push_str("UNIQUE ");
        }
        out.push_str("INDEX ");
        out.push_str(name);
        out.push_str(" ON ");
        out.push_str(table);
        out.push('(');
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&col.0);
        }
        out.push(')');
        out
    }

    /// The index's key length, which is the arity every comparison needs.
    pub fn key_len(&self) -> usize {
        self.columns.len()
    }

    /// The index as the catalog records it, once it has a root page.
    pub fn to_index(&self, root_page: u32) -> Index {
        Index {
            name: self.name.clone(),
            table: self.table.clone(),
            columns: self.columns.iter().map(|(c, _)| c.clone()).collect(),
            ascending: self.columns.iter().map(|(_, a)| *a).collect(),
            unique: self.unique,
            root_page,
        }
    }
}

/// The name the reference derives for an unnamed index.
///
/// sqlite3 names an unnamed `CREATE INDEX ON t(a)` after the table, in the same
/// `sqlite_autoindex_<table>_1` shape a `UNIQUE` constraint gets. The name is
/// then reserved: `DROP INDEX` of it reports "index associated with UNIQUE or
/// PRIMARY KEY constraint cannot be dropped", because SQLite records an unnamed
/// index with an empty `sql` in the schema and refuses to drop any such row.
pub fn derived_index_name(table: &str) -> String {
    format!("sqlite_autoindex_{table}_1")
}

/// Whether a name is one of the reserved `sqlite_` names.
///
/// The prefix is compared as **bytes**, never as a `str` slice. A `&str` slice
/// has to land on a character boundary, and byte 7 of a name containing
/// non-ASCII text generally does not: `name[..7]` aborts the process on
/// `"日本語"`, `"ab日本"` and `"abc日本"`, so any `CREATE INDEX` over a table with
/// a CJK index name — which the reference accepts — killed the engine rather
/// than raising an error. The seven bytes are ASCII, so comparing the `&[u8]`
/// the prefix is anyway cannot split a character.
pub fn is_reserved(name: &str) -> bool {
    name.as_bytes()
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"sqlite_"))
}

/// Where a `CREATE INDEX` statement's prefix ends and its verbatim tail begins.
///
/// The reference normalises everything up to and including the index *name* —
/// `CREATE`, an optional `UNIQUE`, `INDEX`, and the name, one space between each,
/// with `IF NOT EXISTS` and any schema qualifier on the name dropped — and
/// copies the remainder of the statement straight out of the source. This is
/// the split, and it is [`sql_for_statement`] that applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexText {
    /// The name as the statement spelled it, quotes and all, which is what the
    /// normalised prefix is built from.
    pub name: String,
    /// Whether the statement declared the index UNIQUE.
    pub unique: bool,
    /// The statement from just past the name to its end, byte for byte, with
    /// the space run in front of it and any trailing comment kept.
    pub tail: String,
}

/// Splits a `CREATE INDEX` statement into its normalised prefix parts and the
/// verbatim tail that follows them.
///
/// `None` when `statement` is not a `CREATE [UNIQUE] INDEX` this can make sense
/// of, so a caller can fall back to [`IndexSpec::schema_sql`].
///
/// The three-space run before `ON` in `CREATE    INDEX   main.q2   ON   t(a)`
/// survives in the stored text while the `main.` does not, which is what places
/// the boundary exactly at the end of the name token rather than anywhere in
/// the tail. Checked against sqlite3 3.53.4:
///
/// ```text
/// CREATE    INDEX   i1   ON t(a)      -> CREATE INDEX i1   ON t(a)
/// CREATE    INDEX   main.q2   ON   t(a)
///                                      -> CREATE INDEX q2   ON   t(a)
/// CREATE INDEX q3 ON t(a)   ;         -> CREATE INDEX q3 ON t(a)
///                                      (the `;` goes, the spaces stay)
/// CREATE INDEX i9\nON t(a DESC, b COLLATE NOCASE)
///                                      -> unchanged, newline and all
/// ```
pub fn split_create_index_text(statement: &str) -> Option<CreateIndexText> {
    use crate::tokenizer::{Keyword, Punct, Token, Tokenizer};
    let tokens = Tokenizer::tokenize_all(statement).ok()?;
    // The prefix is `CREATE`, an optional `UNIQUE`, the word `INDEX`, an
    // optional `IF NOT EXISTS`, and then the name. The `IF NOT EXISTS` position
    // is the one that trips this up: SQLite's own grammar spells a `CREATE
    // INDEX` as `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ...`, and a
    // `CREATE TABLE` puts the clause *before* the name and before the object
    // kind. Read off 3.53.4, `CREATE INDEX IF NOT EXISTS i1 ON t(a)` stores as
    // `CREATE INDEX i1 ON t(a)` — the clause is after `INDEX`, and it is
    // dropped. Anything else in the run means this is not a statement this
    // handles, and guessing would store text the reference would not.
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum Stage {
        BeforeCreate,
        AfterCreate,
        AfterUnique,
        AfterIndex,
        AfterIf,
        AfterIfNot,
        AfterExists,
    }
    let mut stage = Stage::BeforeCreate;
    let mut unique = false;
    // The name may be schema-qualified — `main.q2` — and the reference stores
    // the bare name, so each segment is recorded and the last one is the name.
    // Once the name is complete a dot belongs to the *tail* (`ON main.t(a)`), so
    // only the dots between segments are consumed.
    let mut segments: Vec<(String, usize)> = Vec::new();
    let mut expect_segment = false;
    'tokens: for (token, span) in tokens {
        let next = match (stage, &token) {
            (Stage::BeforeCreate, Token::Keyword(Keyword::Create)) => Some(Stage::AfterCreate),
            (Stage::AfterCreate, Token::Keyword(Keyword::Unique)) => {
                unique = true;
                Some(Stage::AfterUnique)
            }
            (Stage::AfterCreate, Token::Keyword(Keyword::Index)) => {
                expect_segment = true;
                Some(Stage::AfterIndex)
            }
            (Stage::AfterUnique, Token::Keyword(Keyword::Index)) => {
                expect_segment = true;
                Some(Stage::AfterIndex)
            }
            (Stage::AfterIndex, Token::Keyword(Keyword::If)) => Some(Stage::AfterIf),
            (Stage::AfterIf, Token::Keyword(Keyword::Not)) => Some(Stage::AfterIfNot),
            (Stage::AfterIfNot, Token::Keyword(Keyword::Exists)) => Some(Stage::AfterExists),
            // A dot between two segments of the name continues it — `main.q2` is
            // one name spelled with a schema qualifier, and the reference stores
            // only `q2`. A dot after a *complete* name belongs to the tail, as in
            // `ON main.t(a)`, so the two are told apart by whether a segment is
            // still expected.
            (_, Token::Punct(Punct::Dot)) if stage == Stage::AfterIndex => {
                expect_segment = true;
                None
            }
            (_, _) if expect_segment => {
                segments.push((span_text(statement, span.start, span.end), span.end));
                expect_segment = false;
                None
            }
            // The first token after the name ends the name. It is the tail's
            // first token, and the tail is copied from the source, so it is not
            // parsed here.
            _ => break 'tokens,
        };
        if let Some(s) = next {
            stage = s;
        }
    }
    if !matches!(stage, Stage::AfterIndex | Stage::AfterExists) || segments.is_empty() {
        return None;
    }
    // The name is the last segment: `main.q2` is `q2`, which is also the one
    // the reference stores.
    let (name, name_end) = segments.last()?.clone();
    if name.is_empty() {
        return None;
    }
    // Everything from the end of the name token to the end of the statement,
    // with the trailing semicolon removed and nothing else changed: the space
    // run in front of `ON`, a newline, a `DESC`, a `COLLATE`, a `WHERE` and a
    // trailing comment all survive, because the reference copies them.
    let rest = statement.get(name_end..)?;
    let tail = rest.strip_suffix(';').unwrap_or(rest);
    // A statement that is only the name — `CREATE INDEX i` — has no tail at
    // all, and storing a stray space would be wrong, so an all-whitespace tail
    // becomes empty.
    let tail = if tail.trim().is_empty() {
        String::new()
    } else {
        tail.to_string()
    };
    Some(CreateIndexText { name, unique, tail })
}

/// The exact source text of one token's span, with its quotes left on.
///
/// The reference stores the name as the statement spelled it, so `CREATE INDEX
/// "My Idx" ON t(a)` keeps the quotes while the schema records the bare `My Idx`
/// as the index's name. A span is a byte range the tokenizer has already
/// validated, so this cannot fail; a range that somehow is not on character
/// boundaries yields nothing rather than aborting.
fn span_text(statement: &str, start: usize, end: usize) -> String {
    statement.get(start..end).unwrap_or_default().to_string()
}

/// Renders the `sql` column from a `CREATE INDEX` statement's own source text.
///
/// This is the path a `CREATE INDEX` should take, because it is the only one
/// that reproduces the reference byte for byte: the prefix is normalised and the
/// tail is copied out of the source. It answers `None` when the statement cannot
/// be split, and the caller falls back to [`IndexSpec::schema_sql`].
///
/// ```
/// # use nsqlite::index_ddl::sql_for_statement;
/// assert_eq!(
///     sql_for_statement("CREATE    INDEX   main.q2   ON   t(a)").as_deref(),
///     Some("CREATE INDEX q2   ON   t(a)")
/// );
/// // `IF NOT EXISTS` and interior whitespace in the prefix go; the tail does not.
/// assert_eq!(
///     sql_for_statement("CREATE    INDEX IF NOT EXISTS i1   ON t(a)").as_deref(),
///     Some("CREATE INDEX i1   ON t(a)")
/// );
/// // A trailing semicolon is dropped and everything else is kept, including the
/// // spaces in front of it: 3.53.4 stores that statement as 26 bytes.
/// assert_eq!(
///     sql_for_statement("CREATE INDEX q3 ON t(a)   ;").as_deref(),
///     Some("CREATE INDEX q3 ON t(a)   ")
/// );
/// // A newline and a DESC are the statement's own bytes, kept as they are.
/// assert_eq!(
///     sql_for_statement("CREATE INDEX i9\nON t(a DESC)").as_deref(),
///     Some("CREATE INDEX i9\nON t(a DESC)")
/// );
/// ```
pub fn sql_for_statement(statement: &str) -> Option<String> {
    let split = split_create_index_text(statement)?;
    let mut out = String::from("CREATE ");
    if split.unique {
        out.push_str("UNIQUE ");
    }
    out.push_str("INDEX ");
    out.push_str(&split.name);
    out.push_str(&split.tail);
    Some(out)
}

/// The `UNIQUE constraint failed: t.a, t.b` error, with the reference's
/// extended code.
///
/// The tuple of columns is part of the message, which is why it is built here
/// and not by the b-tree: that layer knows only the entries. The error a
/// `CREATE UNIQUE INDEX` over duplicate data raises and the error a later
/// `INSERT` raises are the same one, so this is shared rather than built twice.
pub fn unique_violation(table: &str, columns: &[String]) -> Error {
    Error::new(
        ResultCode::Constraint,
        unique_violation_message(table, columns),
    )
    .with_extended(SQLITE_CONSTRAINT_UNIQUE)
}

/// The `UNIQUE constraint failed: t.a, t.b` message, on its own.
pub fn unique_violation_message(table: &str, columns: &[String]) -> String {
    let mut msg = String::from("UNIQUE constraint failed: ");
    for (i, c) in columns.iter().enumerate() {
        if i > 0 {
            msg.push_str(", ");
        }
        msg.push_str(table);
        msg.push('.');
        msg.push_str(c);
    }
    msg
}

/// Resolves the indexed columns against a table, reporting the reference's
/// error for a column the table does not have.
///
/// Checked against sqlite3 3.53.4: `CREATE INDEX index1 ON test1(f4)` is
/// `no such column: f4` — the bare column name, with no table prefix, unlike
/// the `no such table` a missing table gets.
pub fn resolve_columns(table: &Table, columns: &[(String, bool)]) -> Result<Vec<usize>> {
    let mut out = Vec::with_capacity(columns.len());
    for (col, _) in columns {
        let idx = table
            .column_index(col)
            .ok_or_else(|| Error::new(ResultCode::Error, format!("no such column: {col}")))?;
        out.push(idx);
    }
    Ok(out)
}

/// The index entry for one row.
///
/// `columns` is the resolved column list from [`resolve_columns`], and `table`
/// supplies the rowid-alias position: a row stored with its `INTEGER PRIMARY
/// KEY` column written as NULL still indexes that column under the rowid, which
/// is what the reference does. Checked against 3.53.4 on
/// `CREATE TABLE t(a INTEGER PRIMARY KEY, b)` with rows `(5,50)` and `(9,90)`:
/// the index over `a` holds records `01 01 05 05` and `01 01 09 09` — the key
/// is 5 and 9, not NULL.
///
/// `rowid` is the rowid the entry points at, and it fills *both* the trailing
/// slot and the rowid-alias key. Those are the same number, which is why they
/// are one parameter rather than two: an `UPDATE` of an `INTEGER PRIMARY KEY`
/// moves the row, so the entry that replaces the old one is keyed and stamped
/// by the row's *new* rowid, while the entry being removed is stamped by the
/// old one. Two parameters would let those two numbers drift apart, and the
/// resulting entry is not a wrong answer so much as an entry pointing at a row
/// that does not exist.
///
/// A column is stored as the reference stores it, which is to say unchanged.
/// A descending column is *not* stored inverted: checked against 3.53.4,
/// `CREATE INDEX di ON t(a DESC)` over the rows 1, 2 and 3 holds the records
/// `01 01 01 01`, `01 01 02 02` and `01 01 03 03` — identical to the ascending
/// index over the same column — and differs only in the order the cells sit
/// in. A key that was inverted on the way in would produce a file the
/// reference reads with the wrong ordering. `spec.ascending` is therefore not
/// consulted here.
///
/// **The cell order itself is not reproduced yet.** The reference writes that
/// `DESC` index's cells in descending key order — on the rows 30, 10, 20 its
/// cell-pointer array is `[4091, 4080, 4086]`, holding 30, 20, 10 — while this
/// module writes both indexes ascending and byte-identical, because
/// [`index::compare`](crate::index::compare) orders by value then rowid and
/// consults no direction. [`Index::compare_keys`] is the comparator that does,
/// and it has no callers; wiring it into the b-tree's ordering is the fix, and
/// it lands in `index.rs`/`index_interior.rs` rather than here. The test
/// `a_descending_index_is_written_in_descending_cell_order` is `#[ignore]`d
/// until then.
pub fn entry_for(
    spec: &Index,
    columns: &[usize],
    table: &Table,
    rowid: i64,
    values: &[Value],
) -> IndexEntry {
    // A column the index names but the row does not have is NULL rather than
    // an error: the row is older than the schema. `spec.columns` is the
    // authority on the arity, and a mismatch with the resolved list means the
    // caller has not resolved it, which is a bug rather than a corrupt file.
    debug_assert_eq!(spec.columns.len(), columns.len());
    let key: Vec<Value> = columns
        .iter()
        .map(|&c| {
            if Some(c) == table.rowid_alias {
                Value::Integer(rowid)
            } else {
                values.get(c).cloned().unwrap_or(Value::Null)
            }
        })
        .collect();
    IndexEntry { key, rowid }
}

/// Whether two entries collide under a UNIQUE index.
///
/// Two keys collide when every column of the key compares equal, which for an
/// index means equal *and* the same direction on every column. A NULL never
/// equals anything, not even another NULL, so a key holding one cannot
/// collide. Checked against 3.53.4: on `CREATE UNIQUE INDEX u ON t(a)` two
/// `NULL`s are accepted, and on `CREATE UNIQUE INDEX u ON t(a,b)` both
/// `(1,NULL)` and `(1,2)` are accepted while a second `(1,2)` is refused.
pub fn collides(spec: &Index, a: &IndexEntry, b: &IndexEntry) -> bool {
    if !spec.unique {
        return false;
    }
    let n = spec.key_len();
    if n == 0 || a.key.len() < n || b.key.len() < n {
        return false;
    }
    for i in 0..n {
        let (x, y) = (&a.key[i], &b.key[i]);
        if x.is_null() || y.is_null() {
            return false;
        }
        if x.compare(y) != std::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}

/// Creates an index over a table and records it in the catalog.
///
/// The pages are allocated first and the table is walked, and the schema row is
/// written by the *caller* only once this returns cleanly — so a `UNIQUE` index
/// that finds a duplicate has to have put every page it allocated back on the
/// freelist itself. That is what the reference does, and the size of the
/// database shows it: checked against sqlite3 3.53.4, a `CREATE UNIQUE INDEX`
/// over 2000 distinct rows and a trailing duplicate leaves `page_count` at 7
/// and `freelist_count` at 0, before and after — not one orphaned page. The
/// root is allocated before the walk rather than after it so that a failure can
/// free a whole tree, and [`discard_partial_index`] is what does.
///
/// # The call the connection has to make
///
/// In `Stmt::CreateIndex { name, table, columns, unique, if_not_exists }`,
/// replace the `"CREATE INDEX is not supported yet"` arm with:
///
/// ```ignore
/// self.create_index(name.as_deref(), table, columns, *unique, *if_not_exists)?
/// ```
///
/// and add to `connection.rs`:
///
/// ```ignore
/// fn create_index(
///     &mut self,
///     name: Option<&str>,
///     table_name: &str,
///     columns: &[(String, bool)],
///     unique: bool,
///     if_not_exists: bool,
/// ) -> Result<Outcome> {
///     let index_name = name.map(str::to_owned)
///         .unwrap_or_else(|| crate::index_ddl::derived_index_name(table_name));
///     // The statement's own text is what gets stored, because the reference
///     // copies the tail of the statement out of the source rather than
///     // re-rendering it. Fall back to the renderer when the text is not to
///     // hand, which is what a synthesised statement has to do:
///     //   let sql = sql_for_statement(&statement).unwrap_or_else(|| {
///     //       IndexSpec::schema_sql(&index_name, table_name, columns, unique)
///     //   });
///     let spec = IndexSpec::new(&index_name, table_name, columns, unique, &sql);
///     let result = crate::index_ddl::build_index(
///         &mut self.pager, &self.catalog, &index_name, table_name, columns,
///         unique, if_not_exists,
///     );
///     ...
/// }
/// ```
///
/// `build_index` returns `Ok(None)` for a no-op (`IF NOT EXISTS` over a name
/// that already exists), so the caller can skip the schema write entirely.
///
/// Five of the checks are about the catalog rather than the file, and each has
/// a *different* message, which is the part most easily got wrong. All read off
/// 3.53.4:
///
/// ```text
/// CREATE INDEX i1 ON t(a)   where i1 is a table  -> there is already a table named i1
/// CREATE INDEX i1 ON t(a)   where i1 is an index -> index i1 already exists
/// CREATE TABLE  i1(a)       where i1 is an index -> there is already an index named i1
/// CREATE INDEX i1 ON sqlite_master(name)        -> table sqlite_master may not be indexed
/// CREATE INDEX sqlite_i1 ON t(a)                 -> object name reserved for internal use: sqlite_i1
/// CREATE INDEX i1 ON nosuch(a)                  -> no such table: main.nosuch
/// ```
///
/// The third is not in this function: it belongs in `create_table`, which
/// today only checks `catalog.contains(name)` and so lets a table take an
/// index's name. `create_table` needs one extra line:
///
/// ```ignore
/// if self.catalog.index(name).is_some() {
///     return Err(Error::new(ResultCode::Error,
///         format!("there is already an index named {name}")));
/// }
/// ```
/// Builds an index the ENGINE is generating rather than the user.
///
/// The only caller is the implicit index a `UNIQUE` constraint or a non-alias
/// `PRIMARY KEY` gets, whose name is `sqlite_autoindex_<table>_<n>`. That name
/// is reserved, and rightly so: `CREATE INDEX sqlite_autoindex_t_1 ON t(a)` is
/// `object name reserved for internal use` on the reference and here, and a
/// user must not be able to squat the name.
///
/// So the reserved-name check is skipped for exactly the case where the name
/// was generated by the engine that owns the namespace, and kept everywhere
/// else. The name is not *passed* in by the caller, either -- it is derived from
/// the table and a number here, so a caller cannot smuggle in a reserved name
/// it made up.
pub fn build_implicit_index(
    pager: &mut Pager,
    catalog: &Catalog,
    table_name: &str,
    ordinal: usize,
    columns: &[(String, bool)],
) -> Result<Option<Index>> {
    let name = format!("sqlite_autoindex_{table_name}_{ordinal}");
    build_index_inner(pager, catalog, &name, table_name, columns, true, false, true)
}

/// Builds an index a statement asked for.
///
/// `engine_generated` is false here and only true from
/// [`build_implicit_index`]; see that function for why the reserved-name check
/// is conditional.
pub fn build_index(
    pager: &mut Pager,
    catalog: &Catalog,
    name: &str,
    table_name: &str,
    columns: &[(String, bool)],
    unique: bool,
    if_not_exists: bool,
) -> Result<Option<Index>> {
    build_index_inner(
        pager, catalog, name, table_name, columns, unique, if_not_exists, false,
    )
}

fn build_index_inner(
    pager: &mut Pager,
    catalog: &Catalog,
    name: &str,
    table_name: &str,
    columns: &[(String, bool)],
    unique: bool,
    if_not_exists: bool,
    engine_generated: bool,
) -> Result<Option<Index>> {
    // `IF NOT EXISTS` suppresses the error only when the name is already an
    // *index*. It does not reach a table: `CREATE INDEX IF NOT EXISTS t ON t(a)`
    // is still `there is already a table named t`, and both that and the
    // `IF NOT EXISTS` form were read off 3.53.4 with `rc=1`. Extending the
    // suppression to a table collision made this a silent no-op where the
    // reference errors, which is a DDL statement the test suite exercises.
    if catalog.index(name).is_some() {
        if if_not_exists {
            return Ok(None);
        }
        return Err(Error::new(
            ResultCode::Error,
            format!("index {name} already exists"),
        ));
    }
    if catalog.contains(name) {
        return Err(Error::new(
            ResultCode::Error,
            format!("there is already a table named {name}"),
        ));
    }
    if !engine_generated && is_reserved(name) {
        return Err(Error::new(
            ResultCode::Error,
            format!("object name reserved for internal use: {name}"),
        ));
    }
    if is_schema_table_name(table_name) {
        return Err(Error::new(
            ResultCode::Error,
            format!("table {} may not be indexed", base_name(table_name)),
        ));
    }
    // A table that does not exist is reported with the schema spelled out,
    // because that is what the name resolves to: `CREATE INDEX i1 ON nosuch(a)`
    // is "no such table: main.nosuch" in 3.53.4.
    let table = catalog.get(base_name(table_name)).ok_or_else(|| {
        Error::new(
            ResultCode::Error,
            format!("no such table: main.{}", base_name(table_name)),
        )
    })?;
    if columns.is_empty() {
        return Err(Error::new(
            ResultCode::Error,
            format!("index {name} has no columns"),
        ));
    }
    let cols = resolve_columns(table, columns)?;

    // The root is allocated and written as an empty leaf before anything is
    // inserted, because a page of zeroes reads as a freelist trunk and the
    // first insert would reject it as being the wrong page type.
    let root = pager.allocate()?;
    let key_len = cols.len();
    IndexTree::create(pager, root, key_len)?;

    let spec = Index {
        name: name.to_owned(),
        table: table.name.clone(),
        columns: columns.iter().map(|(c, _)| c.clone()).collect(),
        ascending: columns.iter().map(|(_, a)| *a).collect(),
        unique,
        root_page: root,
    };

    // Building is a walk of the table and one insert per row. The walk is in
    // rowid order and the tree sorts each entry, so the order the rows arrive
    // in does not matter to the result — only that every row gets exactly one
    // entry.
    let rows = {
        let mut tree = TableTree::open(pager, table.root_page)?.with_rowid_alias(table.rowid_alias);
        tree.scan(pager)?
    };
    for row in &rows {
        let entry = entry_for(&spec, &cols, table, row.rowid, &row.values);
        // `insert_one` enforces uniqueness on the key, ignoring the rowid, which
        // is what the reference does: a non-unique index holds one entry per
        // row whatever the key, and a UNIQUE one holds at most one entry per
        // distinct non-NULL key.
        //
        // A failure part-way through leaves a whole half-built tree behind, and
        // `Pager::allocate` never consults the freelist, so those pages would
        // never be handed out again for the life of the file. Freeing the tree
        // first is what keeps a refused `CREATE UNIQUE INDEX` from growing the
        // database: measured on 2000 distinct rows and a trailing duplicate,
        // the refusal went from 10 pages to 26 and stranded 16, against the
        // reference's 7 pages and a freelist of 0.
        if let Err(e) = insert_one(pager, table, &spec, &entry) {
            // The page count is recorded before the free, so the file itself
            // keeps the pages its pages did not shrink. The reference behaves
            // the same way: a database never loses a page, it puts one on the
            // freelist. What differs is only that `Pager::allocate` does not
            // yet take from that freelist, so the pages are stranded rather
            // than reused.
            let _ = free_index_tree(pager, root);
            return Err(e);
        }
    }

    Ok(Some(spec))
}

/// Whether `entry`'s key is already held by a different row of a UNIQUE index.
///
/// The whole tree is walked rather than descended, because a key a split
/// promoted into an interior cell is in no leaf and a descent would miss it —
/// the same reason [`IndexTree::lookup`] walks. `lookup` returns rowids, so
/// the entry is rebuilt around one of them to be compared as a whole.
fn find_colliding(
    pager: &mut Pager,
    root: u32,
    key_len: usize,
    entry: &IndexEntry,
    spec: &Index,
) -> Option<IndexEntry> {
    // A key holding a NULL cannot collide with anything, so the walk is
    // skipped for one rather than run and discarded. This is not only a
    // shortcut: `IndexTree::lookup` matches on the key, and a NULL probe does
    // find the other NULL-keyed rows, so the walk would return them and the
    // `collides` test below would have to reject each one.
    if !spec.unique || entry.key.iter().any(|v| v.is_null()) {
        return None;
    }
    let rowids = IndexTree::open(pager, root, key_len)
        .lookup(&entry.key)
        .ok()?;
    // The row that is being written again is not a collision with itself: an
    // UPDATE that leaves the key alone re-inserts the same entry, and the
    // b-tree's own check is what refuses a genuine double write.
    rowids
        .iter()
        .find(|r| **r != entry.rowid)
        .map(|r| IndexEntry {
            key: entry.key.clone(),
            rowid: *r,
        })
}

/// Removes an index: frees its tree, its overflow chains, and its schema row.
///
/// The reference does the same in `sqlite3DropIndex`, which nests a
/// `DELETE FROM sqlite_schema WHERE name=? AND type='index'` and then an
/// `OP_Destroy` — the latter being `sqlite3BtreeDropTable`, which frees the
/// whole tree.
///
/// # The call the connection has to make
///
/// The parser turns `DROP INDEX` into `Stmt::Unsupported("drop index")` today,
/// so a real arm has to come first. Add to `parser.rs`:
///
/// ```ignore
/// DropIndex { name: String, if_exists: bool },
/// ```
///
/// and have `drop()` return it in place of the `skip_to_semicolon` fallback.
/// Then in `connection.rs`:
///
/// ```ignore
/// Stmt::DropIndex { name, if_exists } => self.drop_index(name, *if_exists),
/// ```
///
/// ```ignore
/// fn drop_index(&mut self, name: &str, if_exists: bool) -> Result<Outcome> {
///     match crate::index_ddl::drop_index(&mut self.pager, &self.catalog, name, if_exists)? {
///         Some(index) => {
///             self.remove_index_schema_row(&index.name)?;
///             self.pager.flush()?;
///             Ok(Outcome::Changed(0))
///         }
///         None => Ok(Outcome::Changed(0)),
///     }
/// }
/// ```
///
/// `remove_index_schema_row` is `remove_schema_row` narrowed to the index's
/// row. The reference deletes `WHERE name=? AND type='index'`, so a drop can
/// never remove a table of the same name; `remove_schema_row` as written
/// matches on `name` alone. That is a no-op difference today — the names share
/// one namespace — but it is the wrong shape if that ever changes, so the
/// narrowed version should be added rather than reused as-is.
///
/// It also has to bump `schema_cookie` and the change counter, which
/// `remove_schema_row` already does.
///
/// # Why a no-op returns `Ok(None)`
///
/// `DROP INDEX IF EXISTS nosuch` succeeds and changes nothing, checked against
/// 3.53.4, and must not allocate or free a page or write a schema row.
pub fn drop_index(
    pager: &mut Pager,
    catalog: &Catalog,
    name: &str,
    if_exists: bool,
) -> Result<Option<Index>> {
    let Some(index) = catalog.index(name).cloned() else {
        if if_exists {
            return Ok(None);
        }
        // A qualified name is reported as written: `DROP INDEX main.nosuch` is
        // "no such index: main.nosuch", not "... nosuch".
        return Err(Error::new(
            ResultCode::Error,
            format!("no such index: {name}"),
        ));
    };
    // An index SQLite made for a UNIQUE or PRIMARY KEY constraint is part of
    // that constraint and cannot be dropped on its own, and `IF EXISTS` does
    // *not* suppress the refusal. Both read off 3.53.4. The `sqlite_` prefix
    // is what identifies one: the reference decides from the schema row's
    // empty `sql`, which every automatically created index has.
    if is_reserved(index.name.as_str()) {
        return Err(Error::new(
            ResultCode::Error,
            "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped",
        ));
    }
    if index.root_page > 1 {
        free_index_tree(pager, index.root_page)?;
    }
    Ok(Some(index))
}

/// Whether a name is the schema table's, in any of its spellings.
///
/// `sqlite_master`, `sqlite_schema` and a `main.`- or `temp.`-qualified form
/// of either all name the same table, and none may be indexed. Checked against
/// 3.53.4, which answers "table sqlite_master may not be indexed" for both
/// spellings.
pub fn is_schema_table_name(name: &str) -> bool {
    const NAMES: [&str; 4] = [
        "sqlite_master",
        "sqlite_schema",
        "main.sqlite_master",
        "main.sqlite_schema",
    ];
    let base = base_name(name);
    NAMES.iter().any(|n| base.eq_ignore_ascii_case(n))
}

/// The base of a possibly schema-qualified name, for a `no such table: main.X`
/// message.
fn base_name(name: &str) -> &str {
    crate::join::strip_schema_qualifier(name).unwrap_or(name)
}

/// Frees every page an index tree owns, and every page its cells' overflow
/// chains own.
///
/// The overflow chains are released *first*, per cell, while the cell is still
/// readable; the b-tree pages are freed afterwards in a second pass. Doing both
/// in one pass would free a child page before its parent's cells were read.
///
/// A page of the tree is found by walking down from the root, so a whole
/// subtree goes rather than just the root — the reference frees the lot, and a
/// partially-freed index leaks pages the file never reuses. The walk is
/// iterative so a deep tree cannot overflow the stack, and it checks for a
/// repeated page because a corrupt file could name one twice and freeing it
/// twice would put it on the freelist twice.
pub fn free_index_tree(pager: &mut Pager, root: u32) -> Result<()> {
    let mut seen: Vec<u32> = Vec::new();
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        if seen.contains(&page_no) {
            return Err(Error::corrupt(format!(
                "page {page_no} appears twice in the index b-tree"
            )));
        }
        seen.push(page_no);
        let page = pager.read_page(page_no)?;
        if page.len() < 12 {
            return Err(Error::corrupt("index page is truncated"));
        }
        let kind = page[0];
        if kind == page_type::INDEX_LEAF {
            let leaf = IndexLeaf::read(pager, page_no)?;
            for head in leaf.overflows.iter().copied() {
                free_chain(pager, head)?;
            }
        } else if kind == page_type::INDEX_INTERIOR {
            let interior = IndexInterior::read(pager, page_no)?;
            for cell in interior.cells.iter() {
                free_chain(pager, cell.overflow)?;
                stack.push(cell.left_child);
            }
            stack.push(interior.rightmost);
        } else {
            return Err(Error::corrupt(format!(
                "page {page_no} is a {} where an index page was expected",
                page_type::name(kind)
            )));
        }
    }
    for page_no in seen {
        pager.free(page_no)?;
    }
    Ok(())
}

/// Frees an overflow chain, head page included.
///
/// This does not call [`crate::btree_write::free_overflow_chain`], which has an
/// off-by-one: it reads a page, takes the *following* page number out of it,
/// and frees that instead of the page it just read. A two-page chain therefore
/// frees the tail, leaks the head, and never reaches the end of the walk, so
/// the caller sees a chain that is still open. Checked directly on a 6000-byte
/// spill over 4096-byte pages: a chain of `[2, 3]` freed one page and left
/// `freelist_count` at 1 rather than 2.
///
/// That is a bug in a module this one does not own, and the fix is one line
/// there — free `next` *before* reassigning it. Until then this does it
/// correctly, because an index drop that leaks a page per wide key is a silent
/// file-size regression rather than an error.
fn free_chain(pager: &mut Pager, head: u32) -> Result<()> {
    if head == 0 {
        return Ok(());
    }
    let limit = pager.page_count() as usize + 1;
    let mut next = head;
    let mut seen = 0usize;
    while next != 0 {
        seen += 1;
        if seen > limit {
            return Err(Error::corrupt("overflow chain is cyclic"));
        }
        let page = pager.read_page(next)?;
        if page.len() < 4 {
            return Err(Error::corrupt("overflow page is truncated"));
        }
        // The successor is taken out *before* the page is freed, because
        // freeing overwrites the first four bytes with the freelist link.
        let following = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        pager.free(next)?;
        next = following;
    }
    Ok(())
}

/// Adds the index entries for one inserted row.
///
/// Called by the connection **after** the row itself is written, so a
/// constraint failure leaves the two in step. If the index refuses the entry
/// the row has to be taken back out again; the reference rolls the insert back
/// in the same way.
///
/// # The call the connection has to make
///
/// In `Connection::insert_row`, right after `tree.insert(...)` succeeds:
///
/// ```ignore
/// crate::index_ddl::index_insert(&mut self.pager, &self.catalog, &table, rowid, &values)?;
/// ```
///
/// `insert_row` already receives `table: &Table` and `values`, so both are in
/// hand. The catalog borrow and the pager borrow are two fields of `self` and
/// do not conflict.
///
/// If the caller wants the row gone when this fails, the order has to be
/// *check first, then write*, which is what the reference does — it probes the
/// index for a conflict before it touches either. This function inserts as it
/// goes, so a failure on the second index of a table leaves the first one's
/// entry in place. The caller should undo the row, and this module's
/// [`index_delete`] is the matching undo.
pub fn index_insert(
    pager: &mut Pager,
    catalog: &Catalog,
    table: &Table,
    rowid: i64,
    values: &[Value],
) -> Result<()> {
    for index in catalog.indexes_on(&table.name) {
        let index = index.clone();
        let Some(cols) = resolved_for(table, &index) else {
            continue;
        };
        let entry = entry_for(&index, &cols, table, rowid, values);
        insert_one(pager, table, &index, &entry)?;
    }
    Ok(())
}

/// Removes the index entries for one deleted row.
///
/// # The call the connection has to make
///
/// In `Connection::delete`, for each removed row, **before** `tree.remove`:
///
/// ```ignore
/// crate::index_ddl::index_delete(&mut self.pager, &self.catalog, &table, row.rowid, &row.values)?;
/// ```
///
/// The *old* values have to be passed, not new ones: the key to remove is the
/// key the row had before the delete, and the rowid alone is not enough for a
/// non-unique index holding several rows under one key.
pub fn index_delete(
    pager: &mut Pager,
    catalog: &Catalog,
    table: &Table,
    rowid: i64,
    values: &[Value],
) -> Result<()> {
    for index in catalog.indexes_on(&table.name) {
        let index = index.clone();
        let Some(cols) = resolved_for(table, &index) else {
            continue;
        };
        let entry = entry_for(&index, &cols, table, rowid, values);
        remove_entry(pager, index.root_page, index.key_len(), &entry)?;
    }
    Ok(())
}

/// Replaces one row's index entries after an UPDATE.
///
/// Both halves run: the new key's entry is added and the old one removed. That
/// is not a shortcut — a key that changed with the rowid left alone still
/// leaves a stale entry behind if the value half is skipped, and a stale entry
/// gives a *wrong answer* to a query rather than an error.
///
/// **The insert comes first, and that order is the whole point.** Removing the
/// old entry first would make a uniqueness violation undetectable: the
/// conflicting entry is a *different row's*, so it survives either way — but
/// with delete-then-insert the index has already lost the row being updated, so
/// a collision with a row that sorts nearby is missed or the wrong entry goes.
/// Checked against sqlite3 3.53.4:
///
/// ```text
/// CREATE TABLE t(a); CREATE UNIQUE INDEX u ON t(a);
/// INSERT INTO t VALUES(1); INSERT INTO t VALUES(2);
/// UPDATE t SET a=2 WHERE a=1;
///   -> UNIQUE constraint failed: t.a
///   -> SELECT a FROM t is still `1, 2`: nothing was changed
/// ```
///
/// So this refuses *before* removing anything, and on refusal the index still
/// describes the table exactly.
///
/// **The same holds across indexes, not just within one.** The insert-before-
/// remove order above is applied per index, so on its own it left the index set
/// half-updated when a *later* index refused: with `i1` on `t(a)` and a UNIQUE
/// `i2` on `t(b)`, updating rowid 1 from `(1,10)` to `(3,20)` raised
/// `UNIQUE constraint failed: t.b` with `i1` already holding a phantom `([3],1)`
/// for a row whose value was still `1`. So every index is probed with
/// [`check_unique`] before any of them is written to, and the writes only
/// begin once all of them have passed. Checked against 3.53.4, which leaves the
/// whole set describing the unchanged table: `SELECT rowid,a FROM t INDEXED BY
/// i1 WHERE a=3` returns 0 rows and `i1` still reads `1|1, 2|2`.
///
/// # The call the connection has to make
///
/// In `Connection::update`, replace the `tree.remove` / `insert_row` pair with:
///
/// ```ignore
/// crate::index_ddl::index_update(
///     &mut self.pager,
///     &self.catalog,
///     &table,
///     row.rowid,   // the old rowid
///     &row.values, // the old values
///     new_rowid,   // the new rowid, which differs when the alias column moved
///     &new,        // the new values
/// )?;
/// ```
///
/// and when it returns an error the row must be left as it was — which it is,
/// because nothing is removed until the insert has succeeded.
///
/// `index_update` removes every old entry for the row and adds one for the new
/// key, so it is correct whether or not the key changed and whether or not the
/// rowid did.
pub fn index_update(
    pager: &mut Pager,
    catalog: &Catalog,
    table: &Table,
    old_rowid: i64,
    old_values: &[Value],
    new_rowid: i64,
    new_values: &[Value],
) -> Result<()> {
    // Indexes the update touches: those whose key the old and new values
    // disagree on, plus — for the index over the rowid alias — the one whose
    // key moved because the rowid did. An index the update does not touch is
    // skipped rather than removed and re-added, so a multi-index table is not
    // rewritten wholesale on every UPDATE.
    let mut plan: Vec<(Index, Vec<usize>, IndexEntry, IndexEntry)> = Vec::new();
    for index in catalog.indexes_on(&table.name) {
        let Some(cols) = resolved_for(table, index) else {
            continue;
        };
        let old_entry = entry_for(index, &cols, table, old_rowid, old_values);
        let new_entry = entry_for(index, &cols, table, new_rowid, new_values);
        if old_entry == new_entry {
            // Neither the key nor the rowid this index stores moved, so its
            // entry is already correct and the rewrite can be skipped.
            continue;
        }
        plan.push((index.clone(), cols, old_entry, new_entry));
    }
    // Every uniqueness check happens before any index is written to. The
    // ordering *within* one index is right — the new entry goes in before the
    // old one comes out, so a violation is raised while the old entry is still
    // there — but doing that index by index left the set half-updated whenever a
    // later index refused: with `i1` on `t(a)` and a UNIQUE `i2` on `t(b)`,
    // updating rowid 1 from `(1,10)` to `(3,20)` returned `UNIQUE constraint
    // failed: t.b` with `i1` already holding a phantom `([3], 1)` for a row
    // whose value was still 1, and the old `[1]` entry already gone. That is a
    // *wrong query answer* rather than an error, and the caller only ever sees
    // the error, so it cannot repair it. The reference probes every index
    // first and leaves the whole set describing the unchanged table, which is
    // what this does.
    for (index, _, _, new_entry) in &plan {
        check_unique(pager, table, index, new_entry)?;
    }
    for (index, _, old_entry, new_entry) in &plan {
        insert_one(pager, table, index, new_entry)?;
        remove_entry(pager, index.root_page, index.key_len(), old_entry)?;
    }
    Ok(())
}

/// Inserts one entry into one index, enforcing uniqueness on the *key*.
///
/// The b-tree's own duplicate check is on the whole entry, key **and** rowid:
/// inserting `(1, rowid 5)` twice is refused, but `(1, rowid 5)` followed by
/// `(1, rowid 6)` is two perfectly good entries — and in a *non-unique* index
/// that is exactly right, since several rows may share a key.
///
/// For a UNIQUE index it is the key alone that must be distinct, so the check
/// happens here. The lookup is the same walk `IndexTree::lookup` does, and it
/// ignores the rowid: an entry whose key matches and whose rowid differs is the
/// collision, and an entry whose key *and* rowid both match is the same row
/// being written twice, which the b-tree already catches.
///
/// A NULL anywhere in the key exempts it, so the walk is skipped for a key that
/// has one — see [`collides`].
/// Raises the UNIQUE violation `entry` would cause, if it would cause one.
///
/// This is the probe [`index_update`] makes over every index before it writes to
/// any of them, and it is split out of [`insert_one`] so the two agree on what
/// counts as a conflict. A UNIQUE index is the only one that can refuse, and a
/// key holding a NULL cannot, so a non-unique index and a NULL key both return
/// without reading a page.
pub fn check_unique(
    pager: &mut Pager,
    table: &Table,
    index: &Index,
    entry: &IndexEntry,
) -> Result<()> {
    if find_colliding(pager, index.root_page, index.key_len(), entry, index).is_some() {
        return Err(unique_violation(&table.name, &index.columns));
    }
    Ok(())
}

/// Inserts one entry into one index, enforcing uniqueness on the *key*.
///
/// The b-tree's own duplicate check is on the whole entry, key **and** rowid:
/// inserting `(1, rowid 5)` twice is refused, but `(1, rowid 5)` followed by
/// `(1, rowid 6)` is two perfectly good entries — and in a *non-unique* index
/// that is exactly right, since several rows may share a key.
///
/// For a UNIQUE index it is the key alone that must be distinct, so the check
/// happens here, in [`check_unique`]. The lookup is the same walk
/// `IndexTree::lookup` does, and it ignores the rowid: an entry whose key
/// matches and whose rowid differs is the collision, and an entry whose key
/// *and* rowid both match is the same row being written twice, which the b-tree
/// already catches.
fn insert_one(pager: &mut Pager, table: &Table, index: &Index, entry: &IndexEntry) -> Result<()> {
    check_unique(pager, table, index, entry)?;
    let mut tree = IndexTree::open(pager, index.root_page, index.key_len());
    match tree.insert(entry) {
        Ok(()) => Ok(()),
        // The b-tree's own refusal is "UNIQUE constraint failed: index", which
        // names nothing. The reference names the columns, and the catalog is
        // the only layer that knows them.
        Err(e) if e.code == ResultCode::Constraint => {
            Err(unique_violation(&table.name, &index.columns))
        }
        Err(e) => Err(e),
    }
}

/// The resolved column positions of an index over `table`, or `None` when the
/// index names a column the table no longer has — a schema that has drifted,
/// which a rebuild would fix and a single write cannot.
fn resolved_for(table: &Table, index: &Index) -> Option<Vec<usize>> {
    let mut out = Vec::with_capacity(index.columns.len());
    for c in &index.columns {
        out.push(table.column_index(c)?);
    }
    Some(out)
}

/// Removes one exact entry — key *and* rowid — from the tree rooted at `root`.
///
/// The removal is done **in place**: descend to the one page that holds the
/// entry, rewrite that page without it, and drop that page from its parent only
/// if the deletion emptied it. It touches `O(log n)` pages rather than rewriting
/// the whole index, and that is not a performance nicety — it is the difference
/// between a file that stays the size it was and one that grows without bound.
/// The whole-tree rewrite this replaces allocated a fresh copy of the entire
/// index for every single row and then abandoned the old pages, and because
/// [`Pager::allocate`] never consults the freelist those pages were never handed
/// out again for the life of the database.
///
/// Measured on 500 rows in one index: 25 deletes took `page_count` from 8 to
/// 108 and put 125 pages on the freelist, and 100 deletes from 1000 rows took
/// 8.6 seconds and left 814 pages on the freelist. Real sqlite3 3.53.4, on the
/// same schema and the same deletes, leaves the file at 28672 bytes with
/// `page_count` 7 and `freelist_count` 0, because its delete removes a cell
/// from one leaf and stops.
///
/// # An entry lives in one of three places
///
/// A split **promotes** the largest key of the left half out of the leaves and
/// into the interior cell, so a cell holds a real entry of its own. That is why
/// a delete has three shapes rather than one, and why dropping a cell is not the
/// same as dropping a leaf cell:
///
/// * **A leaf cell.** The cell goes. The page stays if it still has cells.
/// * **An interior cell**, where the entry is a promoted key stored nowhere
///   else. The cell cannot simply be dropped or the key is lost from the index
///   — a wrong answer to every later query, not an error. It is replaced with
///   the largest key of the subtree it bounded, which is the same promotion rule
///   run backwards. When that subtree is empty there is nothing to promote, and
///   *then* the cell goes, because the key being deleted is the cell's own.
/// * **A leaf that just emptied.** The page is dropped from its parent. Which
///   cell goes with it is not symmetric, because of the same promotion: the cell
///   whose child emptied keeps its key, so only the cell is removed and the
///   empty page goes with it.
///
/// # The root never moves
///
/// The root's page number is the one the schema row holds for the life of the
/// index, so nothing here frees it. A collapse that reaches the root copies the
/// surviving subtree onto the root page instead, which is what [`graft_root`] is
/// for. A fully emptied index therefore reads back as a root leaf with no cells,
/// which is the shape sqlite3 leaves: on a 300-row index emptied by
/// `DELETE FROM t`, 3.53.4 ends at `page_count` 3 with the index root as an
/// index leaf holding nothing.
///
/// # The hook the pager still needs
///
/// Nothing here allocates, so a delete run no longer grows the file. Freed pages
/// still go to the freelist and are not handed out again, because
/// [`Pager::allocate`] appends rather than popping the trunk — that is a
/// `pager.rs` change this module does not own, and it is what would let a
/// dropped page be reused by a later `CREATE INDEX` or a table split.
pub fn remove_entry(
    pager: &mut Pager,
    root: u32,
    key_len: usize,
    entry: &IndexEntry,
) -> Result<()> {
    let mut path: Vec<DeleteStep> = Vec::new();
    match page_kind(pager, root)? {
        // A root leaf that empties is a legal, empty index — the shape a
        // freshly-created one has — and there is nothing above it to drop it
        // into, so the path stays empty and nothing collapses.
        page_type::INDEX_LEAF => return remove_from_leaf(pager, root, entry),
        page_type::INDEX_INTERIOR => {}
        other => {
            return Err(Error::corrupt(format!(
                "page {root} is a {} where an index root was expected",
                page_type::name(other)
            )))
        }
    }
    let mut page_no = root;
    loop {
        let page = IndexInterior::read(pager, page_no)?;
        // The inclusive descent sends an entry equal to a separator left on
        // purpose, so a key that merely *compares* equal to a separator has to be
        // told apart from the separator itself. This is what tells them apart.
        if let Some(pos) = separator_position(&page, entry, key_len) {
            return demote(pager, &mut path, page_no, pos);
        }
        let (pos, _, child) = page.descent(entry, key_len);
        path.push(DeleteStep { page_no, pos });
        match page_kind(pager, child)? {
            page_type::INDEX_LEAF => {
                // A leaf that empties is *left in place*, under the cell that
                // used to route to it. That cell stores a key of its own, which
                // is still indexed, so the cell cannot go with the page and
                // nothing above it may change either. The page is unreachable
                // but still referenced, which is not a leak — `free_index_tree`
                // reaches it, and `drop_index` frees it — and it is the same
                // shape `split_leaf` produces when it separates the first key
                // from the rest, so it is one this engine already writes and
                // reads. Reclaiming it is SQLite's `balance()` merge, which is
                // an optimisation rather than a correctness requirement: a new
                // key at or below that cell's separator routes into the empty
                // leaf again and refills it.
                return remove_from_leaf(pager, child, entry);
            }
            page_type::INDEX_INTERIOR => page_no = child,
            other => {
                return Err(Error::corrupt(format!(
                    "page {child} is a {} inside an index b-tree",
                    page_type::name(other)
                )))
            }
        }
    }
}

/// One step of a descent, so a page the removal empties can be dropped on the
/// way back up. The root is in it — the root's *page* is never dropped, but the
/// pages under it are.
#[derive(Clone, Copy)]
struct DeleteStep {
    page_no: u32,
    /// The cell position the descent took, where `cells.len()` means the
    /// rightmost child.
    pos: usize,
}

/// Where `entry` is a promoted key of `page`, if it is one.
fn separator_position(page: &IndexInterior, entry: &IndexEntry, key_len: usize) -> Option<usize> {
    page.cells.iter().position(|c| {
        crate::index::compare(&c.separator, entry, key_len) == std::cmp::Ordering::Equal
    })
}

/// The page type at `page_no`, from its header byte, with page 1's 100-byte file
/// header accounted for.
fn page_kind(pager: &mut Pager, page_no: u32) -> Result<u8> {
    let page = pager.read_page(page_no)?;
    let at = if page_no == 1 { 100 } else { 0 };
    page.get(at)
        .copied()
        .ok_or_else(|| Error::corrupt(format!("page {page_no} is truncated")))
}

/// Takes one entry out of one index leaf.
///
/// The entry's own overflow chain is freed, because nothing names it once the
/// cell is gone, and every *other* entry's chain head is carried across the
/// rewrite — [`IndexLeaf::write_to`] allocates a fresh chain for any entry whose
/// head is zero, so a rewrite that dropped the heads would strand a whole chain
/// per wide key and leave pages the file never reuses.
fn remove_from_leaf(pager: &mut Pager, page_no: u32, entry: &IndexEntry) -> Result<()> {
    let mut leaf = IndexLeaf::read(pager, page_no)?;
    let pos = leaf
        .entries
        .iter()
        .position(|e| e == entry)
        .ok_or_else(|| {
            Error::new(
                ResultCode::Error,
                format!("no index entry for rowid {}", entry.rowid),
            )
        })?;
    let head = leaf.overflows.get(pos).copied().unwrap_or(0);
    leaf.entries.remove(pos);
    if leaf.overflows.len() > pos {
        leaf.overflows.remove(pos);
    }
    leaf.write_to(pager)?;
    free_chain(pager, head)
}

/// Removes a promoted key from the interior cell that holds it.
///
/// The cell is the entry's only home, so the key has to be put somewhere. The
/// right answer is the largest key of the subtree the cell bounded: it is a real
/// entry at or below where the old separator sat, so the descent still routes
/// correctly, and promoting it is the same rule a leaf split applies, run
/// backwards. The **largest** is what it has to be — the smallest would sit well
/// below the old separator, and every entry between the two would then sort above
/// a separator that is supposed to bound it.
///
/// When the subtree holds nothing there is nothing to promote, and the key being
/// deleted *is* the cell's own, so the cell goes and its now-empty child goes
/// with it. That is the only way a cell ever leaves a page, and it is why an
/// emptied leaf whose parent's cell still has a key is left alone rather than
/// dropped: a split that separates the first key from the rest leaves exactly
/// that shape, so it is one this engine already writes and reads.
fn demote(pager: &mut Pager, path: &mut [DeleteStep], page_no: u32, pos: usize) -> Result<()> {
    let page = IndexInterior::read(pager, page_no)?;
    let child = page
        .cells
        .get(pos)
        .ok_or_else(|| Error::corrupt(format!("page {page_no} has no cell {pos}")))?
        .left_child;
    let leaf = rightmost_leaf(pager, child)?;
    let mut victim = IndexLeaf::read(pager, leaf)?;
    if victim.entries.is_empty() {
        // Nothing below to promote. The cell holds the key being deleted and
        // nothing else lives under it, so both go — and since the key the cell
        // was storing is exactly the one the delete removed, dropping the cell
        // loses nothing that is still indexed.
        let mut page = IndexInterior::read(pager, page_no)?;
        let removed = page.cells.remove(pos);
        let overflow = removed.overflow;
        let emptied = page.cells.is_empty();
        let rightmost = page.rightmost;
        page.write_to(pager)?;
        free_chain(pager, overflow)?;
        free_index_tree(pager, removed.left_child)?;
        if emptied {
            // The page names one subtree and is no longer a legal interior page.
            //
            // The root's page number is fixed, so the subtree is copied onto it
            // rather than moved to it. That is what leaves the reference's shape
            // behind: a fully deleted index reads back as a root leaf with no
            // cells, which is what sqlite3 3.53.4 leaves on a 300-row index
            // emptied by `DELETE FROM t` — `page_count` 3, and the index root as
            // an index leaf holding nothing.
            //
            // The root is the one page the descent never pushed a step for — a
            // step is pushed when a page descends *to* its child, so the page
            // the descent started on has no step of its own. An empty path here
            // therefore means this page is the root.
            if path.is_empty() {
                return graft_root(pager, rightmost, page_no);
            }
            let rest = &path[..path.len() - 1];
            collapse(pager, rest, page_no)?;
        }
        return Ok(());
    }
    let last = victim.entries.len() - 1;
    let replacement = victim.entries[last].clone();
    let replacement_overflow = victim.overflows.get(last).copied().unwrap_or(0);
    victim.entries.pop();
    if victim.overflows.len() > last {
        victim.overflows.pop();
    }
    victim.write_to(pager)?;

    let mut page = IndexInterior::read(pager, page_no)?;
    let promoted_overflow = page.cells[pos].overflow;
    page.cells[pos].separator = replacement;
    page.cells[pos].overflow = replacement_overflow;
    page.write_to(pager)?;
    // The old separator's chain is the one nothing names now. The replacement's
    // chain moved up with its key, exactly as a split moves it.
    free_chain(pager, promoted_overflow)
}

/// The page holding the largest key of the subtree rooted at `page_no`.
///
/// That is the rightmost leaf: the rightmost child of an interior page is where
/// everything above its last separator lives, and the leaf it reaches is one
/// step further down. Following the *last* cell's child, rather than treating
/// the last cell's separator as the end, is what makes this a descent to a leaf
/// rather than a walk of the interior levels.
fn rightmost_leaf(pager: &mut Pager, page_no: u32) -> Result<u32> {
    let mut page_no = page_no;
    loop {
        if page_kind(pager, page_no)? != page_type::INDEX_INTERIOR {
            return Ok(page_no);
        }
        let page = IndexInterior::read(pager, page_no)?;
        // The last cell's left child holds the keys between the second-to-last
        // separator and the last one, so the rightmost leaf is below it. An
        // interior page with no cells names nothing, so the rightmost is all
        // there is; a delete reads such a page only on the way to creating one.
        match page.cells.last() {
            None => page_no = page.rightmost,
            Some(cell) => page_no = cell.left_child,
        }
    }
}

/// Drops a page the descent left empty, and repeats upwards while the pages it
/// empties on the way are themselves dropped.
///
/// `path` is the descent, root first, and `dead` is the page that just emptied.
/// This is the reference's own rule: a page with no cells is dropped from the
/// page above it, and the space that frees is left to the parent's future splits
/// rather than handed back to the file.
///
/// Which cell goes with the page is not symmetric, and the promotion is why:
///
/// * **The page was a cell's left child.** That key range is empty, so the cell
///   goes with it — and with it goes the key the cell was storing, which is
///   *correct* here only because the page being dropped is the demote's empty
///   subtree. A leaf that merely emptied is not collapsed at all, because its
///   parent's cell still holds a live key.
/// * **The page was the rightmost.** Everything above the last separator went
///   with it, so the last cell goes and that cell's own child becomes the
///   rightmost, which keeps the range between the two separators covered.
///
/// A parent left with no cells is not a legal interior page, so it collapses
/// into the one subtree it still names, and the process repeats one level up.
fn collapse(pager: &mut Pager, path: &[DeleteStep], dead: u32) -> Result<()> {
    let mut dead = dead;
    for (i, step) in path.iter().enumerate().rev() {
        let is_root = i == 0;
        let mut parent = IndexInterior::read(pager, step.page_no)?;
        let was_rightmost = step.pos >= parent.cells.len();

        let removed = if was_rightmost {
            // The last cell's left child becomes the rightmost, so the range
            // between the two separators stays covered by the parent.
            let last = parent
                .cells
                .pop()
                .ok_or_else(|| Error::corrupt(format!("page {} has no cells", step.page_no)))?;
            parent.rightmost = last.left_child;
            last
        } else {
            parent.cells.remove(step.pos)
        };
        free_chain(pager, removed.overflow)?;

        if parent.cells.is_empty() {
            // Nothing left to separate, so the page names one subtree and stops
            // being a legal interior page.
            if is_root {
                // The root's page number is fixed, so the surviving subtree is
                // copied onto it rather than moved to it. Copying an emptied
                // leaf is what leaves the reference's shape behind: a fully
                // deleted index reads back as a root leaf with no cells.
                return graft_root(pager, parent.rightmost, step.page_no);
            }
            free_index_tree(pager, step.page_no)?;
            dead = step.page_no;
            continue;
        }
        // The parent still has cells, so it keeps its shape and simply stops
        // naming the page that emptied. It is written before that page is
        // freed, so there is never a moment at which a live page points at a
        // freed one.
        parent.write_to(pager)?;
        free_index_tree(pager, dead)?;
        return Ok(());
    }
    Ok(())
}

/// Moves a freshly-built tree onto `to`, so a rebuilt index keeps the root
/// page number its schema row already names.
///
/// The copy is byte for byte, which is correct because both trees were written
/// by this engine and the destination pages are brand new: every child and
/// overflow pointer in the copied cells names a page that is *not* `to`, since
/// the only page freed and reused here is the old root and nothing inside a
/// b-tree points at its own root.
///
/// The page `to` is written as a plain copy, so the interior layout is the
/// source's rather than a re-fit one. Both are produced by
/// [`IndexInterior::write_to`] with the same geometry, so the copy is the same
/// bytes the writer would have produced anyway.
fn graft_root(pager: &mut Pager, new_root: u32, to: u32) -> Result<()> {
    if new_root == to {
        return Ok(());
    }
    let bytes = pager.read_page(new_root)?;
    debug_assert_eq!(bytes.len(), pager.page_size() as usize);
    {
        let dst = pager.page(to)?;
        dst.copy_from_slice(&bytes);
    }
    pager.mark_dirty(to);
    // `new_root` is now unreferenced. It is freed *after* the copy, because
    // freeing zeroes the page and the bytes have just been read out of it.
    pager.free(new_root)?;
    Ok(())
}

/// An index's five schema-row values: `(type, name, tbl_name, rootpage, sql)`.
///
/// The shape is a table's with `type` set to `index` and `tbl_name` naming the
/// *table*, which is the one place an index row differs. Checked against 3.53.4
/// by the `type|name|tbl_name|rootpage|sql` listing quoted in the module
/// comment.
pub fn schema_row_values(index: &Index, root_page: u32, sql: &str) -> Vec<Value> {
    vec![
        Value::Text("index".into()),
        Value::Text(index.name.clone()),
        Value::Text(index.table.clone()),
        Value::Integer(root_page as i64),
        Value::Text(sql.to_owned()),
    ]
}

/// Rebuilds an index's definition by re-parsing the text the schema stores.
///
/// This is `connection::rebuild_table`'s counterpart. The reference reads the
/// `sql` column back on every open, so an index created by a previous
/// connection — or by the real sqlite3 — has to come back with its columns,
/// directions, uniqueness and root page intact. A schema row whose `sql` does
/// not re-parse is reported the same way a table's is: `malformed database
/// schema (NAME)`, with the parser's own message after it, under
/// `SQLITE_CORRUPT`.
///
/// # The call the connection has to make
///
/// `Connection::load_schema` currently skips any row whose `values[0]` is not
/// `"table"`, which is where an index is silently dropped on reopen. It has to
/// take this branch too:
///
/// ```ignore
/// for row in rows {
///     if row.values.len() < 5 { continue; }
///     match row.values[0].as_str() {
///         Some("table") => { /* the existing rebuild_table path */ }
///         Some("index") => {
///             let index = crate::index_ddl::rebuild_index(
///                 row.values[1].as_str().unwrap_or_default(),
///                 row.values[4].as_str().unwrap_or_default(),
///                 row.values[3].as_i64().unwrap_or(0) as u32,
///             )?;
///             self.catalog.put_index(index);
///         }
///         _ => continue,
///     }
/// }
/// ```
///
/// Two details that come with it:
///
/// * An automatically created index has an **empty** `sql` — checked against
///   3.53.4, where `SELECT sql FROM sqlite_schema` on
///   `CREATE TABLE t(a UNIQUE)` returns an empty string for
///   `sqlite_autoindex_t_1`. `rebuild_index` reconstructs one with
///   [`derived_index_name`], empty columns and `unique: true`, so such a row
///   comes back as an index that `drop_index` will then refuse to remove.
/// * `rootpage` comes from the schema row rather than from the statement, and
///   it is the one field that is not re-derivable: a table's root can move when
///   it splits, and so can an index's.
pub fn rebuild_index(name: &str, sql_text: &str, root: u32) -> Result<Index> {
    let malformed = |detail: Option<String>| match detail {
        Some(e) => Error::new(
            ResultCode::Corrupt,
            format!("malformed database schema ({name}): {e}"),
        ),
        None => Error::new(
            ResultCode::Corrupt,
            format!("malformed database schema ({name})"),
        ),
    };
    if sql_text.trim().is_empty() {
        // An automatically created index stores no text at all.
        return Ok(Index {
            name: name.to_owned(),
            table: String::new(),
            columns: Vec::new(),
            ascending: Vec::new(),
            unique: true,
            root_page: root,
        });
    }
    let stmt = crate::parser::parse_one(sql_text).map_err(|e| malformed(Some(e.to_string())))?;
    let crate::parser::Stmt::CreateIndex { table, columns, .. } = stmt else {
        return Err(malformed(None));
    };
    Ok(Index {
        name: name.to_owned(),
        table,
        columns: columns.iter().map(|(c, _)| c.clone()).collect(),
        ascending: columns.iter().map(|(_, a)| *a).collect(),
        // `unique` is taken as the word UNIQUE appears between CREATE and
        // INDEX, read from the stored text rather than from the parse.
        //
        // The parse cannot be trusted for it: `Parser::create` eats `UNIQUE`
        // itself to decide that the statement is an index at all, and by the
        // time `create_index` runs there is nothing left to eat, so
        // `Stmt::CreateIndex.unique` is `false` for a `CREATE UNIQUE INDEX`
        // that arrived through the normal path. Read back from a schema row
        // that would drop the index's only reason to exist, and a UNIQUE
        // constraint that stopped being enforced after a reopen is exactly the
        // class of bug this whole layer exists to prevent.
        //
        // This is a workaround for a bug in the parser, which this module does
        // not own. The one-line fix in `parser.rs` is to have `create_index`
        // stop re-eating the keyword — take the flag `create` already
        // determined and pass it in — and then this expression becomes the
        // parsed `unique` again.
        unique: is_unique_sql(sql_text),
        root_page: root,
    })
}

/// Whether a stored `CREATE INDEX` text declares a UNIQUE index.
///
/// This reads the word between `CREATE` and `INDEX`, which is where the
/// reference puts it and the only place it can appear: `CREATE [UNIQUE] INDEX`.
///
/// It is a word match rather than a substring match, so an index whose *name*
/// is `unique` — `CREATE INDEX unique ON t(unique)` — is not mistaken for a
/// UNIQUE index. The tokenizer already knows the boundaries, so it is asked
/// rather than a scan being written: every token from the start until `INDEX`
/// is spelled out, and anything but the single word `UNIQUE` means not unique.
fn is_unique_sql(sql: &str) -> bool {
    use crate::tokenizer::{Keyword, Token, Tokenizer};
    let Ok(tokens) = Tokenizer::tokenize_all(sql) else {
        return false;
    };
    for (token, _) in tokens {
        match token {
            // `INDEX` closes the search: everything before it is either the
            // single word UNIQUE or nothing.
            Token::Keyword(Keyword::Index) => return false,
            Token::Keyword(Keyword::Unique) => return true,
            // `CREATE`, TEMP and the schema qualifier are all allowed here.
            _ => continue,
        }
    }
    false
}

/// An index's entries, read back in key order. This is how a caller checks an
/// index against its table rather than against a fixed expectation.
pub fn read_entries(pager: &mut Pager, root: u32, key_len: usize) -> Result<Vec<IndexEntry>> {
    IndexTree::open(pager, root, key_len).scan()
}

/// An index's entries, reduced to `(key, rowid)` pairs, which is what a
/// comparison against a walk of the table produces.
pub fn read_pairs(pager: &mut Pager, root: u32, key_len: usize) -> Result<Vec<(Vec<Value>, i64)>> {
    Ok(read_entries(pager, root, key_len)?
        .into_iter()
        .map(|e| (e.key, e.rowid))
        .collect())
}

/// The interior page at `page_no`, for a caller that wants to check a rebuilt
/// index's shape.
pub fn read_interior(pager: &mut Pager, page_no: u32) -> Result<IndexInterior> {
    IndexInterior::read(pager, page_no)
}

/// The page numbers a whole index tree reaches, root first.
pub fn tree_pages(pager: &mut Pager, root: u32) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        if out.contains(&page_no) {
            continue;
        }
        out.push(page_no);
        let page = pager.read_page(page_no)?;
        match page[0] {
            page_type::INDEX_LEAF => {}
            page_type::INDEX_INTERIOR => {
                let interior = IndexInterior::read(pager, page_no)?;
                for cell in interior.cells.iter() {
                    stack.push(cell.left_child);
                }
                stack.push(interior.rightmost);
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

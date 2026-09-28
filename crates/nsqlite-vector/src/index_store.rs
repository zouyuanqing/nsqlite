//! Storage for a `vec0` table: one ordinary table per shadow table.
//!
//! # Why this is not a struct with a `Vec`
//!
//! A kNN table has an unusual property: the *index* and the *table* must never
//! disagree. If the vectors live in a `Vec<Vec<f64>>` and the rowids in a
//! `BTreeMap`, then a bug in either one is a silently wrong answer rather than
//! an error — a neighbour that does not exist, a row whose embedding is missing,
//! a delete that left a vector behind to be found by the next query. The
//! reference implementation's answer is the same one: it stores vectors in real
//! SQLite tables and goes through the engine's row path, so an inconsistency
//! cannot survive a page write.
//!
//! So this module holds a [`RowStore`], a trait, and the reference here is
//! [`MemoryRowStore`] — an in-process implementation used by the tests and by
//! anything wiring the module up before the engine side exists. The *trait* is
//! the integration contract; the in-memory store is what makes it testable
//! without a database.
//!
//! # The row layout
//!
//! A stored row is `(rowid, embedding bytes, metadata)`, exactly as the task
//! specifies, split across two shadow tables so that a row's payload does not
//! have to be rewritten to change its metadata:
//!
//! * `<table>_vectors` — `(rowid INTEGER PRIMARY KEY, embedding BLOB)`
//! * `<table>_chunks` — `(rowid INTEGER PRIMARY KEY, "+aux" BLOB)`
//!
//! The embedding is a big-endian `f64` array, which is what
//! [`crate::vtab::encode_vector`] writes and what the engine's own record
//! writer uses (`crates/nsqlite/src/record.rs:206`), so a blob in this table
//! is a blob the real `sqlite3` can at least read without being surprised.
//!
//! # What a query costs
//!
//! Building the search index reads every row of the vectors table. That is the
//! honest cost of exact search: `search::FlatIndex` is brute force by
//! construction, and this module does not pretend otherwise by caching across
//! calls. A later step can swap in a persistent index; the trait below is what
//! it would have to implement.

use std::collections::BTreeMap;

use crate::search::{FlatIndex, Neighbor, SearchError};
use crate::vtab::Schema;

/// The bytes of a vector as stored in a shadow-table blob.
///
/// **Big-endian, deliberately.** The engine writes an `f64` inside a SQLite
/// record with `to_be_bytes` — `crates/nsqlite/src/record.rs:206` — so a blob
/// in any other order would be the one value in the table that the engine's own
/// tools could not read, and a `PRAGMA integrity_check` in the real `sqlite3`
/// would be reading bytes it was never meant to interpret.
///
/// The format is otherwise deliberately trivial: `dim` IEEE-754 doubles, in
/// order, with no header and no padding. Anything more would make a blob
/// written here unreadable by a future reader that knew only the width, and the
/// width is in the `CREATE TABLE` text.
pub fn encode_vector(vector: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 8);
    for value in vector {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

/// Converts a stored blob back into a vector.
///
/// A blob of the wrong length is [`SearchError::Config`] rather than a silent
/// truncation or a panic: a row whose embedding is the wrong size is a corrupt
/// row, and the only safe answer to a corrupt row is to refuse it.
pub fn decode_vector(blob: &[u8], dim: usize) -> Result<Vec<f64>, SearchError> {
    let expected = dim * 8;
    if blob.len() != expected {
        return Err(SearchError::Config(format!(
            "a float[{dim}] embedding is {expected} bytes, got {}",
            blob.len()
        )));
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in blob.chunks_exact(8) {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(chunk);
        out.push(f64::from_be_bytes(bytes));
    }
    Ok(out)
}

/// One stored row, as the search sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRow {
    /// The rowid. The virtual table's `rowid`.
    pub rowid: i64,
    /// The decoded embedding.
    pub vector: Vec<f64>,
    /// The auxiliary columns' values, keyed by column name, in the schema's
    /// order. Empty when the table declared none.
    pub metadata: Vec<Option<Vec<u8>>>,
}

impl StoredRow {
    /// The embedding as the bytes a shadow table holds.
    pub fn embedding_blob(&self) -> Vec<u8> {
        encode_vector(&self.vector)
    }

    /// The auxiliary payload as a single blob, or `None` when there is none.
    ///
    /// The metadata columns are stored one blob rather than one column each, so
    /// that adding an auxiliary column to a schema is a change to the payload
    /// format and not a migration of the shadow table's columns.
    pub fn metadata_blob(&self) -> Option<Vec<u8>> {
        if self.metadata.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for slot in &self.metadata {
            match slot {
                Some(bytes) => {
                    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                    out.extend_from_slice(bytes);
                }
                None => out.extend_from_slice(&u32::MAX.to_be_bytes()),
            }
        }
        Some(out)
    }

    /// Splits a metadata blob back into per-column values.
    ///
    /// Returns an empty vector for a blob that does not decode, rather than
    /// failing: a corrupt auxiliary payload must not make the *vector* column
    /// unsearchable, and the row is still findable. This is the one place in
    /// the module where a decode failure is deliberately swallowed, and the
    /// comment is here so that it is not mistaken for an oversight.
    pub fn parse_metadata(blob: Option<&[u8]>, count: usize) -> Vec<Option<Vec<u8>>> {
        let Some(blob) = blob else {
            return vec![None; count];
        };
        let mut out = Vec::with_capacity(count);
        let mut at = 0usize;
        for _ in 0..count {
            if at + 4 > blob.len() {
                out.push(None);
                continue;
            }
            let len = u32::from_be_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);
            at += 4;
            // `u32::MAX` is the NULL marker, and a length that runs past the end
            // of the blob is a truncated one. Both decode to NULL rather than to
            // an error: a corrupt auxiliary payload must not make the *vector*
            // column unsearchable, and the row is still findable.
            if len == u32::MAX || len as usize > blob.len() - at {
                out.push(None);
            } else {
                out.push(Some(blob[at..at + len as usize].to_vec()));
                at += len as usize;
            }
        }
        out
    }
}

/// The row path a `vec0` table reads and writes through.
///
/// This is the whole engine-side contract, minus the parts that are not about
/// rows. It is deliberately small: six methods, all of which the engine's
/// existing [`TableTree`](crate::table_tree) can already do for an ordinary
/// table, so implementing it against the real engine is a matter of routing
/// each method to `TableTree::insert`, `scan`, `get`, `remove`, `max_rowid`,
/// and `count` rather than writing new storage.
///
/// The methods take and return owned values rather than cursors, because the
/// engine's `TableTree::scan` returns `Vec<Row>` and a cursor abstraction would
/// be a second row format to keep in step. A table large enough for that to
/// matter is a table that should be using the approximate index instead.
pub trait RowStore {
    /// Inserts or replaces the row with this rowid.
    fn upsert(&mut self, row: &StoredRow) -> Result<(), StoreError>;

    /// Every row, in ascending rowid order.
    fn scan(&self) -> Result<Vec<StoredRow>, StoreError>;

    /// One row by rowid.
    fn get(&self, rowid: i64) -> Result<Option<StoredRow>, StoreError>;

    /// Removes a row, reporting whether it was there.
    fn remove(&mut self, rowid: i64) -> Result<bool, StoreError>;

    /// The number of rows.
    fn count(&self) -> Result<usize, StoreError>;

    /// The largest rowid, or `0` when the table is empty.
    fn max_rowid(&self) -> Result<i64, StoreError>;
}

/// A [`RowStore`] over a `BTreeMap`.
///
/// The map is keyed by rowid, which gives the ascending scan order the search
/// tie-break depends on for free: `search::FlatIndex` breaks a tie on insertion
/// order, and insertion order here *is* rowid order, so the result list is
/// reproducible across a save and reload.
#[derive(Debug, Clone, Default)]
pub struct MemoryRowStore {
    rows: BTreeMap<i64, StoredRow>,
}

impl MemoryRowStore {
    /// An empty store.
    pub fn new() -> Self {
        MemoryRowStore {
            rows: BTreeMap::new(),
        }
    }

    /// Builds a store holding `rows`, checking them against `schema` first.
    pub fn from_rows(schema: &Schema, rows: Vec<StoredRow>) -> Result<Self, StoreError> {
        let mut store = MemoryRowStore::new();
        for row in rows {
            store.insert_checked(schema, &row)?;
        }
        Ok(store)
    }

    /// Inserts a row after checking it against the schema.
    fn insert_checked(&mut self, schema: &Schema, row: &StoredRow) -> Result<(), StoreError> {
        check_row(schema, row)?;
        self.rows.insert(row.rowid, row.clone());
        Ok(())
    }

    /// The rows, in rowid order.
    pub fn rows(&self) -> impl Iterator<Item = &StoredRow> {
        self.rows.values()
    }
}

impl RowStore for MemoryRowStore {
    fn upsert(&mut self, row: &StoredRow) -> Result<(), StoreError> {
        // A store on its own has no schema, so the width check here is only
        // against the row it is replacing: a row cannot change width under
        // itself, and `Vec0Table` checks the width against the schema on the
        // way in.
        if row.vector.is_empty() {
            return Err(StoreError::EmptyVector);
        }
        if let Some(existing) = self.rows.get(&row.rowid) {
            if existing.vector.len() != row.vector.len() {
                return Err(StoreError::Dimension {
                    expected: existing.vector.len(),
                    found: row.vector.len(),
                });
            }
        }
        self.rows.insert(row.rowid, row.clone());
        Ok(())
    }

    fn scan(&self) -> Result<Vec<StoredRow>, StoreError> {
        Ok(self.rows.values().cloned().collect())
    }

    fn get(&self, rowid: i64) -> Result<Option<StoredRow>, StoreError> {
        Ok(self.rows.get(&rowid).cloned())
    }

    fn remove(&mut self, rowid: i64) -> Result<bool, StoreError> {
        Ok(self.rows.remove(&rowid).is_some())
    }

    fn count(&self) -> Result<usize, StoreError> {
        Ok(self.rows.len())
    }

    fn max_rowid(&self) -> Result<i64, StoreError> {
        Ok(self.rows.keys().next_back().copied().unwrap_or(0))
    }
}

/// A store checked against a schema, and the thing a query runs against.
///
/// This is the join between [`crate::vtab`]'s schema and the row path: it owns
/// the `vec0` row semantics (width, non-finite rejection, the rowid the table
/// hands out) and leaves the arithmetic to [`crate::search`].
#[derive(Debug, Clone)]
pub struct Vec0Table {
    schema: Schema,
    store: MemoryRowStore,
}

impl Vec0Table {
    /// An empty table with this schema.
    pub fn create(schema: Schema) -> Self {
        Vec0Table {
            schema,
            store: MemoryRowStore::new(),
        }
    }

    /// A table already holding `rows`.
    pub fn with_rows(schema: Schema, rows: Vec<StoredRow>) -> Result<Self, StoreError> {
        let store = MemoryRowStore::from_rows(&schema, rows)?;
        Ok(Vec0Table { schema, store })
    }

    /// The table's declaration.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The row path, for a caller that wants the store directly.
    pub fn store_mut(&mut self) -> &mut MemoryRowStore {
        &mut self.store
    }

    /// Inserts a row, assigning a rowid when `rowid` is `None`.
    ///
    /// An explicit rowid of `0` is *not* the same as no rowid: SQLite treats
    /// `0` as a value, and an `INSERT` that means "give me a rowid" says
    /// `NULL`. So `None` allocates and `Some(0)` inserts rowid 0, which is what
    /// the reference does and what a caller copying its output would rely on.
    pub fn insert(
        &mut self,
        rowid: Option<i64>,
        vector: Vec<f64>,
        metadata: Vec<Option<Vec<u8>>>,
    ) -> Result<i64, StoreError> {
        let rowid = match rowid {
            Some(id) => id,
            None => self.store.max_rowid()? + 1,
        };
        let row = StoredRow {
            rowid,
            vector,
            metadata,
        };
        check_row(&self.schema, &row)?;
        self.store.upsert(&row)?;
        Ok(rowid)
    }

    /// Replaces an existing row's vector and metadata.
    pub fn update(&mut self, rowid: i64, vector: Vec<f64>) -> Result<(), StoreError> {
        let Some(mut row) = self.store.get(rowid)? else {
            return Err(StoreError::NoSuchRow(rowid));
        };
        row.vector = vector;
        check_row(&self.schema, &row)?;
        self.store.upsert(&row)?;
        Ok(())
    }

    /// Deletes a row.
    pub fn delete(&mut self, rowid: i64) -> Result<bool, StoreError> {
        self.store.remove(rowid)
    }

    /// One row by rowid.
    pub fn get(&self, rowid: i64) -> Result<Option<StoredRow>, StoreError> {
        self.store.get(rowid)
    }

    /// The number of rows.
    pub fn len(&self) -> Result<usize, StoreError> {
        self.store.count()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.store.count()? == 0)
    }

    /// Builds the search index over every row.
    ///
    /// The index is built per query, not cached. That is the honest cost of
    /// exact search and it is stated here rather than hidden: `FlatIndex` is
    /// brute force, so the work is `O(n * dim)` for the build plus
    /// `O(n * dim * log k)` for the search, every time. The row order is
    /// rowid order, which is what makes the tie-break reproducible.
    pub fn build_index(&self) -> Result<FlatIndex, StoreError> {
        let vectors: Vec<Vec<f64>> = self
            .store
            .scan()?
            .into_iter()
            .map(|row| row.vector)
            .collect();
        FlatIndex::new(vectors, self.schema.metric).map_err(StoreError::from)
    }

    /// The `k` nearest rows to `query`, nearest first.
    ///
    /// **An empty table returns no rows rather than an error.** `FlatIndex::new`
    /// rejects an empty vector list, and a kNN query against a table with no
    /// rows is a question with the answer "none", not a fault. This is the one
    /// place the empty case is special-cased, and it is a deliberate departure
    /// from the index constructor's rule.
    pub fn search(&self, query: &[f64], k: usize) -> Result<Vec<Neighbor>, StoreError> {
        if self.store.count()? == 0 || k == 0 {
            return Ok(Vec::new());
        }
        let index = self.build_index()?;
        // The id the index returns is a position in the rowid-ordered scan,
        // which is exactly the position of the row in `self.store.scan()`.
        // Reading the rowid back out of the same scan is what keeps the two in
        // step; a parallel scan would be a second thing to get wrong.
        let rowids: Vec<i64> = self
            .store
            .scan()?
            .into_iter()
            .map(|row| row.rowid)
            .collect();
        let hits = index.search(query, k).map_err(StoreError::from)?;
        Ok(hits
            .into_iter()
            .map(|hit| Neighbor {
                id: rowids[hit.id] as usize,
                distance: hit.distance,
            })
            .collect())
    }

    /// The distance from `query` to one row, without ranking anything.
    pub fn distance_to(&self, query: &[f64], rowid: i64) -> Result<f64, StoreError> {
        let Some(row) = self.store.get(rowid)? else {
            return Err(StoreError::NoSuchRow(rowid));
        };
        self.schema
            .metric
            .distance(query, &row.vector)
            .map_err(StoreError::from)
    }

    /// The rows the shadow tables hold, in the order they would be written.
    ///
    /// This is the boundary the engine writes through: a caller with a real
    /// pager takes these two lists and puts each into its shadow table. The
    /// split into vectors and metadata is exactly the two-table layout
    /// [`crate::vtab::Schema::shadow_table_sql`] declares, and returning them
    /// separately is what keeps the storage layout in one place.
    pub fn shadow_rows(&self) -> Result<ShadowRows, StoreError> {
        let mut out = ShadowRows::default();
        for row in self.store.scan()? {
            out.vectors.push((row.rowid, row.embedding_blob()));
            out.metadata.push((row.rowid, row.metadata_blob()));
        }
        out.max_rowid = self.store.max_rowid()?;
        Ok(out)
    }

    /// Rebuilds a table from the rows its shadow tables hold.
    ///
    /// The inverse of [`Vec0Table::shadow_rows`], and the path a reopened
    /// database takes: the schema comes back out of the `_info` shadow table
    /// and the rows out of `_vectors`, with no index having to be rebuilt from
    /// anything but the file.
    pub fn from_shadow_rows(schema: Schema, rows: &ShadowRows) -> Result<Self, StoreError> {
        let aux = schema.auxiliaries().count();
        let mut table = Vec0Table::create(schema);
        for (rowid, blob) in &rows.vectors {
            let vector = decode_vector(blob, table.schema.dim()).map_err(StoreError::from)?;
            let metadata = rows
                .metadata
                .iter()
                .find(|(id, _)| id == rowid)
                .and_then(|(_, bytes)| bytes.as_deref());
            table.insert(
                Some(*rowid),
                vector,
                StoredRow::parse_metadata(metadata, aux),
            )?;
        }
        Ok(table)
    }
}

/// The contents of the two payload shadow tables.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShadowRows {
    /// `<table>_vectors`: `(rowid, embedding bytes)`, in rowid order.
    pub vectors: Vec<(i64, Vec<u8>)>,
    /// `<table>_chunks`: `(rowid, metadata bytes)`, in rowid order.
    pub metadata: Vec<(i64, Option<Vec<u8>>)>,
    /// The largest rowid seen, for the `_rowids` shadow table.
    pub max_rowid: i64,
}

/// Checks a row against a schema.
fn check_row(schema: &Schema, row: &StoredRow) -> Result<(), StoreError> {
    if row.vector.len() != schema.dim() {
        return Err(StoreError::Dimension {
            expected: schema.dim(),
            found: row.vector.len(),
        });
    }
    if let Some(at) = row.vector.iter().position(|v| !v.is_finite()) {
        return Err(StoreError::NonFinite {
            rowid: row.rowid,
            at,
        });
    }
    let want = schema.auxiliaries().count();
    if row.metadata.len() != want {
        return Err(StoreError::MetadataCount {
            expected: want,
            found: row.metadata.len(),
        });
    }
    Ok(())
}

/// What can go wrong storing or searching rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A row's vector was not the declared width.
    Dimension {
        /// The width the schema declares.
        expected: usize,
        /// The width that was supplied.
        found: usize,
    },
    /// A vector held a NaN or an infinity.
    ///
    /// Rejected at the row path rather than at the search, because a NaN makes
    /// the sort order arbitrary and the tie-break this module promises would
    /// stop being a promise. `search::FlatIndex::new` rejects the same thing;
    /// doing it here means the row is refused at the door rather than the table
    /// failing to open.
    NonFinite {
        /// The row that carried it.
        rowid: i64,
        /// The dimension it was at.
        at: usize,
    },
    /// A row carried the wrong number of auxiliary slots.
    MetadataCount {
        /// How many the schema declares.
        expected: usize,
        /// How many were supplied.
        found: usize,
    },
    /// An update or distance named a rowid that is not there.
    NoSuchRow(i64),
    /// A row with no components at all, which no declared width can hold.
    EmptyVector,
    /// The underlying search rejected the query.
    Search(SearchError),
}

impl From<SearchError> for StoreError {
    fn from(err: SearchError) -> Self {
        StoreError::Search(err)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Dimension { expected, found } => {
                write!(f, "expected a vector of {expected} dimensions, got {found}")
            }
            StoreError::NonFinite { rowid, at } => {
                write!(f, "row {rowid} has a non-finite value at dimension {at}")
            }
            StoreError::MetadataCount { expected, found } => {
                write!(f, "expected {expected} auxiliary values, got {found}")
            }
            StoreError::NoSuchRow(rowid) => write!(f, "no such row: {rowid}"),
            StoreError::EmptyVector => write!(f, "a stored vector cannot be empty"),
            StoreError::Search(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtab::Schema;

    fn schema() -> Schema {
        Schema::parse(
            "t",
            "(rowid, *bucket, embedding float[3], +note, distance_metric=L2)",
        )
        .unwrap()
    }

    fn row(rowid: i64, vector: Vec<f64>) -> StoredRow {
        StoredRow {
            rowid,
            vector,
            metadata: vec![Some(b"note".to_vec())],
        }
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused() {
        let err = Vec0Table::with_rows(schema(), vec![row(1, vec![1.0, 2.0])]).unwrap_err();
        assert_eq!(
            err,
            StoreError::Dimension {
                expected: 3,
                found: 2
            }
        );
    }

    #[test]
    fn a_non_finite_component_is_refused_at_the_row() {
        let err =
            Vec0Table::with_rows(schema(), vec![row(1, vec![1.0, f64::NAN, 3.0])]).unwrap_err();
        assert_eq!(err, StoreError::NonFinite { rowid: 1, at: 1 });
    }

    #[test]
    fn a_row_with_the_wrong_number_of_auxiliary_slots_is_refused() {
        let mut bad = row(1, vec![1.0, 2.0, 3.0]);
        bad.metadata = vec![];
        let err = Vec0Table::with_rows(schema(), vec![bad]).unwrap_err();
        assert_eq!(
            err,
            StoreError::MetadataCount {
                expected: 1,
                found: 0
            }
        );
    }

    #[test]
    fn an_explicit_rowid_of_zero_is_kept_and_none_allocates() {
        let mut t = Vec0Table::create(schema());
        assert_eq!(
            t.insert(Some(0), vec![1.0, 0.0, 0.0], vec![None]).unwrap(),
            0
        );
        assert_eq!(t.insert(None, vec![0.0, 1.0, 0.0], vec![None]).unwrap(), 1);
        assert_eq!(t.insert(None, vec![0.0, 0.0, 1.0], vec![None]).unwrap(), 2);
    }

    #[test]
    fn searching_an_empty_table_returns_nothing_rather_than_failing() {
        let t = Vec0Table::create(schema());
        assert!(t.search(&[1.0, 0.0, 0.0], 5).unwrap().is_empty());
    }

    #[test]
    fn searching_agrees_with_the_flat_index_on_ranking() {
        let t = Vec0Table::with_rows(
            schema(),
            vec![
                row(10, vec![1.0, 0.0, 0.0]),
                row(20, vec![0.0, 1.0, 0.0]),
                row(30, vec![0.9, 0.1, 0.0]),
                row(40, vec![0.0, 0.0, 1.0]),
            ],
        )
        .unwrap();
        let query = [1.0, 0.0, 0.0];
        let got = t.search(&query, 3).unwrap();
        // Row 10 is the exact match at 0, row 30 is close at 0.02, and rows 20
        // and 40 are both *orthogonal* to the query at a squared L2 of 2.0 each.
        // They genuinely tie, and the tie-break is rowid order, so 20 comes
        // first. The expectation is written out in full because a tie is exactly
        // the case a test checking only "the nearest first" would pass while the
        // ordering was wrong.
        assert_eq!(got.iter().map(|n| n.id).collect::<Vec<_>>(), [10, 30, 20]);
        // Squared, not rooted: `search` documents that and the module does not
        // take a root at the boundary, so the number a query reports is the
        // index's number.
        assert_eq!(got[0].distance, 0.0);
        assert!((got[1].distance - 0.02).abs() < 1e-12);
        assert_eq!(got[2].distance, 2.0);
        // And the numbers agree with the index built over the same rows.
        let index = t.build_index().unwrap();
        let direct = index.search(&query, 3).unwrap();
        assert_eq!(
            got.iter().map(|n| n.distance).collect::<Vec<_>>(),
            direct.iter().map(|n| n.distance).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_saved_table_reopens_to_the_same_ranking() {
        let rows = vec![
            row(1, vec![1.0, 0.0, 0.0]),
            row(2, vec![0.0, 1.0, 0.0]),
            row(3, vec![0.5, 0.5, 0.0]),
        ];
        let t = Vec0Table::with_rows(schema(), rows).unwrap();
        let shadow = t.shadow_rows().unwrap();
        let reopened = Vec0Table::from_shadow_rows(schema(), &shadow).unwrap();
        let query = [1.0, 0.0, 0.0];
        assert_eq!(
            t.search(&query, 3).unwrap(),
            reopened.search(&query, 3).unwrap()
        );
    }

    #[test]
    fn auxiliary_values_survive_the_shadow_table_round_trip() {
        let mut r = row(1, vec![1.0, 0.0, 0.0]);
        r.metadata = vec![Some(b"hello".to_vec())];
        let t = Vec0Table::with_rows(schema(), vec![r]).unwrap();
        let shadow = t.shadow_rows().unwrap();
        let reopened = Vec0Table::from_shadow_rows(schema(), &shadow).unwrap();
        assert_eq!(
            reopened.get(1).unwrap().unwrap().metadata,
            vec![Some(b"hello".to_vec())]
        );
    }

    #[test]
    fn a_null_auxiliary_value_round_trips_as_null_and_not_as_empty() {
        // `row()` sets a value, so this builds the same row with an explicit
        // NULL instead, and the point is that NULL and an empty blob are not
        // the same thing on the way back out.
        let mut null = row(1, vec![1.0, 0.0, 0.0]);
        null.metadata = vec![None];
        let t = Vec0Table::with_rows(schema(), vec![null]).unwrap();
        let shadow = t.shadow_rows().unwrap();
        let reopened = Vec0Table::from_shadow_rows(schema(), &shadow).unwrap();
        assert_eq!(reopened.get(1).unwrap().unwrap().metadata, vec![None]);
    }

    #[test]
    fn a_delete_removes_the_row_from_the_search_as_well() {
        let mut t = Vec0Table::with_rows(
            schema(),
            vec![row(1, vec![1.0, 0.0, 0.0]), row(2, vec![0.0, 1.0, 0.0])],
        )
        .unwrap();
        assert!(t.delete(1).unwrap());
        assert!(!t.delete(1).unwrap());
        let hits = t.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 2);
    }

    #[test]
    fn an_update_changes_the_ranking() {
        let mut t = Vec0Table::with_rows(
            schema(),
            vec![row(1, vec![0.0, 1.0, 0.0]), row(2, vec![0.0, 0.0, 1.0])],
        )
        .unwrap();
        let query = [1.0, 0.0, 0.0];
        assert_eq!(t.search(&query, 2).unwrap()[0].id, 1);
        t.update(2, query.to_vec()).unwrap();
        assert_eq!(t.search(&query, 2).unwrap()[0].id, 2);
    }

    #[test]
    fn updating_a_row_that_is_not_there_is_an_error() {
        let mut t = Vec0Table::create(schema());
        assert_eq!(
            t.update(7, vec![1.0, 0.0, 0.0]),
            Err(StoreError::NoSuchRow(7))
        );
    }

    #[test]
    fn a_query_of_the_wrong_width_is_the_search_error_not_a_dimension_of_our_own() {
        let t = Vec0Table::with_rows(schema(), vec![row(1, vec![1.0, 0.0, 0.0])]).unwrap();
        let err = t.search(&[1.0, 0.0], 1).unwrap_err();
        assert!(matches!(
            err,
            StoreError::Search(SearchError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn a_k_larger_than_the_table_returns_every_row() {
        // Measured against the reference: `SELECT d FROM t ORDER BY d LIMIT 5`
        // over a two-row table returns both rows rather than erroring.
        let t = Vec0Table::with_rows(
            schema(),
            vec![row(1, vec![1.0, 0.0, 0.0]), row(2, vec![0.0, 1.0, 0.0])],
        )
        .unwrap();
        assert_eq!(t.search(&[1.0, 0.0, 0.0], 99).unwrap().len(), 2);
    }

    #[test]
    fn a_zero_k_returns_nothing() {
        let t = Vec0Table::with_rows(schema(), vec![row(1, vec![1.0, 0.0, 0.0])]).unwrap();
        assert!(t.search(&[1.0, 0.0, 0.0], 0).unwrap().is_empty());
    }

    #[test]
    fn distance_to_reports_a_single_row_without_ranking() {
        let t = Vec0Table::with_rows(schema(), vec![row(1, vec![1.0, 0.0, 0.0])]).unwrap();
        // L2 is squared in `search`, and this module does not take a root: the
        // number is the index's number, and the docs say so where it matters.
        assert_eq!(t.distance_to(&[0.0, 0.0, 0.0], 1).unwrap(), 1.0);
    }

    #[test]
    fn a_cosine_table_refuses_a_zero_query_rather_than_ranking_it() {
        let s = Schema::parse("t", "(embedding float[2], distance_metric=cosine)").unwrap();
        let t = Vec0Table::with_rows(
            s,
            vec![StoredRow {
                rowid: 1,
                vector: vec![1.0, 0.0],
                metadata: vec![],
            }],
        )
        .unwrap();
        assert_eq!(
            t.search(&[0.0, 0.0], 1),
            Err(StoreError::Search(SearchError::ZeroVector))
        );
    }

    #[test]
    fn a_row_store_tracks_count_and_max_rowid() {
        let mut s = MemoryRowStore::new();
        assert_eq!(s.count().unwrap(), 0);
        assert_eq!(s.max_rowid().unwrap(), 0);
        s.upsert(&row(5, vec![1.0, 2.0, 3.0])).unwrap();
        s.upsert(&row(2, vec![1.0, 2.0, 3.0])).unwrap();
        assert_eq!(s.count().unwrap(), 2);
        assert_eq!(s.max_rowid().unwrap(), 5);
        assert!(s.remove(2).unwrap());
        assert!(!s.remove(2).unwrap());
    }

    #[test]
    fn shadow_rows_are_in_rowid_order_regardless_of_insertion_order() {
        let t = Vec0Table::with_rows(
            schema(),
            vec![
                row(30, vec![0.0, 0.0, 1.0]),
                row(10, vec![1.0, 0.0, 0.0]),
                row(20, vec![0.0, 1.0, 0.0]),
            ],
        )
        .unwrap();
        let shadow = t.shadow_rows().unwrap();
        assert_eq!(
            shadow.vectors.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [10, 20, 30]
        );
        assert_eq!(shadow.max_rowid, 30);
    }

    #[test]
    fn a_vector_blob_round_trips_through_big_endian_bytes() {
        let v = vec![1.0f64, -0.5, 0.25, 1e300, f64::MIN_POSITIVE];
        let blob = encode_vector(&v);
        assert_eq!(blob.len(), v.len() * 8);
        // The first 8 bytes are the big-endian IEEE-754 of 1.0, which is what
        // the engine's own record writer produces (record.rs:206).
        assert_eq!(&blob[..8], &[0x3F, 0xF0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_vector(&blob, v.len()).unwrap(), v);
    }

    #[test]
    fn decoding_a_blob_of_the_wrong_width_is_an_error() {
        let blob = encode_vector(&[1.0, 2.0]);
        assert!(decode_vector(&blob, 3).is_err());
        assert!(decode_vector(&blob, 1).is_err());
        // A zero-width table has an empty encoding, and that one round-trips.
        assert_eq!(decode_vector(&[], 0).unwrap(), Vec::<f64>::new());
    }
}

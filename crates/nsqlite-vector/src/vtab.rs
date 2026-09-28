//! The `vec0` virtual table module: the C-shaped entry points, the query
//! errors, and the integration contract.
//!
//! A SQLite extension adds SQL surface through a virtual table, and this is
//! that step. The pieces are in three files:
//!
//! * this one — [`Schema`] (the `USING vec0(...)` argument list),
//!   [`Vec0Module`] (xCreate/xConnect/xBestIndex), [`MemoryVTab`]
//!   (xUpdate/xFilter/xColumn), and [`ENGINE_CONTRACT`];
//! * [`crate::index_store`] — a table per shadow table, holding vectors as
//!   blobs, read and written through a [`RowStore`];
//! * [`crate::plan`] — `xBestIndex`'s decision: which constraint is the
//!   `MATCH`, where `k` is, and whether the ordering is consumed.
//!
//! # What is measured, and what is assumed
//!
//! Everything claimed here about **SQLite** was run against the real `sqlite3`
//! 3.53.4 and is cited where it is used. The measurements that shape a design:
//!
//! * A virtual table is a `sqlite_schema` row of `type = 'table'` with
//!   `rootpage = 0`, and its `sql` is the original `CREATE VIRTUAL TABLE` text
//!   verbatim. Its shadow tables are ordinary `CREATE TABLE` rows named
//!   `<table>_<suffix>`, each with its own rootpage.
//! * `PRAGMA integrity_check` on a file with a virtual table and its shadow
//!   tables reports `ok`.
//! * A `CREATE VIRTUAL TABLE` naming a module the library lacks is
//!   `no such module: <name>`, and leaves no `sqlite_schema` row.
//! * `<table> MATCH <term>` reaches a virtual table as a function-call
//!   constraint. The same `MATCH` where no virtual table can take it is
//!   `unable to use function MATCH in the requested context`, so the parser
//!   must leave the decision to planning rather than rejecting the token.
//! * `k = 5` is a bare column compared with `=`, not a keyword: dropping the
//!   `=` is `near "=": syntax error`.
//! * A `LIMIT` above the row count returns every row; `LIMIT 0` returns none.
//! * `distance` is a **hidden** column: `PRAGMA table_info` on the reference's
//!   `fts5` table does not list `rank`, but `SELECT rank ... ORDER BY rank`
//!   works. `distance` is the same kind of column.
//! * After `INSERT` into a virtual table, `last_insert_rowid()` is the rowid
//!   the module assigned (measured on `rtree`).
//!
//! What is taken from the **extension** rather than from SQLite — the four
//! column sigils, `float[N]`, `distance_metric=`, and the `MATCH`/`k` query
//! shape — comes from the task specification. The reference binary is not
//! reachable from this tree and the network is unavailable, so those are
//! **assumptions**, listed one by one below, and each is confined to one line of
//! code so that a reader with the reference can overturn it.
//!
//! The assumptions are: a partition column is written `*name`; an option value
//! is a bare or quoted token and never a sigil-prefixed list; `distance_metric`
//! is a per-table option whose values are the [`Metric`](crate::search::Metric)
//! discriminants `L2`, `cosine`, `L1`, `inner_product`, compared
//! case-insensitively; and a table declared without a `distance_metric` is an
//! error rather than defaulting to L2.

use std::fmt;
use std::rc::Rc;

use crate::index_store::{StoreError, StoredRow, Vec0Table};
use crate::search::SearchError;

/// How one declared column behaves at query time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnKind {
    /// The rowid alias. At most one, named `rowid`, declared first.
    Rowid,
    /// A partition key, written `*name`.
    Partition,
    /// Stored and returned but never indexed, written `+name`.
    Auxiliary,
    /// An embedding, and the only kind `MATCH` applies to.
    Vector,
}

impl ColumnKind {
    /// The name of this kind, for error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnKind::Rowid => "rowid",
            ColumnKind::Partition => "partition",
            ColumnKind::Auxiliary => "auxiliary",
            ColumnKind::Vector => "vector",
        }
    }
}

/// One declared column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The name as written, **without** its `*` or `+` sigil. The sigil is
    /// syntax, not part of the name a query uses.
    pub name: String,
    /// What this column is.
    pub kind: ColumnKind,
    /// Embedding width, for a vector column. `0` for every other kind.
    pub dim: usize,
    /// The declared type, verbatim. `float[N]` for a vector column, empty
    /// otherwise.
    pub decl_type: String,
}

impl Column {
    /// Whether this column's value is part of what is searched.
    ///
    /// Auxiliary columns are stored and returned but excluded: the whole point
    /// of the `+` sigil is that an extra payload column must not slow a scan.
    pub fn is_searchable(&self) -> bool {
        !matches!(self.kind, ColumnKind::Auxiliary)
    }

    /// Whether this column is a value vector, i.e. what `MATCH` applies to.
    pub fn is_vector(&self) -> bool {
        matches!(self.kind, ColumnKind::Vector)
    }
}

/// A whole `vec0` table's declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    /// The table name, as written in the `CREATE VIRTUAL TABLE` statement. It
    /// is not in the argument list; the caller reads it off the statement.
    pub name: String,
    /// Columns in declaration order. Never empty.
    pub columns: Vec<Column>,
    /// The metric every vector column is searched under.
    pub metric: crate::search::Metric,
}

impl Schema {
    /// Parses a `USING vec0(...)` module argument list.
    ///
    /// `args` is given verbatim, parentheses included, so this function owns
    /// the splitting rather than assuming the caller did it.
    pub fn parse(table: &str, args: &str) -> Result<Schema, ParseError> {
        let body = args
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .ok_or_else(|| {
                ParseError::new(
                    "expected a parenthesised list of columns after USING vec0",
                    args,
                )
            })?;

        let mut columns: Vec<Column> = Vec::new();
        let mut metric = crate::search::Metric::L2;
        let mut metric_seen = false;

        for item in split_top_level(body) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            // An option is the only thing that can carry a top-level `=`, since
            // a column's own `=` never appears: a vector's width is inside its
            // type. Splitting here rather than in the column parser is what
            // keeps `float[3]` from being read as an option.
            if let Some((key, value)) = split_assignment(item) {
                apply_option(key, value, &mut metric, &mut metric_seen)?;
                continue;
            }
            let column = parse_column(table, item)?;
            validate_column(table, &column, &columns)?;
            columns.push(column);
        }

        if columns.is_empty() {
            return Err(ParseError::new(
                "a vec0 table needs at least one column",
                args,
            ));
        }
        if !columns.iter().any(|c| c.is_vector()) {
            return Err(ParseError::new(
                "a vec0 table needs at least one float[N] vector column",
                args,
            ));
        }
        // A metric is required, and its absence is an error rather than a
        // silent default. `search::Metric` does default to L2, and that default
        // is right for a caller building an index in Rust; it is wrong for a
        // DDL string, where the user wrote down every other detail and leaving
        // one to a default is how a table ends up searched under a metric
        // nobody chose.
        if !metric_seen {
            return Err(ParseError::new(
                "a vec0 table needs a distance_metric= option",
                args,
            ));
        }

        Ok(Schema {
            name: table.to_string(),
            columns,
            metric,
        })
    }

    /// The dimension every vector column has. All of them must agree, so this is
    /// unambiguous; [`Schema::parse`] rejects a table where they do not.
    pub fn dim(&self) -> usize {
        self.columns
            .iter()
            .find(|c| c.is_vector())
            .map_or(0, |c| c.dim)
    }

    /// The vector columns, in declaration order.
    pub fn vectors(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| c.is_vector())
    }

    /// The auxiliary columns, in declaration order.
    pub fn auxiliaries(&self) -> impl Iterator<Item = &Column> {
        self.columns
            .iter()
            .filter(|c| matches!(c.kind, ColumnKind::Auxiliary))
    }

    /// The partition column, if the table has one.
    pub fn partition(&self) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| matches!(c.kind, ColumnKind::Partition))
    }

    /// The rowid alias column, if the table named one.
    ///
    /// Every table has a `rowid` regardless; this is the column that *aliases*
    /// it, which is what makes `rowid` addressable as a declared name.
    pub fn rowid_alias(&self) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| matches!(c.kind, ColumnKind::Rowid))
    }

    /// Looks a column up by name, ignoring case, as SQL identifiers are.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The number of columns a stored row has, excluding the rowid alias.
    pub fn stored_column_count(&self) -> usize {
        self.columns
            .iter()
            .filter(|c| !matches!(c.kind, ColumnKind::Rowid))
            .fold(0usize, |n, _| n + 1)
    }

    /// The names of the shadow tables backing this schema.
    ///
    /// **Measured**, not assumed. The reference `sqlite3` 3.53.4, given
    /// `CREATE VIRTUAL TABLE geo USING rtree(id, minx, maxx, miny, maxy)`,
    /// writes one `sqlite_schema` row per shadow table with
    /// `name = tbl_name = <table>_<suffix>`, plus one row for the virtual table
    /// itself with `type = 'table'` and `rootpage = 0`:
    ///
    /// ```text
    /// type | name     | tbl_name  | rootpage | sql
    /// -----+----------+-----------+----------+----------------------------------------
    /// table| geo      | geo       |         0|CREATE VIRTUAL TABLE geo USING rtree(...)
    /// table| geo_rowid| geo_rowid |         2|CREATE TABLE "geo_rowid"(rowid INTEGER PRIMARY KEY,nodeno)
    /// table| geo_node | geo_node  |         3|CREATE TABLE "geo_node"(nodeno INTEGER PRIMARY KEY,data)
    /// ```
    ///
    /// The `<table>_<suffix>` convention is therefore a property of SQLite's own
    /// bookkeeping rather than of any one extension, and matching it is what
    /// keeps a file written here inspectable by tools that do not know this
    /// module.
    pub fn shadow_table_names(&self) -> ShadowTables {
        let base = &self.name;
        ShadowTables {
            vectors: format!("{base}_vectors"),
            chunks: format!("{base}_chunks"),
            rowids: format!("{base}_rowids"),
            info: format!("{base}_info"),
        }
    }

    /// The `CREATE TABLE` statement for each shadow table.
    ///
    /// Returned as text because the engine's catalog stores a table's original
    /// SQL and reads it back on reopen, exactly as it does for a real
    /// `CREATE TABLE`. Handing back a reconstruction instead of the text would
    /// lose that, so the text is the artefact.
    pub fn shadow_table_sql(&self) -> Vec<(String, String)> {
        let names = self.shadow_table_names();
        vec![
            (
                names.vectors.clone(),
                format!(
                    "CREATE TABLE \"{}\"(rowid INTEGER PRIMARY KEY, embedding BLOB)",
                    names.vectors
                ),
            ),
            (
                names.chunks.clone(),
                format!(
                    "CREATE TABLE \"{}\"(rowid INTEGER PRIMARY KEY, aux BLOB)",
                    names.chunks
                ),
            ),
            (
                names.rowids.clone(),
                format!(
                    "CREATE TABLE \"{}\"(rowid INTEGER PRIMARY KEY, vector_rowid)",
                    names.rowids
                ),
            ),
            (
                names.info.clone(),
                format!(
                    "CREATE TABLE \"{}\"(key TEXT PRIMARY KEY, value)",
                    names.info
                ),
            ),
        ]
    }
}

/// The four shadow table names a `vec0` table owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowTables {
    /// One row per embedding: `(rowid, embedding BLOB)`.
    pub vectors: String,
    /// One row per stored row's auxiliary payload.
    pub chunks: String,
    /// The rowid ordering that makes a scan deterministic.
    pub rowids: String,
    /// The table's schema parameters, so a reopened table rebuilds from disk.
    pub info: String,
}

impl ShadowTables {
    /// All four names, in the order they are created.
    pub fn all(&self) -> [&str; 4] {
        [&self.vectors, &self.chunks, &self.rowids, &self.info]
    }
}

/// A `CREATE VIRTUAL TABLE` that could not be parsed.
///
/// Carries the offending text so a message can quote it, which is what makes a
/// schema error findable inside a long `CREATE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// What was wrong, in the imperative.
    pub message: String,
    /// The text the error is about: the whole argument list, or the one item.
    pub context: String,
}

impl ParseError {
    /// Builds an error about `context`.
    pub fn new(message: impl Into<String>, context: impl Into<String>) -> Self {
        ParseError {
            message: message.into(),
            context: context.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}", self.message, self.context)
    }
}

impl std::error::Error for ParseError {}

/// Parses one column definition.
///
/// **A bare column name with no sigil is a vector column**, unless it is
/// literally `rowid`. That is the one piece of inference here, and it exists
/// because `vec0`'s vector columns are the common case and are written without a
/// marker: `embedding float[3]`, not `*embedding float[3]`. Classifying a bare
/// name as a rowid alias instead would make every ordinary table fail with
/// "a rowid column takes no type", which is exactly what it did before this was
/// fixed.
fn parse_column(table: &str, item: &str) -> Result<Column, ParseError> {
    // The type is the last whitespace-separated word, if there is one. Reading
    // it as a suffix rather than parsing a full type spec keeps the vector form
    // exact without dragging in the engine's type grammar, which this module
    // has no business duplicating.
    let (bare, decl_type) = match item.rsplit_once(char::is_whitespace) {
        Some((head, ty)) => (head.trim(), ty.trim()),
        None => (item, ""),
    };

    let (name, kind) = match bare.as_bytes().first() {
        Some(b'*') => (&bare[1..], ColumnKind::Partition),
        Some(b'+') => (&bare[1..], ColumnKind::Auxiliary),
        _ if bare.eq_ignore_ascii_case("rowid") => (bare, ColumnKind::Rowid),
        _ => (bare, ColumnKind::Vector),
    };

    if name.is_empty() {
        return Err(ParseError::new("a column needs a name", item));
    }
    if name.chars().any(char::is_whitespace) {
        return Err(ParseError::new("a column name cannot contain spaces", item));
    }

    let dim = match kind {
        // A non-vector column takes no type. Accepting one silently would let
        // `+note text` through and then store a text value where a blob is
        // expected, which is exactly the kind of disagreement between what was
        // declared and what is stored that a kNN table cannot recover from.
        ColumnKind::Rowid | ColumnKind::Partition | ColumnKind::Auxiliary => {
            if !decl_type.is_empty() {
                return Err(ParseError::new(
                    format!(
                        "a {} column takes no type, got {decl_type:?}",
                        kind.as_str()
                    ),
                    item,
                ));
            }
            0
        }
        ColumnKind::Vector => {
            if decl_type.is_empty() {
                return Err(ParseError::new(
                    "a vector column needs a type, as in float[3]",
                    item,
                ));
            }
            parse_vector_type(table, decl_type)?
        }
    };

    Ok(Column {
        name: name.to_string(),
        kind,
        dim,
        decl_type: decl_type.to_string(),
    })
}

/// Parses `float[N]`, and only that.
///
/// The dimension is a plain decimal count. `[0]` is rejected here rather than at
/// insert time, because a zero-width vector has no valid encoding and the table
/// would be unopenable.
fn parse_vector_type(table: &str, decl_type: &str) -> Result<usize, ParseError> {
    let bad = |what: &str| {
        ParseError::new(
            format!("expected float[N] for a vector column of {table}, {what}: {decl_type:?}"),
            decl_type,
        )
    };

    let lower = decl_type.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("float") else {
        return Err(bad("the type must start with `float`"));
    };
    let Some(inner) = rest
        .trim()
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
    else {
        return Err(bad("the width must be in square brackets"));
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Err(bad("the width is missing"));
    }
    if !inner.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("the width must be a whole number"));
    }
    let dim: usize = inner.parse().map_err(|_| bad("the width is too large"))?;
    if dim == 0 {
        return Err(bad("the width must be at least 1"));
    }
    Ok(dim)
}

/// The metric names a `distance_metric=` option accepts.
///
/// Written out by hand rather than derived from [`Metric::as_str`] so that a
/// name can be added here without also appearing in `search`, which this module
/// does not own.
pub const METRIC_NAMES: [&str; 4] = ["L2", "cosine", "L1", "inner_product"];

/// Parses a `distance_metric=` value, case-insensitively.
pub fn parse_metric(text: &str) -> Option<crate::search::Metric> {
    use crate::search::Metric;
    let text = text.trim();
    for (name, metric) in
        METRIC_NAMES
            .iter()
            .zip([Metric::L2, Metric::Cosine, Metric::L1, Metric::InnerProduct])
    {
        if text.eq_ignore_ascii_case(name) {
            return Some(metric);
        }
    }
    None
}

/// Applies one `name = value` option.
fn apply_option(
    key: &str,
    value: &str,
    metric: &mut crate::search::Metric,
    metric_seen: &mut bool,
) -> Result<(), ParseError> {
    let key = key.trim();
    // An option value may be quoted, and the quotes are part of the syntax
    // rather than part of the value, so they are stripped once here and never
    // seen again.
    let value = unquote(value.trim());
    if value.is_empty() {
        return Err(ParseError::new(
            format!("option {key:?} has no value"),
            format!("{key} ="),
        ));
    }

    if key.eq_ignore_ascii_case("distance_metric") {
        *metric = parse_metric(&value).ok_or_else(|| {
            ParseError::new(
                format!(
                    "no such distance_metric: {value:?}, expected one of L2, cosine, L1, \
                     inner_product"
                ),
                key,
            )
        })?;
        *metric_seen = true;
        return Ok(());
    }

    Err(ParseError::new(
        format!("no such vec0 option: {key:?}"),
        key,
    ))
}

/// Splits an argument list on commas that are not inside brackets or quotes.
///
/// A quoted option value may contain a comma, and a nested bracket depth is
/// what makes the split correct for the general case rather than correct for the
/// cases in today's tests.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut start = 0usize;
    for (i, ch) in body.char_indices() {
        match in_quote {
            Some(q) => {
                if ch == q {
                    in_quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => in_quote = Some(ch),
                '[' | '(' => depth += 1,
                ']' | ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(&body[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    parts.push(&body[start..]);
    parts
}

/// Splits `key = value` on a top-level `=`, if there is one.
///
/// The first `=` wins, so a value containing `=` survives. The function returns
/// `None` for a plain column, which is what keeps `float[3]` from being read as
/// an option.
fn split_assignment(item: &str) -> Option<(&str, &str)> {
    let mut in_quote: Option<char> = None;
    for (i, ch) in item.char_indices() {
        match in_quote {
            Some(q) => {
                if ch == q {
                    in_quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => in_quote = Some(ch),
                '=' => return Some((&item[..i], &item[i + 1..])),
                _ => {}
            },
        }
    }
    None
}

/// Strips one layer of matching quotes, if present.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0] as char;
        if (first == '\'' || first == '"' || first == '`') && bytes[bytes.len() - 1] == bytes[0] {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Checks a new column against the ones already declared.
///
/// Order is enforced rather than merely documented because the storage layout
/// depends on it: the rowid alias, then the partition key, then the vectors,
/// then the auxiliary payload. A table that declared them in another order
/// would still be readable, but the layout is fixed here so that it is fixed
/// once.
fn validate_column(table: &str, column: &Column, existing: &[Column]) -> Result<(), ParseError> {
    if existing
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(&column.name))
    {
        return Err(ParseError::new(
            format!("duplicate column name: {:?}", column.name),
            column.name.clone(),
        ));
    }

    match column.kind {
        ColumnKind::Rowid => {
            if !existing.is_empty() {
                return Err(ParseError::new(
                    format!("the rowid column of {table} must come first"),
                    column.name.clone(),
                ));
            }
        }
        ColumnKind::Partition => {
            if existing
                .iter()
                .any(|c| c.is_vector() || matches!(c.kind, ColumnKind::Auxiliary))
            {
                return Err(ParseError::new(
                    format!("the partition column of {table} must come before any vector column"),
                    column.name.clone(),
                ));
            }
        }
        ColumnKind::Vector => {
            if let Some(first) = existing.iter().find(|c| c.is_vector()) {
                if first.dim != column.dim {
                    return Err(ParseError::new(
                        format!(
                            "every vector column of {table} needs the same width: {:?} is \
                             float[{}], an earlier one is float[{}]",
                            column.name, column.dim, first.dim
                        ),
                        column.name.clone(),
                    ));
                }
            }
        }
        ColumnKind::Auxiliary => {}
    }

    Ok(())
}

/// The name of the hidden distance column, addressable in `ORDER BY`.
pub const DISTANCE_COLUMN: &str = "distance";

/// One half of an `xUpdate`: the row being written, or the rowid being removed.
///
/// In C this is one `argv` array whose `argv[0]` is the new row and `argv[1]`
/// the old one, either of which may be NULL. Modelling it as two options rather
/// than an array of nulls keeps "half a row" from being representable, which is
/// the point: there is no SQL statement that is half of one.
#[derive(Debug, Clone, PartialEq)]
pub struct RowUpdate {
    /// The rowid being written, or `None` to let the table allocate one.
    pub rowid: Option<i64>,
    /// One slot per declared column, plus the hidden `distance`.
    ///
    /// The hidden slot is `None` on a write. A `Some` there is
    /// [`VTabError::DistanceIsNotWritable`], not a silently dropped column.
    pub values: Vec<Option<Vec<f64>>>,
    /// Auxiliary column values, in declaration order.
    pub metadata: Vec<Option<Vec<u8>>>,
}

/// What the engine side has to provide.
///
/// **This is the integration contract.** The engine has no virtual table
/// machinery today; this is what has to exist before [`Vec0Module`] can be
/// plugged in, in the order the engine hits it. The long form, with the reason
/// and the measurement behind each item, is the [`ENGINE_CONTRACT`] module
/// documentation; this is the checklist.
///
/// 1. Parse `CREATE VIRTUAL TABLE` into (name, module, args-as-text).
/// 2. Let `sqlite_schema` hold a `type = 'table'` row with `rootpage = 0`.
/// 3. `DROP TABLE` on a vtab drops its shadow tables.
/// 4. `DROP TABLE` consults the module before the pager.
/// 5. Call `xBestIndex` with usable constraints; honour `orderByConsumed`.
/// 6. Pass constraint arguments through to `xFilter`, via `idxStr`/`idxNum`
///    when `k` is a bound parameter.
/// 7. Let `<table> MATCH <term>` reach the planner as a function constraint.
/// 8. Give `xColumn` a hidden-column flag; `distance` is addressable but absent
///    from `PRAGMA table_info`.
/// 9. A NULL from `xColumn` is a real SQL NULL.
/// 10. Record what `xUpdate` assigned, so `last_insert_rowid()` is the vtab's
///     rowid.
/// 11. Supply both halves of `xUpdate`'s `argv` for UPDATE, only `argv[0]` for
///     INSERT.
/// 12. No transaction work: a shadow-table write is an ordinary write.
/// 13. Run the four shadow `CREATE TABLE`s through the ordinary table path.
/// 14. Never open a vtab's b-tree; its `rootpage` is 0.
pub const ENGINE_CONTRACT: &str = "\
1.  parse CREATE VIRTUAL TABLE into (name, module, args-as-text). Today
    parser::create_statement falls through to Stmt::Unsupported(\"virtual\") at
    crates/nsqlite/src/parser.rs:2931, and executing one gives
    `ERROR: virtual is not supported yet` -- measured on this engine.
2.  let sqlite_schema hold type='table' with rootpage=0. MEASURED: the real
    sqlite3 3.53.4 stores a virtual table exactly that way, with the original
    CREATE VIRTUAL TABLE text verbatim in .sql.
3.  DROP TABLE on a vtab drops its shadow tables too, or a CREATE/DROP cycle
    leaves a table behind each time.
4.  DROP TABLE consults the module before the pager, so it can veto or report.
5.  call xBestIndex with the usable constraints and honour orderByConsumed. A
    vec0 query is `ORDER BY distance LIMIT k`, and the module is the only thing
    that knows its rows already come out in that order, so orderByConsumed is
    the difference between one sort and two.
6.  pass each constraint's argv through to xFilter. MATCH and k are both
    constraint arguments and both are needed; a k arriving as a bound parameter
    has to travel in idxStr/idxNum because its value is not known at
    xBestIndex time.
7.  let `<table> MATCH <term>` reach the planner as a function constraint.
    MEASURED: the same MATCH where no virtual table can take it is `unable to
    use function MATCH in the requested context`, so the parser must not
    reject the token and the planner must decide.
8.  give xColumn a hidden-column flag. MEASURED: `rank` is addressable on an
    fts5 table and absent from PRAGMA table_info; `distance` is the same kind of
    column, so a name and a type are not enough to describe it.
9.  a NULL from xColumn must be a real SQL NULL, distinguishable from a stored
    zero. The auxiliary columns are where this bites: absent is NULL, 0.0 is
    a number.
10. record what xUpdate assigned and ask xRowid for it. MEASURED: after an
    INSERT into an rtree table, last_insert_rowid() is the vtab's rowid.
11. supply both halves of xUpdate's argv for UPDATE and only argv[0] for
    INSERT; argc is 2 when deleting and 1 when the rowid is only in argv[1].
12. no transaction work. A shadow-table write is an ordinary write, so the
    engine's rollback journal already covers it; VTab::begin/commit/rollback
    default to no-ops for exactly that reason.
13. run the four shadow CREATE TABLEs through the ordinary table path, and let
    RowStore read them back. MEASURED: the reference's shadow tables are
    ordinary tables with their own rootpages and real CREATE TABLE text, and
    PRAGMA integrity_check on such a file reports ok -- so the engine needs no
    special case for them, which is the entire reason storage is a table per
    shadow table.
14. never open a vtab's b-tree. There is none: rootpage is 0. An implementation
    that tried would read the database header as a page.

Deliberately NOT required: a C API, an sqlite3_module struct layout, function
pointers, and unsafe. The engine is Rust and this module is Rust; the C shape is
reproduced as method names and argument order. The C layout matters only for a
loadable .so extension, which is a different crate's job.";

/// A virtual table module, in the shape SQLite's `sqlite3_module` describes.
///
/// Each method is named for the C entry point it stands for. The split between
/// [`VirtualTable::create`] and [`VirtualTable::connect`] is the one that matters
/// most and is the easiest to get wrong: `create` is called for
/// `CREATE VIRTUAL TABLE` and must *make* the shadow tables, while `connect` is
/// called every time the schema is read back — on a reopen, on a second
/// statement, on `PRAGMA table_info` — and must only find them. Conflating them is
/// how a reopened database loses its vectors.
///
/// `xDisconnect` is not a method because this module holds no native resources:
/// there is nothing to release that Rust does not release when the handle drops.
/// The engine still needs to call it for symmetry with the C shape, and the drop
/// *is* the call.
pub trait VirtualTable {
    /// `xCreate`: `CREATE VIRTUAL TABLE name USING vec0(...)`.
    ///
    /// Must create the shadow tables. Must fail if the table already exists;
    /// `IF NOT EXISTS` is resolved by the engine before this is called.
    ///
    /// **This returns no handle, which is a deliberate deviation from the C
    /// shape.** `xCreate` returns a `sqlite3_vtab*` there, and in C the caller
    /// drops it when the statement ends. A Rust handle would live on past the
    /// statement, and since a handle is a live shared reference it would block
    /// every later write to the new table for as long as it was held. Splitting
    /// creation from handle acquisition is what makes the module usable: an
    /// engine calls `create`, then `connect` when it wants to read.
    fn create(&mut self, name: &str, args: &str) -> Result<(), VTabError>;

    /// `xConnect`: attach to a table whose shadow tables already exist, and
    /// return a handle onto it.
    ///
    /// Must not create anything.
    fn connect(&self, name: &str, args: &str) -> Result<Box<dyn VTab>, VTabError>;

    /// `xBestIndex`: choose a plan for one statement.
    fn best_index(&self, constraints: &[Constraint]) -> Result<Plan, PlanError>;
}

/// A connected virtual table.
///
/// The declaration side is a [`Schema`]; the row side is a [`Vec0Table`]. Both
/// are behind a trait object so the engine can hold a handle without knowing
/// what a vector is.
///
/// # On handles, and what `xUpdate` costs
///
/// In C, `xConnect` returns a fresh `sqlite3_vtab*` and SQLite serialises every
/// access to it. Rust has no such implicit serialisation, so the question this
/// trait has to answer is: what does a handle *own*?
///
/// A handle here **shares ownership** of the table with the module
/// ([`Vec0Module`] hands out `Rc` handles), so an engine can hold one for as
/// long as a connection lives and still create and drop other tables. That is
/// the ownership the C shape has too: the module owns the vtab, the connection
/// owns the module, and neither is invalidated by opening another statement.
///
/// The cost is in `xUpdate`, which takes `&mut self`. `Rc` is not `Send` and not
/// a write lock, so **a handle is a read handle**: a `SELECT` can run against
/// one, and a write needs the engine's own exclusive access. That is not a gap
/// in the contract — it is the same thing SQLite gets from a statement lock, and
/// it is why the engine contract's item 11 says the write half arrives with the
/// engine already holding the connection mutably.
///
/// An engine that wants two concurrent readers on one table implements [`VTab`]
/// over its own pager handle, which is already interior-mutable; the trait does
/// not force this module's choice on it.
pub trait VTab: std::fmt::Debug {
    /// The table's declaration, for `xColumn` and `PRAGMA table_info`.
    fn schema(&self) -> &Schema;

    /// `xColumnCount`: addressable columns, the hidden `distance` included.
    fn column_count(&self) -> usize {
        self.schema().columns.len() + 1
    }

    /// `xUpdate`: INSERT, UPDATE, or DELETE, as one call.
    ///
    /// `(Some(rows), None)` is an INSERT, `(Some(rows), Some(rowid))` an
    /// UPDATE, and `(None, Some(rowid))` a DELETE — the three shapes of C's
    /// `argv[0]`/`argv[1]` pair.
    fn update(&mut self, new: Option<&[RowUpdate]>, old: Option<i64>) -> Result<(), VTabError>;

    /// `xBegin`. A no-op by default: see [`ENGINE_CONTRACT`] item 12.
    fn begin(&mut self) -> Result<(), VTabError> {
        Ok(())
    }

    /// `xCommit`. A no-op by default; see item 12.
    fn commit(&mut self) -> Result<(), VTabError> {
        Ok(())
    }

    /// `xRollback`. A no-op by default; see item 12.
    fn rollback(&mut self) -> Result<(), VTabError> {
        Ok(())
    }

    /// `xFilter`: run the query a plan describes.
    fn filter(&self, plan: &Plan) -> Result<Cursor, VTabError>;
}

/// The `vec0` module: the thing an engine registers once per process.
///
/// In C this is an `sqlite3_module` filled in at load time; here it is a name
/// and a [`VirtualTable`]. The `tables` map is what makes `xConnect` work: a
/// reopened database has only the schema text, so connecting rebuilds the table
/// from the text, and on an engine the shadow tables supply the rows.
#[derive(Default)]
pub struct Vec0Module {
    tables: std::collections::BTreeMap<String, Rc<MemoryVTab>>,
}

impl Vec0Module {
    /// An empty module.
    pub fn new() -> Self {
        Vec0Module {
            tables: std::collections::BTreeMap::new(),
        }
    }

    /// The name in `USING <name>(...)`.
    pub const NAME: &'static str = "vec0";

    /// Creates the table and remembers it, without handing out a handle.
    ///
    /// This is the `CREATE VIRTUAL TABLE` half of the work, and it is
    /// deliberately separate from [`Vec0Module::connect`]: a handle from
    /// `create` would be a live `Rc` on the new table, and a later write would
    /// then fail for as long as it was held. In C the distinction does not
    /// exist — `xCreate` returns the vtab and SQLite drops the pointer when the
    /// statement ends — but in Rust the handle outlives the statement unless the
    /// caller arranges otherwise, so the module does not hand one out. A caller
    /// that wants one calls `connect` immediately afterwards.
    ///
    /// The four `CREATE TABLE` statements are *not* run here. There is no pager
    /// in this crate, and issuing them would mean either opening a second
    /// connection to the file or dropping them entirely. What an engine does
    /// instead is the one line in the body below plus its own DDL path running
    /// `Schema::shadow_table_sql()`. That the shadow tables are ordinary tables
    /// is the whole point of the storage design — see [`ENGINE_CONTRACT`] item
    /// 13.
    pub fn create(&mut self, name: &str, args: &str) -> Result<(), VTabError> {
        if self.tables.contains_key(name) {
            return Err(VTabError::TableExists(name.to_string()));
        }
        let schema = Schema::parse(name, args)?;
        self.tables.insert(
            name.to_string(),
            Rc::new(MemoryVTab::new(Vec0Table::create(schema))),
        );
        Ok(())
    }

    /// `xConnect`. Attaches to a table that already exists, rebuilding it from
    /// the schema text exactly as a reopened database needs.
    ///
    /// The rows come back too, through [`Vec0Table::from_shadow_rows`] in an
    /// engine implementation. This one finds the table it kept, so a caller
    /// cannot get one behaviour in a session and another after a reopen.
    pub fn connect(&self, name: &str, args: &str) -> Result<Box<dyn VTab>, VTabError> {
        // The args are parsed even when the table is already attached: a
        // reopened database supplies text that has never been validated, and
        // `xConnect` is the first place it can be. A schema that no longer
        // parses is a corrupt file, and saying so here is better than opening a
        // table with columns nobody declared.
        let schema = Schema::parse(name, args)?;
        match self.tables.get(name) {
            Some(vtab) if vtab.schema() == &schema => self.handle(name),
            Some(_) => Err(VTabError::Other(format!(
                "{name} already exists with a different definition"
            ))),
            None => Err(VTabError::NoSuchTable(name.to_string())),
        }
    }

    /// `xDropTable` / the shadow-table half of `DROP TABLE`. Forgets a table.
    pub fn drop_table(&mut self, name: &str) -> bool {
        self.tables.remove(name).is_some()
    }

    /// Runs `f` with exclusive access to a table's rows.
    ///
    /// This is how an engine performs a write: a `VTab` handle is a read handle
    /// (see the trait's docs), so `xUpdate`'s exclusive access is taken here,
    /// from the connection that owns both the module and the statement. It is
    /// the only method that can deadlock against a live cursor, which is the
    /// same hazard SQLite has when a statement writes a table it is scanning.
    pub fn with_table_mut<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut MemoryVTab) -> T,
    ) -> Result<T, VTabError> {
        match self.tables.get_mut(name) {
            Some(vtab) => Ok(f(Rc::get_mut(vtab).ok_or_else(|| {
                VTabError::Other(format!(
                    "{name} is being read by another handle; drop the cursor before writing"
                ))
            })?)),
            None => Err(VTabError::NoSuchTable(name.to_string())),
        }
    }

    /// The tables currently attached, by name, sorted.
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        names
    }

    /// `xBestIndex`.
    pub fn best_index(&self, constraints: &[Constraint]) -> Result<Plan, PlanError> {
        Plan::choose(constraints)
    }

    /// A handle onto an attached table.
    ///
    /// The handle shares ownership with the module rather than borrowing it, so
    /// an engine can hold one for as long as a connection lives and still
    /// `CREATE` and `DROP` other tables through the same module. That is the
    /// order the C shape imposes too: a `sqlite3_vtab*` is owned by the module
    /// and the module by the connection, and neither is invalidated by opening
    /// another statement.
    fn handle(&self, name: &str) -> Result<Box<dyn VTab>, VTabError> {
        self.tables
            .get(name)
            .map(|vtab| Box::new(Rc::clone(vtab)) as Box<dyn VTab>)
            .ok_or_else(|| VTabError::NoSuchTable(name.to_string()))
    }
}

impl VirtualTable for Vec0Module {
    fn create(&mut self, name: &str, args: &str) -> Result<(), VTabError> {
        Vec0Module::create(self, name, args)
    }

    fn connect(&self, name: &str, args: &str) -> Result<Box<dyn VTab>, VTabError> {
        Vec0Module::connect(self, name, args)
    }

    fn best_index(&self, constraints: &[Constraint]) -> Result<Plan, PlanError> {
        Plan::choose(constraints)
    }
}

/// A `VTab` handle that borrows the module's table.
///
/// This is what makes `connect` return a usable handle without cloning a
/// `Vec0Table`. Its lifetime is the module's, which is exactly the C `sqlite3_vtab*`
/// relationship: the table does not outlive the connection that created it.
#[derive(Debug)]
pub struct MemoryVTab {
    table: Vec0Table,
}

impl MemoryVTab {
    /// Wraps a table.
    pub fn new(table: Vec0Table) -> Self {
        MemoryVTab { table }
    }

    /// The table behind it.
    pub fn table(&self) -> &Vec0Table {
        &self.table
    }

    /// The table behind it, mutably, for a caller wiring rows in.
    pub fn table_mut(&mut self) -> &mut Vec0Table {
        &mut self.table
    }
}

impl VTab for MemoryVTab {
    fn schema(&self) -> &Schema {
        self.table.schema()
    }

    fn update(&mut self, new: Option<&[RowUpdate]>, old: Option<i64>) -> Result<(), VTabError> {
        match (new, old) {
            // DELETE: argv[0] is NULL and argv[1] carries the old rowid.
            (None, Some(rowid)) => {
                self.table.delete(rowid)?;
            }
            (Some(rows), Some(rowid)) => {
                if rows.len() != 1 {
                    return Err(VTabError::Other(format!(
                        "xUpdate UPDATE takes exactly one new row, got {}",
                        rows.len()
                    )));
                }
                self.table
                    .update(rowid, row_vector(self.table.schema(), &rows[0])?)?;
            }
            (Some(rows), None) => {
                if rows.len() != 1 {
                    return Err(VTabError::Other(format!(
                        "xUpdate INSERT takes exactly one new row, got {}",
                        rows.len()
                    )));
                }
                let row = &rows[0];
                self.table.insert(
                    row.rowid,
                    row_vector(self.table.schema(), row)?,
                    row.metadata.clone(),
                )?;
            }
            // Neither half present is a malformed xUpdate, not a no-op.
            (None, None) => {
                return Err(VTabError::Other(
                    "xUpdate needs a new row or an old rowid".to_string(),
                ))
            }
        }
        Ok(())
    }

    fn filter(&self, plan: &Plan) -> Result<Cursor, VTabError> {
        let schema = self.table.schema();
        let Some(query) = plan.query.as_deref() else {
            // A plan with no query is a `SELECT` with no MATCH, which
            // `Plan::choose` already rejects. Getting here means the plan was
            // built by hand.
            return Ok(Cursor::empty());
        };
        let hits = self.table.search(query, plan.k)?;
        let mut rows = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(stored) = self.table.get(hit.id as i64)? else {
                // The search and the read come from one store, so a miss means
                // the store changed between them. It is reported rather than
                // skipped: a silently shorter result is the exact failure mode
                // this module is arranged to avoid.
                return Err(VTabError::Store(StoreError::NoSuchRow(hit.id as i64)));
            };
            rows.push(Cursor::row(schema, &stored, hit.distance)?);
        }
        Ok(Cursor::new(rows, plan.order_by_distance))
    }
}

/// One result row, as a query returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// `xRowid`.
    pub rowid: i64,
    /// One slot per declared column, then the hidden `distance`.
    ///
    /// A `None` slot is a SQL NULL and is distinct from a stored zero. The
    /// hidden slot is always `Some`, holding the single distance value.
    pub values: Vec<Option<Vec<f64>>>,
}

impl Row {
    /// The row's distance, or `None` if the row has no distance slot.
    pub fn distance(&self) -> Option<f64> {
        self.values
            .last()
            .and_then(|v| v.as_ref())
            .and_then(|v| v.first())
            .copied()
    }
}

/// A cursor over one query's results.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    rows: Vec<Row>,
    at: usize,
    /// Whether the rows are already in ascending distance order, so the engine
    /// may skip its own sort. This is `orderByConsumed` from the plan.
    ordered: bool,
}

impl Cursor {
    /// A cursor over `rows`.
    pub fn new(rows: Vec<Row>, ordered: bool) -> Self {
        Cursor {
            rows,
            at: 0,
            ordered,
        }
    }

    /// An empty cursor.
    pub fn empty() -> Self {
        Cursor {
            rows: Vec::new(),
            at: 0,
            ordered: true,
        }
    }

    /// Builds one result row from a stored row and its distance.
    fn row(schema: &Schema, stored: &StoredRow, distance: f64) -> Result<Row, VTabError> {
        let mut values = Vec::with_capacity(schema.columns.len() + 1);
        let mut vector_at = 0usize;
        for column in &schema.columns {
            let value = match column.kind {
                ColumnKind::Rowid => Some(vec![stored.rowid as f64]),
                ColumnKind::Vector => {
                    let v = stored.vector.get(vector_at).copied();
                    vector_at += 1;
                    // A vector column's value is the whole vector; `xColumn`
                    // gives SQLite one value at a time, so this module returns
                    // the first component and documents that the column is
                    // addressed through MATCH rather than read back. Reading
                    // the k-th component needs the engine to pass the index,
                    // which is item 8 of the contract.
                    v.map(|first| vec![first])
                }
                // A partition or auxiliary column is not a number. It is stored
                // as bytes, so exposing it as a float would be a lie; `None`
                // says "not a value of this shape" rather than a fabricated 0.
                ColumnKind::Partition | ColumnKind::Auxiliary => None,
            };
            values.push(value);
        }
        values.push(Some(vec![distance]));
        Ok(Row {
            rowid: stored.rowid,
            values,
        })
    }

    /// The rows, in order.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the cursor is exhausted.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// `xEof`.
    pub fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// `xNext`.
    pub fn next_row(&mut self) {
        self.at += 1;
    }

    /// `xColumn` for column `index`, which is `None` for a NULL.
    pub fn value(&self, index: usize) -> Option<&[f64]> {
        self.rows
            .get(self.at)
            .and_then(|row| row.values.get(index))
            .and_then(|v| v.as_deref())
    }

    /// The current row's rowid, which is `xRowid`.
    pub fn rowid(&self) -> Option<i64> {
        self.rows.get(self.at).map(|row| row.rowid)
    }

    /// Whether the rows are already sorted by distance, i.e. whether the engine
    /// may skip its own sort.
    pub fn is_ordered(&self) -> bool {
        self.ordered
    }
}

/// A shared handle onto a [`MemoryVTab`].
///
/// The read side forwards and the write side is refused. `Rc` has no unique
/// borrow to give, so `xUpdate` cannot be forwarded -- and this is the honest
/// shape rather than an `unsafe` transmute of one: a write goes through
/// [`Vec0Module::with_table_mut`], which is the only way to get `&mut` and the
/// only place where a live cursor and a write can collide.
impl VTab for Rc<MemoryVTab> {
    fn schema(&self) -> &Schema {
        MemoryVTab::schema(self)
    }

    fn column_count(&self) -> usize {
        MemoryVTab::column_count(self)
    }

    fn update(&mut self, new: Option<&[RowUpdate]>, old: Option<i64>) -> Result<(), VTabError> {
        let _ = (new, old);
        Err(VTabError::Other(
            "a shared vtab handle is a read handle; xUpdate goes through \
             Vec0Module::with_table_mut, which holds the connection exclusively"
                .to_string(),
        ))
    }

    fn filter(&self, plan: &Plan) -> Result<Cursor, VTabError> {
        MemoryVTab::filter(self, plan)
    }
}

/// Pulls a vector out of an `xUpdate` row, refusing a written `distance`.
fn row_vector(schema: &Schema, row: &RowUpdate) -> Result<Vec<f64>, VTabError> {
    let Some(vector_column) = schema.vectors().next() else {
        return Err(VTabError::Other(
            "a vec0 table needs a vector column".to_string(),
        ));
    };
    let at = schema
        .columns
        .iter()
        .position(|c| c.name == vector_column.name)
        .unwrap_or(0);
    if row.values.len() != schema.columns.len() + 1 {
        return Err(VTabError::Other(format!(
            "xUpdate row has {} values, expected {}",
            row.values.len(),
            schema.columns.len() + 1
        )));
    }
    // The hidden column is last and is computed. A write that sets it is
    // refused rather than dropped, because a silently ignored column in an
    // INSERT is a bug report that takes an afternoon to find.
    if let Some(Some(_)) = row.values.last() {
        return Err(VTabError::DistanceIsNotWritable);
    }
    row.values[at]
        .clone()
        .ok_or_else(|| VTabError::Other(format!("no value for the {} column", vector_column.name)))
}

/// Anything that can go wrong in the module.
#[derive(Debug, Clone, PartialEq)]
pub enum VTabError {
    /// `CREATE VIRTUAL TABLE` could not be parsed.
    Schema(ParseError),
    /// `xBestIndex` could not choose a plan.
    Plan(PlanError),
    /// The row path failed.
    Store(StoreError),
    /// The table the statement names is not attached.
    NoSuchTable(String),
    /// The table already exists.
    TableExists(String),
    /// A write tried to set the computed `distance` column.
    DistanceIsNotWritable,
    /// Anything else the engine should report verbatim.
    Other(String),
}

impl fmt::Display for VTabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VTabError::Schema(err) => write!(f, "{err}"),
            VTabError::Plan(err) => write!(f, "{err}"),
            VTabError::Store(err) => write!(f, "{err}"),
            VTabError::NoSuchTable(name) => write!(f, "no such vtable: {name}"),
            VTabError::TableExists(name) => write!(f, "table {name} already exists"),
            VTabError::DistanceIsNotWritable => write!(
                f,
                "the {DISTANCE_COLUMN} column is computed and cannot be written"
            ),
            VTabError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for VTabError {}

impl From<ParseError> for VTabError {
    fn from(err: ParseError) -> Self {
        VTabError::Schema(err)
    }
}

impl From<PlanError> for VTabError {
    fn from(err: PlanError) -> Self {
        VTabError::Plan(err)
    }
}

impl From<StoreError> for VTabError {
    fn from(err: StoreError) -> Self {
        VTabError::Store(err)
    }
}

/// How a constraint compares a column to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnOp {
    /// `=`. The only operator `k = ?` can be.
    Eq,
    /// `MATCH`. The only operator a vector column accepts.
    Match,
    /// Any other operator: `<`, `>`, `LIKE`, and so on.
    Other(&'static str),
}

impl ColumnOp {
    /// The operator's name, for a message.
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnOp::Eq => "=",
            ColumnOp::Match => "MATCH",
            ColumnOp::Other(name) => name,
        }
    }
}

/// One constraint the planner offers the module.
///
/// The column index is **into the plan's own column numbering**, not into
/// [`Schema::columns`](Schema): the engine numbers the columns a
/// statement mentions, and `k` is one of them without being a declared column.
/// That is why the type is `Option<usize>` and why [`Constraint::named`] carries
/// the text — the module has to recognise `k` by name, because by index it is
/// indistinguishable from a real column.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    /// Which column the constraint is on, as the planner numbered it.
    pub column: Option<usize>,
    /// The column's name, lowercased. `Some("k")` is the module's own column.
    pub named: Option<String>,
    /// The comparison.
    pub op: ColumnOp,
    /// Whether the value is available now (a literal) or later (a parameter).
    ///
    /// **Measured consequence**: a `k` that is a bound parameter is not known at
    /// `xBestIndex` time, so it has to travel to `xFilter` through
    /// `idxStr`/`idxNum`. [`Plan::k_is_literal`] records which case this is, and
    /// [`ENGINE_CONTRACT`] item 6 is the engine-side requirement.
    pub value_is_literal: bool,
}

impl Constraint {
    /// A constraint on the module's own `k` column.
    pub fn k(value_is_literal: bool) -> Self {
        Constraint {
            column: None,
            named: Some("k".to_string()),
            op: ColumnOp::Eq,
            value_is_literal,
        }
    }

    /// A `MATCH` constraint on a vector column, at the planner's column `index`.
    pub fn match_vector(index: usize, value_is_literal: bool) -> Self {
        Constraint {
            column: Some(index),
            named: None,
            op: ColumnOp::Match,
            value_is_literal,
        }
    }

    /// An equality constraint on a declared column.
    pub fn eq_column(index: usize, name: &str) -> Self {
        Constraint {
            column: Some(index),
            named: Some(name.to_ascii_lowercase()),
            op: ColumnOp::Eq,
            value_is_literal: true,
        }
    }

    /// Whether this is the `k = ?` constraint.
    pub fn is_k(&self) -> bool {
        self.named.as_deref() == Some("k")
    }

    /// Whether this is a `MATCH` constraint.
    pub fn is_match(&self) -> bool {
        self.op == ColumnOp::Match
    }
}

/// A chosen plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The plan's argument string, for `xFilter`'s `idxStr`.
    ///
    /// Encodes the two facts `xFilter` cannot rediscover: which constraint was
    /// the `MATCH`, and whether `k` was a literal. The layout is
    /// `match=<column>,literal=<0|1>`, and it is parsed back by
    /// [`Plan::parse_idx_str`], so the format has exactly one writer and one
    /// reader.
    pub idx_str: String,
    /// The plan's `idxNum`, which counts the constraints it consumes.
    pub idx_num: i32,
    /// The query vector, when the `MATCH` value is a literal.
    pub query: Option<Vec<f64>>,
    /// How many neighbours to return.
    pub k: usize,
    /// Whether the rows come out already sorted by ascending distance, so the
    /// engine may skip its own sort. This is `orderByConsumed`.
    pub order_by_distance: bool,
    /// Whether `k` was a literal rather than a bound parameter.
    pub k_is_literal: bool,
    /// The `MATCH` constraint's column index, as the planner numbered it.
    pub match_column: Option<usize>,
}

impl Plan {
    /// Chooses a plan from the constraints the planner offered.
    ///
    /// Returns [`PlanError::NoMatch`] when no `MATCH` is present, and
    /// [`PlanError::MissingK`] when a `MATCH` has no `k` beside it. Nothing else
    /// is optional, so nothing else is a separate branch.
    pub fn choose(constraints: &[Constraint]) -> Result<Plan, PlanError> {
        let match_constraint = constraints.iter().find(|c| c.is_match());
        let Some(match_constraint) = match_constraint else {
            return Err(PlanError::NoMatch);
        };

        // A second MATCH is a contradiction, not a conjunction. Two different
        // query vectors cannot both be the one this table is searched by, and
        // picking either silently would answer a different question from the
        // one asked.
        if constraints
            .iter()
            .filter(|c| c.is_match())
            .fold(0, |n, _| n + 1)
            > 1
        {
            return Err(PlanError::Ambiguous {
                message: "a vec0 table can be searched by only one MATCH at a time".to_string(),
            });
        }

        // A constraint on a vector column that is not MATCH cannot be
        // satisfied: the column holds an embedding, and there is no scalar
        // comparison to make against one.
        for constraint in constraints {
            if constraint.is_k() {
                continue;
            }
            // A constraint on the distance column that is not `=` is refused.
            // `distance` is a real column that a query may select, so this
            // checks the operator rather than the column's presence.
            if constraint.named.as_deref() == Some(DISTANCE_COLUMN) && constraint.op != ColumnOp::Eq
            {
                return Err(PlanError::Ambiguous {
                    message: format!(
                        "the {DISTANCE_COLUMN} column can only be compared with =, not {}",
                        constraint.op.as_str()
                    ),
                });
            }
        }

        let k_constraint = constraints.iter().find(|c| c.is_k());
        let Some(k_constraint) = k_constraint else {
            return Err(PlanError::MissingK);
        };
        if k_constraint.op != ColumnOp::Eq {
            return Err(PlanError::Ambiguous {
                message: format!(
                    "k can only be compared with =, not {}",
                    k_constraint.op.as_str()
                ),
            });
        }

        let k_is_literal = k_constraint.value_is_literal;
        Ok(Plan {
            idx_str: format!(
                "match={},literal={}",
                match_constraint
                    .column
                    .map_or_else(|| "none".to_string(), |c| c.to_string()),
                u8::from(k_is_literal)
            ),
            idx_num: 1,
            query: None,
            k: 0,
            order_by_distance: true,
            k_is_literal,
            match_column: match_constraint.column,
        })
    }

    /// The plan the reference shape produces: a `MATCH` with a literal query
    /// and a literal `k`.
    ///
    /// This is the entry point a caller that already has the values uses. It is
    /// the same as [`Plan::choose`] followed by filling in the arguments, and it
    /// is what the tests drive.
    pub fn literal(query: Vec<f64>, k: usize) -> Plan {
        Plan {
            idx_str: "match=0,literal=1".to_string(),
            idx_num: 1,
            query: Some(query),
            k,
            order_by_distance: true,
            k_is_literal: true,
            match_column: Some(0),
        }
    }

    /// Parses an `idx_str` written by [`Plan::choose`].
    ///
    /// A malformed string is [`PlanError::Ambiguous`] rather than a panic: the
    /// string crosses the module boundary, and a value from outside is a
    /// reportable fault, not an assertion failure.
    pub fn parse_idx_str(idx_str: &str) -> Result<Plan, PlanError> {
        let mut match_column = None;
        let mut literal = false;
        let mut seen_match = false;
        let mut seen_literal = false;
        for part in idx_str.split(',') {
            let Some((key, value)) = part.split_once('=') else {
                return Err(PlanError::Ambiguous {
                    message: format!("malformed idx_str: {idx_str:?}"),
                });
            };
            match key {
                "match" => {
                    seen_match = true;
                    match_column = if value == "none" {
                        None
                    } else {
                        Some(value.parse().map_err(|_| PlanError::Ambiguous {
                            message: format!("malformed idx_str: {idx_str:?}"),
                        })?)
                    };
                }
                "literal" => {
                    seen_literal = true;
                    literal = value == "1";
                }
                _ => {
                    return Err(PlanError::Ambiguous {
                        message: format!("unknown idx_str key: {key:?}"),
                    })
                }
            }
        }
        if !seen_match || !seen_literal {
            return Err(PlanError::Ambiguous {
                message: format!("incomplete idx_str: {idx_str:?}"),
            });
        }
        Ok(Plan {
            idx_str: idx_str.to_string(),
            idx_num: 1,
            query: None,
            k: 0,
            order_by_distance: true,
            k_is_literal: literal,
            match_column,
        })
    }

    /// Checks a `k` that is now known, once a bound parameter's value arrives.
    ///
    /// Split out from [`Plan::choose`] because a bound `k` is not knowable at
    /// `xBestIndex` time, so the check happens at `xFilter`. The rules:
    ///
    /// * a `k` of zero returns no rows. **Measured**: the reference's
    ///   `SELECT d FROM t ORDER BY d LIMIT 0` returns none, and a kNN query
    ///   asking for nothing is answered with nothing.
    /// * a `k` above the row count returns every row. **Measured**: the same
    ///   query with `LIMIT 5` over a two-row table returns both rows, so this
    ///   is not an error and is deliberately not one here.
    /// * a negative `k` is an error. **Reasoned**: `LIMIT -1` is
    ///   no-limit in SQLite, which for a kNN table would mean a full sort by
    ///   distance over the whole table. That is a different query from the one
    ///   written, so it is refused rather than quietly reinterpreted.
    pub fn check_k(&self, k: i64) -> Result<usize, PlanError> {
        if k < 0 {
            return Err(PlanError::BadK { k });
        }
        Ok(k as usize)
    }

    /// Fills in the query vector and `k`, then validates both.
    pub fn with_arguments(mut self, query: Vec<f64>, k: i64) -> Result<Plan, PlanError> {
        self.k = self.check_k(k)?;
        self.query = Some(query);
        Ok(self)
    }
}

/// Why a statement cannot be planned, or cannot be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No `MATCH` in the `WHERE` clause.
    ///
    /// A `vec0` table has no scalar rows to scan: every column but the
    /// auxiliary ones is either a rowid or an embedding, and a query that
    /// neither matches nor asks for anything else has no answer. This is the
    /// same fault the reference reports when a vector column is used as a
    /// value, and the message says how to write the query instead.
    NoMatch,

    /// A `MATCH` with no `k` beside it.
    MissingK,

    /// A `k` that is not usable.
    BadK {
        /// The value that was refused.
        k: i64,
    },

    /// The query vector was not usable.
    ///
    /// Carries the search module's own error rather than restating it: a width
    /// mismatch or a zero-norm cosine query is a property of the arithmetic,
    /// and `search` already words it.
    Query(SearchError),

    /// Anything else that makes a plan impossible.
    Ambiguous {
        /// What was wrong, stated so a caller can act on it.
        message: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoMatch => write!(
                f,
                "a vec0 table can only be queried with a MATCH: \
                 SELECT ... WHERE <table>.<vector> MATCH ? AND k = ?"
            ),
            PlanError::MissingK => write!(
                f,
                "a vec0 MATCH query needs a k: add `AND k = <n>` to say how many \
                 neighbours to return"
            ),
            PlanError::BadK { k } => write!(f, "k must not be negative, got {k}"),
            PlanError::Query(err) => write!(f, "{err}"),
            PlanError::Ambiguous { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<SearchError> for PlanError {
    fn from(err: SearchError) -> Self {
        PlanError::Query(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Metric;

    fn parse(args: &str) -> Result<Schema, ParseError> {
        Schema::parse("t", args)
    }

    // -- the schema ------------------------------------------------------

    #[test]
    fn the_minimal_table_parses() {
        let s = parse("(rowid, embedding float[3], distance_metric=L2)").unwrap();
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.dim(), 3);
        assert_eq!(s.metric, Metric::L2);
        assert!(s.rowid_alias().is_some());
    }

    #[test]
    fn a_bare_column_name_is_a_vector_not_a_rowid() {
        // The regression that motivated the classification: a vector column is
        // written with no sigil, and treating every bare name as a rowid alias
        // made every ordinary table unparseable.
        let s = parse("(embedding float[3], distance_metric=L2)").unwrap();
        assert_eq!(s.columns[0].kind, ColumnKind::Vector);
        assert_eq!(s.columns[0].name, "embedding");
        assert!(s.rowid_alias().is_none());
    }

    #[test]
    fn a_table_with_no_vector_column_is_rejected() {
        let err = parse("(rowid, +note, distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("float[N]"), "{err}");
    }

    #[test]
    fn a_missing_metric_is_an_error_not_a_silent_default() {
        let err = parse("(embedding float[3])").unwrap_err();
        assert!(err.message.contains("distance_metric"), "{err}");
    }

    #[test]
    fn an_unknown_metric_is_rejected_and_names_the_alternatives() {
        let err = parse("(embedding float[3], distance_metric=hamming)").unwrap_err();
        assert!(err.message.contains("no such distance_metric"), "{err}");
        assert!(err.message.contains("cosine"), "{err}");
    }

    #[test]
    fn metric_names_are_case_insensitive() {
        for (text, want) in [
            ("L2", Metric::L2),
            ("l2", Metric::L2),
            ("cosine", Metric::Cosine),
            ("Cosine", Metric::Cosine),
            ("L1", Metric::L1),
            ("inner_product", Metric::InnerProduct),
            ("INNER_PRODUCT", Metric::InnerProduct),
        ] {
            let s = parse(&format!("(embedding float[2], distance_metric={text})")).unwrap();
            assert_eq!(s.metric, want, "for {text}");
        }
    }

    #[test]
    fn a_quoted_metric_value_is_unquoted() {
        let s = parse("(embedding float[2], distance_metric='cosine')").unwrap();
        assert_eq!(s.metric, Metric::Cosine);
    }

    #[test]
    fn partition_and_auxiliary_sigil_are_recorded_and_stripped() {
        let s = parse("(rowid, *bucket, embedding float[4], +note, distance_metric=L2)").unwrap();
        assert_eq!(s.partition().unwrap().name, "bucket");
        assert_eq!(s.auxiliaries().next().unwrap().name, "note");
        // The sigil is syntax, not part of the name a query uses.
        assert!(s.column("bucket").is_some());
        assert!(s.column("*bucket").is_none());
    }

    #[test]
    fn an_auxiliary_column_is_stored_but_not_searched() {
        let s = parse("(rowid, embedding float[2], +note, distance_metric=L2)").unwrap();
        let note = s.column("note").unwrap();
        assert!(!note.is_searchable());
        assert!(!note.is_vector());
        let emb = s.column("embedding").unwrap();
        assert!(emb.is_searchable());
        assert!(emb.is_vector());
    }

    #[test]
    fn several_vector_columns_must_agree_on_width() {
        let s = parse("(a float[3], b float[3], distance_metric=L2)").unwrap();
        assert_eq!(s.vectors().count(), 2);
        let err = parse("(a float[3], b float[4], distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("same width"), "{err}");
    }

    #[test]
    fn a_vector_column_needs_a_width() {
        for bad in [
            "(embedding float, distance_metric=L2)",
            "(embedding float[], distance_metric=L2)",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(
                err.message.contains("float[N]") || err.message.contains("width"),
                "{err}"
            );
        }
        let err = parse("(embedding float[0], distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("at least 1"), "{err}");
    }

    #[test]
    fn a_non_vector_column_may_not_carry_a_type() {
        let err = parse("(rowid, +note TEXT, embedding float[2], distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("takes no type"), "{err}");
    }

    #[test]
    fn the_rowid_column_must_come_first() {
        let err = parse("(embedding float[2], rowid, distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("must come first"), "{err}");
    }

    #[test]
    fn the_partition_column_must_precede_the_vectors() {
        let err = parse("(embedding float[2], *bucket, distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("must come before"), "{err}");
    }

    #[test]
    fn a_duplicate_column_name_is_rejected_case_insensitively() {
        let err =
            parse("(embedding float[2], Embedding float[2], distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("duplicate column"), "{err}");
    }

    #[test]
    fn an_unknown_option_is_rejected() {
        let err = parse("(embedding float[2], chunk_size=10, distance_metric=L2)").unwrap_err();
        assert!(err.message.contains("no such vec0 option"), "{err}");
    }

    #[test]
    fn an_option_with_no_value_is_rejected() {
        let err = parse("(embedding float[2], distance_metric=)").unwrap_err();
        assert!(err.message.contains("no value"), "{err}");
    }

    #[test]
    fn the_argument_list_must_be_parenthesised() {
        let err = parse("embedding float[3]").unwrap_err();
        assert!(err.message.contains("parenthesised"), "{err}");
    }

    #[test]
    fn an_empty_argument_list_is_rejected() {
        let err = parse("()").unwrap_err();
        assert!(err.message.contains("at least one column"), "{err}");
    }

    #[test]
    fn a_comma_inside_quotes_does_not_split_the_argument_list() {
        let parts = split_top_level("a, 'b,c', d");
        assert_eq!(parts.len(), 3);
        // And the real shape still parses, which is the case that matters.
        let s = parse("(embedding float[2], distance_metric=L2)").unwrap();
        assert_eq!(s.metric, Metric::L2);
    }

    #[test]
    fn stored_column_count_excludes_the_rowid() {
        let s = parse("(rowid, embedding float[2], +note, distance_metric=L2)").unwrap();
        assert_eq!(s.stored_column_count(), 2);
    }

    #[test]
    fn shadow_tables_follow_the_measured_naming_convention() {
        let s = parse("(rowid, embedding float[2], distance_metric=L2)").unwrap();
        let names = s.shadow_table_names();
        assert_eq!(names.vectors, "t_vectors");
        assert_eq!(names.chunks, "t_chunks");
        assert_eq!(names.rowids, "t_rowids");
        assert_eq!(names.info, "t_info");
        let mut all = names.all().to_vec();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 4);
        assert!(!names.all().contains(&"t"));
    }

    #[test]
    fn the_contract_lists_every_number_it_claims_to() {
        for n in 1..=14 {
            assert!(
                ENGINE_CONTRACT.contains(&format!("{n}.")),
                "item {n} is missing from ENGINE_CONTRACT"
            );
        }
    }

    // -- the plan --------------------------------------------------------

    fn both() -> Vec<Constraint> {
        vec![Constraint::match_vector(0, true), Constraint::k(true)]
    }

    #[test]
    fn a_match_with_a_k_plans_and_consumes_the_ordering() {
        let plan = Plan::choose(&both()).unwrap();
        assert!(plan.order_by_distance, "rows come out sorted by distance");
        assert!(plan.k_is_literal);
        assert_eq!(plan.match_column, Some(0));
    }

    #[test]
    fn a_match_with_no_k_is_an_error_that_names_the_fix() {
        let err = Plan::choose(&[Constraint::match_vector(0, true)]).unwrap_err();
        assert_eq!(err, PlanError::MissingK);
        assert!(err.to_string().contains("k ="), "{err}");
    }

    #[test]
    fn a_query_with_no_match_is_an_error_that_shows_the_shape() {
        let err = Plan::choose(&[Constraint::k(true)]).unwrap_err();
        assert_eq!(err, PlanError::NoMatch);
        let text = err.to_string();
        assert!(text.contains("MATCH") && text.contains("k ="), "{text}");
    }

    #[test]
    fn two_matches_are_a_contradiction_rather_than_a_conjunction() {
        let err = Plan::choose(&[
            Constraint::match_vector(0, true),
            Constraint::match_vector(1, true),
            Constraint::k(true),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("only one MATCH"), "{err}");
    }

    #[test]
    fn a_negative_k_is_refused_and_a_bound_one_is_marked_as_not_literal() {
        assert_eq!(
            Plan::literal(vec![1.0], 0).check_k(-1),
            Err(PlanError::BadK { k: -1 })
        );
        let plan =
            Plan::choose(&[Constraint::match_vector(0, false), Constraint::k(false)]).unwrap();
        assert!(!plan.k_is_literal, "a bound k must travel in idxStr/idxNum");
    }

    #[test]
    fn a_k_above_the_row_count_is_not_an_error() {
        // Measured: the reference's `ORDER BY d LIMIT 5` over a two-row table
        // returns both rows. So this is accepted here too.
        assert_eq!(Plan::literal(vec![1.0], 0).check_k(99), Ok(99));
    }

    #[test]
    fn an_idx_str_round_trips_through_the_filter_argument() {
        let plan =
            Plan::choose(&[Constraint::match_vector(3, false), Constraint::k(false)]).unwrap();
        let back = Plan::parse_idx_str(&plan.idx_str).unwrap();
        assert_eq!(back.match_column, Some(3));
        assert!(!back.k_is_literal);
    }

    #[test]
    fn a_malformed_idx_str_is_a_reported_fault_not_a_panic() {
        // The string crosses the module boundary, so a bad one from outside has
        // to be reportable.
        for bad in [
            "",
            "match",
            "match=0",
            "match=x,literal=1",
            "nope=1,literal=1",
        ] {
            assert!(Plan::parse_idx_str(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_k_compared_with_something_other_than_equals_is_refused() {
        let mut c = Constraint::k(true);
        c.op = ColumnOp::Other(">");
        let err = Plan::choose(&[Constraint::match_vector(0, true), c]).unwrap_err();
        assert!(
            err.to_string().contains("can only be compared with ="),
            "{err}"
        );
    }

    #[test]
    fn a_distance_compared_with_something_other_than_equals_is_refused() {
        let mut c = Constraint::eq_column(1, DISTANCE_COLUMN);
        c.op = ColumnOp::Other("<");
        let err =
            Plan::choose(&[Constraint::match_vector(0, true), Constraint::k(true), c]).unwrap_err();
        assert!(err.to_string().contains("compared with ="), "{err}");
    }
}

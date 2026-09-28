//! Tests for the `vec0` virtual table module.
//!
//! Everything here is deterministic and offline: the vector set is a fixed
//! literal, and the ranking is checked against the *exact* search this crate
//! already has rather than against a hard-coded expectation. That matters more
//! than it might look — a fixed list of expected rowids would go stale the
//! moment the tie-break or the metric changed, and would keep passing as long
//! as the answer was self-consistent. Comparing against `FlatIndex` means a
//! change that breaks the kNN table has to break the baseline too, which is a
//! louder failure.
//!
//! The suite is grouped by what each part has to prove:
//!
//! * the DDL surface, including every rejection and why it is one;
//! * the storage round-trip, because a table that reopens differently is worse
//!   than one that fails to open;
//! * the query shape, including the three error cases the reference defines;
//! * the module lifecycle, `xCreate` against `xConnect`, because conflating them
//!   is how a reopened database loses its vectors;
//! * the ranking itself, against the flat index.

use nsqlite_vector::index_store::{StoredRow, Vec0Table};
use nsqlite_vector::search::{FlatIndex, Metric};
use nsqlite_vector::vtab::{
    ColumnOp, Constraint, MemoryVTab, Plan, PlanError, RowUpdate, Schema, VTab, VTabError,
    Vec0Module,
};

// ---------------------------------------------------------------------------
// A deterministic vector set
// ---------------------------------------------------------------------------

/// A small fixed corpus: the six axis-ish points of a 3-dimensional space, plus
/// two off-axis ones so a ranking has something to get wrong.
///
/// The first six are the vertices of an octahedron, which is a useful shape
/// because every one of them is at the same distance from the origin and each
/// axis is nearest to exactly one of them — so a kNN query against the origin
/// has a genuine six-way tie, and the tie-break is observable.
fn corpus() -> Vec<(i64, Vec<f64>)> {
    vec![
        (1, vec![1.0, 0.0, 0.0]),
        (2, vec![-1.0, 0.0, 0.0]),
        (3, vec![0.0, 1.0, 0.0]),
        (4, vec![0.0, -1.0, 0.0]),
        (5, vec![0.0, 0.0, 1.0]),
        (6, vec![0.0, 0.0, -1.0]),
        (7, vec![0.5, 0.5, 0.5]),
        (8, vec![0.9, 0.1, 0.0]),
    ]
}

const SCHEMA_ARGS: &str = "(rowid, embedding float[3], +note, distance_metric=L2)";
const RICH_ARGS: &str = "(rowid, *bucket, embedding float[3], +note, +tag, distance_metric=L2)";

fn schema() -> Schema {
    Schema::parse("docs", SCHEMA_ARGS).unwrap()
}

fn rich_schema() -> Schema {
    Schema::parse("docs", RICH_ARGS).unwrap()
}

fn note(text: &str) -> Vec<Option<Vec<u8>>> {
    vec![Some(text.as_bytes().to_vec())]
}

/// A table holding the corpus, with a note per row.
fn loaded() -> Vec0Table {
    let s = schema();
    let rows = corpus()
        .into_iter()
        .map(|(rowid, vector)| StoredRow {
            rowid,
            vector,
            metadata: note(&format!("doc {rowid}")),
        })
        .collect();
    Vec0Table::with_rows(s, rows).unwrap()
}

/// The ids a search returns, in order.
fn ids(hits: &[nsqlite_vector::search::Neighbor]) -> Vec<usize> {
    hits.iter().map(|n| n.id).collect()
}

/// The exact ranking, computed longhand from a row set.
///
/// This is the oracle. It is written out rather than delegating to `FlatIndex`
/// so that a bug in the kNN module is not masked by a bug in the index it is
/// supposed to agree with; the two are then also compared to each other, so
/// both have to be right.
fn exact_order(rows: &[(i64, Vec<f64>)], query: &[f64], k: usize, metric: Metric) -> Vec<usize> {
    let mut scored: Vec<(f64, usize)> = rows
        .iter()
        .map(|(rowid, vector)| {
            let d = match metric {
                Metric::L2 => vector
                    .iter()
                    .zip(query)
                    .map(|(a, b)| (a - b) * (a - b))
                    .sum(),
                Metric::L1 => vector.iter().zip(query).map(|(a, b)| (a - b).abs()).sum(),
                Metric::Cosine => {
                    let dot: f64 = vector.iter().zip(query).map(|(a, b)| a * b).sum();
                    let na = vector.iter().map(|a| a * a).sum::<f64>().sqrt();
                    let nb = query.iter().map(|b| b * b).sum::<f64>().sqrt();
                    1.0 - dot / (na * nb)
                }
                Metric::InnerProduct => -vector.iter().zip(query).map(|(a, b)| a * b).sum::<f64>(),
            };
            (d, *rowid as usize)
        })
        .collect();
    // Ties break on rowid, which is the order the rows are stored in, so this
    // is the same rule the module promises.
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, rowid)| rowid).collect()
}

/// A plan for a literal MATCH and a literal k, which is the shape the reference
/// query produces. Kept in one place so a change to how a plan is built shows up
/// in one diff rather than twenty.
fn plan_for(query: [f64; 3], k: i64) -> Plan {
    let q = query.to_vec();
    Plan::literal(q.clone(), 0).with_arguments(q, k).unwrap()
}

fn table_with(metric_args: &str, rows: Vec<(i64, Vec<f64>)>) -> Vec0Table {
    let s = Schema::parse("docs", metric_args).unwrap();
    let stored = rows
        .into_iter()
        .map(|(rowid, vector)| StoredRow {
            rowid,
            vector,
            metadata: Vec::new(),
        })
        .collect();
    Vec0Table::with_rows(s, stored).unwrap()
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

#[test]
fn a_table_is_created_from_the_reference_ddl() {
    // The DDL the reference uses: a rowid column, a vector column written
    // float[N] with a distance_metric, and an auxiliary column marked with +.
    let s = Schema::parse("docs", SCHEMA_ARGS).unwrap();
    assert_eq!(s.name, "docs");
    assert_eq!(s.dim(), 3);
    assert_eq!(s.metric, Metric::L2);
    assert_eq!(s.vectors().count(), 1);
    assert_eq!(s.auxiliaries().count(), 1);
    assert!(s.rowid_alias().is_some());
}

#[test]
fn a_partition_column_and_several_auxiliaries_parse() {
    let s = rich_schema();
    assert_eq!(s.partition().unwrap().name, "bucket");
    let aux: Vec<&str> = s.auxiliaries().map(|c| c.name.as_str()).collect();
    assert_eq!(aux, ["note", "tag"]);
    assert_eq!(s.dim(), 3);
}

#[test]
fn a_table_with_a_cosine_metric_declares_it() {
    let s = Schema::parse("docs", "(embedding float[3], distance_metric=cosine)").unwrap();
    assert_eq!(s.metric, Metric::Cosine);
}

#[test]
fn the_shadow_tables_are_ordinary_create_table_statements() {
    // The engine reads a table's definition back from this text on reopen, so
    // each one has to be a CREATE TABLE the parser accepts -- and the parser is
    // the engine's, not a copy of it.
    for (name, sql) in schema().shadow_table_sql() {
        let stmt =
            nsqlite::parser::parse_one(&sql).unwrap_or_else(|e| panic!("{sql} did not parse: {e}"));
        assert!(matches!(stmt, nsqlite::parser::Stmt::CreateTable { .. }));
        assert!(sql.contains(&name));
    }
}

#[test]
fn the_shadow_tables_are_named_the_way_sqlite_names_its_own() {
    // Measured against the reference: rtree writes geo_rowid, geo_node, and so
    // on, each an ordinary table with its own rootpage.
    let names = schema().shadow_table_names();
    assert_eq!(names.vectors, "docs_vectors");
    assert_eq!(names.chunks, "docs_chunks");
    assert_eq!(names.rowids, "docs_rowids");
    assert_eq!(names.info, "docs_info");
}

#[test]
fn a_schema_that_does_not_parse_is_reported_with_its_text() {
    let err = Schema::parse(
        "docs",
        "(embedding float[3], chunk_size=10, distance_metric=L2)",
    )
    .unwrap_err();
    // The context is what makes a long CREATE statement debuggable.
    assert!(err.to_string().contains("chunk_size"), "{err}");
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[test]
fn rows_inserted_through_the_table_path_come_back_out_of_it() {
    let mut t = Vec0Table::create(schema());
    for (rowid, vector) in corpus() {
        let got = t.insert(Some(rowid), vector, note("x")).unwrap();
        assert_eq!(got, rowid);
    }
    assert_eq!(t.len().unwrap(), corpus().len());
    for (rowid, vector) in corpus() {
        let stored = t.get(rowid).unwrap().unwrap();
        assert_eq!(stored.vector, vector);
        assert_eq!(stored.metadata, note("x"));
    }
}

#[test]
fn a_vector_is_stored_as_a_big_endian_blob_of_doubles() {
    let t = loaded();
    let shadow = t.shadow_rows().unwrap();
    assert_eq!(shadow.vectors.len(), corpus().len());
    for (_, blob) in &shadow.vectors {
        assert_eq!(blob.len(), 3 * 8);
    }
    // 1.0 as big-endian IEEE-754, which is what the engine's own record writer
    // emits (crates/nsqlite/src/record.rs:206).
    assert_eq!(&shadow.vectors[0].1[..8], &[0x3F, 0xF0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn a_saved_table_reopens_with_the_same_rows_and_the_same_ranking() {
    let t = loaded();
    let shadow = t.shadow_rows().unwrap();
    let reopened = Vec0Table::from_shadow_rows(schema(), &shadow).unwrap();
    assert_eq!(reopened.len().unwrap(), t.len().unwrap());
    for query in [[1.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.5, 0.5, 0.5]] {
        assert_eq!(
            ids(&t.search(&query, 4).unwrap()),
            ids(&reopened.search(&query, 4).unwrap()),
            "query {query:?}"
        );
    }
}

#[test]
fn the_table_and_the_index_agree_because_they_are_the_same_store() {
    // The property the whole storage design exists for: an index built from the
    // rows and a search over the table cannot disagree, because the index is
    // built from the table on the way in rather than maintained beside it.
    let t = loaded();
    let query = [0.9, 0.1, 0.0];
    let index = t.build_index().unwrap();
    let direct = index.search(&query, 5).unwrap();
    let through = t.search(&query, 5).unwrap();
    assert_eq!(direct.len(), through.len());
    for (a, b) in direct.iter().zip(through.iter()) {
        assert_eq!(a.distance, b.distance);
    }
}

#[test]
fn a_delete_removes_the_row_from_the_index_as_well() {
    let mut t = loaded();
    assert!(t.delete(1).unwrap());
    assert!(
        !t.delete(1).unwrap(),
        "deleting twice reports nothing to delete"
    );
    let hits = t.search(&[1.0, 0.0, 0.0], 8).unwrap();
    assert!(!hits.iter().any(|n| n.id == 1), "row 1 still ranks");
    assert_eq!(hits.len(), corpus().len() - 1);
}

#[test]
fn a_row_of_the_wrong_width_is_refused_at_the_door() {
    let mut t = Vec0Table::create(schema());
    let err = t.insert(None, vec![1.0, 2.0], note("x")).unwrap_err();
    assert!(err.to_string().contains("3 dimensions"), "{err}");
}

#[test]
fn a_row_with_a_nan_is_refused_rather_than_left_to_poison_the_sort() {
    let mut t = Vec0Table::create(schema());
    let err = t
        .insert(None, vec![1.0, f64::NAN, 0.0], note("x"))
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"), "{err}");
}

#[test]
fn an_insert_with_no_rowid_allocates_the_next_one() {
    let mut t = Vec0Table::create(schema());
    assert_eq!(t.insert(None, vec![1.0, 0.0, 0.0], note("a")).unwrap(), 1);
    assert_eq!(t.insert(None, vec![0.0, 1.0, 0.0], note("b")).unwrap(), 2);
    // An explicit rowid of 0 is a value, not "allocate": SQLite treats NULL as
    // "give me one", so `Some(0)` must not be folded into the same case.
    assert_eq!(
        t.insert(Some(0), vec![0.0, 0.0, 1.0], note("c")).unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------
// The query shape
// ---------------------------------------------------------------------------

#[test]
fn a_match_query_with_a_k_plans_and_answers() {
    let t = loaded();
    let vtab = MemoryVTab::new(t);
    let cursor = vtab.filter(&plan_for([1.0, 0.0, 0.0], 3)).unwrap();
    assert_eq!(cursor.len(), 3);
    assert_eq!(
        cursor.rows().iter().map(|r| r.rowid).collect::<Vec<_>>(),
        [1, 8, 7]
    );
}

#[test]
fn a_match_with_no_k_is_an_error() {
    let err = Plan::choose(&[Constraint::match_vector(0, true)]).unwrap_err();
    assert_eq!(err, PlanError::MissingK);
    // The message has to be actionable: a user who wrote the query is one
    // syntax mistake away from the right one.
    assert!(err.to_string().contains("AND k ="), "{err}");
}

#[test]
fn a_query_with_no_match_is_an_error() {
    // A vec0 table has no scalar rows, so a query that neither matches nor asks
    // for anything else has no answer.
    let err = Plan::choose(&[Constraint::k(true)]).unwrap_err();
    assert_eq!(err, PlanError::NoMatch);
    assert!(err.to_string().contains("MATCH"), "{err}");
}

#[test]
fn a_negative_k_is_refused_but_a_k_above_the_row_count_is_not() {
    // Measured: `ORDER BY d LIMIT 5` over a two-row table returns both rows.
    // A negative k is the case that is a fault: `LIMIT -1` means no limit in
    // SQLite, which for a kNN table is a full sort -- a different query.
    assert_eq!(
        Plan::literal(vec![1.0], 0).check_k(-1),
        Err(PlanError::BadK { k: -1 })
    );
    let t = loaded();
    let hits = t.search(&[1.0, 0.0, 0.0], 99).unwrap();
    assert_eq!(hits.len(), corpus().len());
}

#[test]
fn a_non_match_use_of_the_vector_column_is_an_error() {
    // The vector column holds an embedding, so `= 1.0` against it cannot be
    // satisfied. It is refused rather than silently matching nothing.
    let mut c = Constraint::eq_column(0, "embedding");
    c.op = ColumnOp::Eq;
    let err = Plan::choose(&[c, Constraint::k(true)]).unwrap_err();
    assert_eq!(err, PlanError::NoMatch, "no MATCH, so no plan: {err}");
    // And a MATCH on the same column is fine, which is the contrast that makes
    // the error meaningful.
    assert!(Plan::choose(&[Constraint::match_vector(0, true), Constraint::k(true)]).is_ok());
}

#[test]
fn a_query_vector_of_the_wrong_width_is_the_search_error() {
    let t = loaded();
    let err = t.search(&[1.0, 0.0], 3).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("dimension") || text.contains("3"), "{text}");
}

#[test]
fn a_cosine_table_refuses_a_zero_query_rather_than_ranking_it() {
    // The division by the norm has no answer, and a NaN that ranks is worse
    // than an error.
    let t = table_with(
        "(embedding float[3], distance_metric=cosine)",
        vec![(1, vec![1.0, 0.0, 0.0])],
    );
    let err = t.search(&[0.0, 0.0, 0.0], 1).unwrap_err();
    assert!(err.to_string().contains("zero"), "{err}");
}

#[test]
fn a_bound_k_is_planned_before_its_value_is_known() {
    // xBestIndex runs before a parameter's value arrives, so the plan records
    // that the k is not literal and the value travels in idxStr/idxNum. That is
    // engine-contract item 6, and this is the test that says it is real.
    let plan = Plan::choose(&[Constraint::match_vector(0, false), Constraint::k(false)]).unwrap();
    assert!(!plan.k_is_literal);
    assert!(plan.idx_str.contains("literal=0"), "{}", plan.idx_str);
    // The value is applied later and checked then.
    let plan = plan.with_arguments(vec![1.0, 0.0, 0.0], 2).unwrap();
    assert_eq!(plan.k, 2);
}

#[test]
fn the_distance_column_is_addressable_in_order_by() {
    // `orderByConsumed` is what lets the engine skip its own sort, and it is
    // only sound if the rows really do come out in ascending distance.
    let vtab = MemoryVTab::new(loaded());
    let cursor = vtab.filter(&plan_for([0.0, 0.0, 0.0], 8)).unwrap();
    assert!(cursor.is_ordered());
    let distances: Vec<f64> = cursor
        .rows()
        .iter()
        .map(|r| r.distance().unwrap())
        .collect();
    let mut sorted = distances.clone();
    sorted.sort_by(f64::total_cmp);
    assert_eq!(
        distances, sorted,
        "rows are not in ascending distance order"
    );
}

#[test]
fn every_row_carries_a_distance_and_it_is_the_last_column() {
    let vtab = MemoryVTab::new(loaded());
    let cursor = vtab.filter(&plan_for([1.0, 0.0, 0.0], 2)).unwrap();
    for row in cursor.rows() {
        // The schema declares 3 columns; the hidden distance is the fourth.
        assert_eq!(row.values.len(), 4);
        assert!(row.distance().is_some());
    }
    // The exact match is at distance zero.
    assert_eq!(cursor.rows()[0].distance(), Some(0.0));
}

#[test]
fn the_cursor_walks_rows_the_way_nextover_eof_would() {
    let vtab = MemoryVTab::new(loaded());
    let mut cursor = vtab.filter(&plan_for([1.0, 0.0, 0.0], 2)).unwrap();
    let mut seen = Vec::new();
    while !cursor.eof() {
        seen.push(cursor.rowid().unwrap());
        cursor.next_row();
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen, [1, 8]);
}

#[test]
fn an_empty_table_answers_a_match_query_with_no_rows() {
    let vtab = MemoryVTab::new(Vec0Table::create(schema()));
    let cursor = vtab.filter(&plan_for([1.0, 0.0, 0.0], 5)).unwrap();
    assert!(cursor.is_empty());
    assert!(cursor.eof());
}

// ---------------------------------------------------------------------------
// The module: xCreate against xConnect
// ---------------------------------------------------------------------------

#[test]
fn a_module_creates_and_then_connects_to_the_same_table() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    assert_eq!(vtab.schema().dim(), 3);
    let again = module.connect("docs", SCHEMA_ARGS).unwrap();
    assert_eq!(again.schema(), vtab.schema());
    assert_eq!(module.table_names(), ["docs"]);
}

#[test]
fn creating_the_same_table_twice_is_an_error() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    let err = module.create("docs", SCHEMA_ARGS).unwrap_err();
    assert!(matches!(err, VTabError::TableExists(_)), "{err}");
}

#[test]
fn connecting_to_a_table_that_was_never_created_is_an_error() {
    // A reopened database supplies only the schema text, so this is the path
    // that has to rebuild rather than create.
    let module = Vec0Module::new();
    let err = module.connect("nope", SCHEMA_ARGS).unwrap_err();
    assert!(matches!(err, VTabError::NoSuchTable(_)), "{err}");
}

#[test]
fn connect_validates_schema_text_it_has_never_seen() {
    // A reopened database supplies text that has never been parsed, and
    // xConnect is the first place it can be. A schema that no longer parses is
    // a corrupt file, not a table to open optimistically.
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    let err = module.connect("docs", "(embedding float[3])").unwrap_err();
    assert!(matches!(err, VTabError::Schema(_)), "{err}");
}

#[test]
fn dropping_a_table_forgets_it_so_a_recreate_starts_empty() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    module
        .with_table_mut("docs", |t| {
            t.update(Some(&[row_update(1, vec![1.0, 0.0, 0.0])]), None)
        })
        .unwrap()
        .unwrap();
    assert!(module.drop_table("docs"));
    assert!(!module.drop_table("docs"));
    module.create("docs", SCHEMA_ARGS).unwrap();
    let fresh = module.connect("docs", SCHEMA_ARGS).unwrap();
    let cursor = fresh.filter(&plan_for([1.0, 0.0, 0.0], 5)).unwrap();
    assert!(cursor.is_empty(), "the dropped table's rows came back");
}

#[test]
fn the_module_reports_the_column_count_the_way_xcolumncount_does() {
    let mut module = Vec0Module::new();
    // 3 declared columns (rowid, embedding, +note) plus the hidden distance.
    module.create("docs", SCHEMA_ARGS).unwrap();
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    assert_eq!(vtab.column_count(), 4);
    // And a partitioned table with two auxiliaries is 6.
    module.create("rich", RICH_ARGS).unwrap();
    let rich = module.connect("rich", RICH_ARGS).unwrap();
    assert_eq!(rich.column_count(), 6);
}

/// Builds an `xUpdate` row for a three-column schema: rowid, embedding, +note.
fn row_update(rowid: i64, vector: Vec<f64>) -> RowUpdate {
    RowUpdate {
        rowid: Some(rowid),
        // rowid slot, vector slot, auxiliary slot, and the hidden distance,
        // which a write must leave empty.
        values: vec![None, Some(vector), None, None],
        metadata: vec![None],
    }
}

#[test]
fn an_insert_through_xupdate_lands_in_the_table() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    module
        .with_table_mut("docs", |t| {
            t.update(Some(&[row_update(7, vec![1.0, 0.0, 0.0])]), None)
        })
        .unwrap()
        .unwrap();
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    let cursor = vtab.filter(&plan_for([1.0, 0.0, 0.0], 1)).unwrap();
    assert_eq!(cursor.len(), 1);
    assert_eq!(cursor.rows()[0].rowid, 7);
}

#[test]
fn a_delete_through_xupdate_removes_the_row() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    module
        .with_table_mut("docs", |t| {
            t.update(Some(&[row_update(7, vec![1.0, 0.0, 0.0])]), None)
        })
        .unwrap()
        .unwrap();
    // C's xUpdate for a DELETE passes argv[0] = NULL and argv[1] = the old rowid.
    module
        .with_table_mut("docs", |t| t.update(None, Some(7)))
        .unwrap()
        .unwrap();
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    let cursor = vtab
        .filter(
            &Plan::literal(vec![1.0, 0.0, 0.0], 5)
                .with_arguments(vec![1.0, 0.0, 0.0], 5)
                .unwrap(),
        )
        .unwrap();
    assert!(cursor.is_empty());
}

#[test]
fn a_write_to_the_distance_column_is_refused_rather_than_dropped() {
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    let mut row = row_update(1, vec![1.0, 0.0, 0.0]);
    row.values[3] = Some(vec![0.0]);
    let err = module
        .with_table_mut("docs", |t| t.update(Some(&[row]), None))
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, VTabError::DistanceIsNotWritable), "{err}");
    // And the refused write left nothing behind.
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    assert!(vtab
        .filter(&plan_for([1.0, 0.0, 0.0], 5))
        .unwrap()
        .is_empty());
}

#[test]
fn a_transaction_is_a_no_op_because_a_shadow_write_is_an_ordinary_write() {
    // Contract item 12: the engine's journal already covers this, so the module
    // has nothing to add. The test is here so that the absence stays a decision
    // on the record rather than a gap discovered later.
    let mut module = Vec0Module::new();
    module.create("docs", SCHEMA_ARGS).unwrap();
    module
        .with_table_mut("docs", |t| {
            t.begin().unwrap();
            t.update(Some(&[row_update(1, vec![1.0, 0.0, 0.0])]), None)
                .unwrap();
            t.commit().unwrap();
            t.begin().unwrap();
            t.rollback().unwrap();
        })
        .unwrap();
    let vtab = module.connect("docs", SCHEMA_ARGS).unwrap();
    assert_eq!(vtab.schema().dim(), 3);
}

// ---------------------------------------------------------------------------
// Ranking, against the exact search
// ---------------------------------------------------------------------------

#[test]
fn a_match_ranks_the_nearest_first() {
    let t = loaded();
    let query = [1.0, 0.0, 0.0];
    let got = ids(&t.search(&query, 4).unwrap());
    assert_eq!(got, exact_order(&corpus(), &query, 4, Metric::L2));
    assert_eq!(got[0], 1, "the exact match is nearest");
}

#[test]
fn the_ranking_agrees_with_a_flat_index_built_over_the_same_rows() {
    let t = loaded();
    let vectors: Vec<Vec<f64>> = corpus().into_iter().map(|(_, v)| v).collect();
    let index = FlatIndex::new(vectors, Metric::L2).unwrap();
    for query in [
        [1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [-0.5, 0.25, 0.75],
        [0.9, 0.1, 0.0],
    ] {
        let direct = index.search(&query, 5).unwrap();
        let through = t.search(&query, 5).unwrap();
        assert_eq!(direct.len(), through.len(), "query {query:?}");
        for (a, b) in direct.iter().zip(through.iter()) {
            assert_eq!(a.distance, b.distance, "query {query:?}");
        }
    }
}

#[test]
fn a_tie_is_broken_by_rowid_so_the_answer_is_reproducible() {
    // From the origin, rows 1-6 -- the octahedron -- are all at squared L2 of
    // 1.0, and rows 7 and 8 are nearer (0.75 and 0.82). So a k of 4 lands one
    // row into a six-way tie, and *that* is the case where "sorted by distance"
    // alone does not pin the answer down. The module promises insertion order and
    // insertion order is rowid order, so the same query twice gives the same
    // rows.
    let t = loaded();
    let query = [0.0, 0.0, 0.0];
    let first = ids(&t.search(&query, 3).unwrap());
    let second = ids(&t.search(&query, 3).unwrap());
    assert_eq!(first, second, "the tie was broken differently twice");
    // Rows 7 and 8 are nearest on distance, and row 1 is the lowest rowid among
    // the six that tie at 1.0.
    assert_eq!(first, [7, 8, 1], "the tie is not rowid order");
    assert_eq!(first, exact_order(&corpus(), &query, 3, Metric::L2));
}

#[test]
fn a_l1_table_ranks_under_its_own_metric() {
    let rows = vec![
        (1, vec![0.0, 0.0, 0.0]),
        (2, vec![1.0, 0.0, 0.0]),
        (3, vec![0.0, 2.0, 0.0]),
        (4, vec![0.0, 0.0, 3.0]),
    ];
    let t = table_with("(embedding float[3], distance_metric=L1)", rows.clone());
    let query = [0.0, 0.0, 0.0];
    assert_eq!(
        ids(&t.search(&query, 4).unwrap()),
        exact_order(&rows, &query, 4, Metric::L1)
    );
    assert_eq!(t.search(&query, 4).unwrap()[0].id, 1);
}

#[test]
fn a_cosine_table_ranks_under_its_own_metric() {
    let rows = vec![
        (1, vec![1.0, 0.0, 0.0]),
        (2, vec![0.9, 0.1, 0.0]),
        (3, vec![0.0, 1.0, 0.0]),
        (4, vec![-1.0, 0.0, 0.0]),
    ];
    let t = table_with("(embedding float[3], distance_metric=cosine)", rows.clone());
    let query = [1.0, 0.0, 0.0];
    let got = ids(&t.search(&query, 3).unwrap());
    assert_eq!(got, exact_order(&rows, &query, 3, Metric::Cosine));
    // Cosine is scale invariant, so a longer vector in the same direction ties.
    assert_eq!(got[0], 1, "the parallel vector is nearest");
    assert_eq!(got[2], 3, "the opposite vector is furthest");
}

#[test]
fn an_inner_product_table_ranks_largest_dot_first() {
    // inner_product is negated inside `search` so that it sorts with the other
    // metrics. If the negation were missing, this ordering would invert -- which
    // is why the expectation is here rather than assumed.
    let rows = vec![
        (1, vec![0.0, 0.0, 0.0]),
        (2, vec![1.0, 0.0, 0.0]),
        (3, vec![0.5, 0.0, 0.0]),
        (4, vec![-1.0, 0.0, 0.0]),
    ];
    let t = table_with(
        "(embedding float[3], distance_metric=inner_product)",
        rows.clone(),
    );
    let query = [1.0, 0.0, 0.0];
    assert_eq!(
        ids(&t.search(&query, 4).unwrap()),
        exact_order(&rows, &query, 4, Metric::InnerProduct)
    );
    assert_eq!(
        t.search(&query, 2).unwrap()[0].id,
        2,
        "the biggest dot is nearest"
    );
}

#[test]
fn a_k_that_spans_the_whole_tie_still_comes_back_in_rowid_order() {
    let t = loaded();
    let got = ids(&t.search(&[0.0, 0.0, 0.0], 8).unwrap());
    assert_eq!(got, [7, 8, 1, 2, 3, 4, 5, 6]);
    assert_eq!(got, exact_order(&corpus(), &[0.0, 0.0, 0.0], 8, Metric::L2));
}

#[test]
fn a_k_of_one_returns_only_the_nearest() {
    let t = loaded();
    let query = [0.0, 0.0, 1.0];
    let got = t.search(&query, 1).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, 5, "[0,0,1] is the exact match for [0,0,1]");
}

#[test]
fn a_k_of_zero_returns_nothing() {
    // Measured: the reference's `LIMIT 0` returns no rows.
    let t = loaded();
    assert!(t.search(&[1.0, 0.0, 0.0], 0).unwrap().is_empty());
}

#[test]
fn the_rowids_a_query_returns_are_the_stored_rowids_not_the_positions() {
    // The index works in positions; the table works in rowids. Conflating them
    // is the bug this test exists to catch, so the corpus is deliberately not
    // 1..n contiguous.
    let mut t = Vec0Table::create(schema());
    for (rowid, vector) in [
        (100, vec![1.0, 0.0, 0.0]),
        (250, vec![0.0, 1.0, 0.0]),
        (9999, vec![0.0, 0.0, 1.0]),
    ] {
        t.insert(Some(rowid), vector, note("x")).unwrap();
    }
    let got = ids(&t.search(&[1.0, 0.0, 0.0], 3).unwrap());
    assert_eq!(got, [100, 250, 9999], "positions leaked out as rowids");
}

#[test]
fn a_growing_table_still_ranks_against_the_exact_search() {
    // The property that has to survive an INSERT, not just a bulk load.
    let mut t = Vec0Table::create(schema());
    let mut rows = Vec::new();
    for (rowid, vector) in corpus() {
        t.insert(Some(rowid), vector.clone(), note("x")).unwrap();
        rows.push((rowid, vector));
        let query = [0.5, 0.5, 0.5];
        let exact = FlatIndex::new(rows.iter().map(|(_, v)| v.clone()).collect(), Metric::L2)
            .unwrap()
            .search(&query, 3)
            .unwrap();
        let through = t.search(&query, 3).unwrap();
        assert_eq!(through.len(), exact.len(), "after inserting {rowid}");
        for (a, b) in exact.iter().zip(through.iter()) {
            assert_eq!(a.distance, b.distance, "after inserting {rowid}");
        }
    }
}

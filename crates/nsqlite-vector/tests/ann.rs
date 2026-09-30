//! A query finds the rows it should, and reports them at their true distance.
//!
//! Two claims, and they are different claims.
//!
//! The *candidate set* may be approximate. HNSW is a graph, and a graph can
//! miss a neighbour; that is what the approximation buys. What it must never do
//! is report a wrong distance, or rank two rows wrongly against each other, or
//! return a row twice. So under §SearchMode::Ann§ every row that comes back is
//! rescored with the exact metric, and this file checks that against the exact
//! index row by row rather than trusting the pipeline.
//!
//! The *recall* is measured, not asserted at a magic constant. §recall_at_k§
//! against the exact answer is printed on every run, because a threshold that
//! passes today and fails after an unrelated change to the graph is a threshold
//! nobody can act on.
//!
//! The index is now built once and kept until a write drops it, so the second
//! half of this file is about the cache being *invalidated* rather than about
//! the cache existing: a query after an insert has to see the insert.

use nsqlite_vector::index_store::{SearchMode, StoredRow, Vec0Table};
use nsqlite_vector::search::{recall_at_k, Metric, Neighbor};
use nsqlite_vector::vtab::Schema;

/// A tiny reproducible LCG, for the same reason §tests/search.rs§ has one:
/// the graph is seeded, so the data has to be seeded too or a recall number
/// moves for reasons that have nothing to do with the index.
struct Lcg(u64);

impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn vector(&mut self, dim: usize) -> Vec<f64> {
        (0..dim).map(|_| self.next_f64() * 2.0 - 1.0).collect()
    }
}

fn schema() -> Schema {
    Schema::parse("v", "(embedding float[8], distance_metric=l2)").expect("the schema parses")
}

fn table_of(vectors: Vec<Vec<f64>>) -> Vec0Table {
    let schema = schema();
    let rows = vectors
        .into_iter()
        .enumerate()
        .map(|(i, vector)| StoredRow {
            rowid: i as i64 + 1,
            vector,
            metadata: Vec::new(),
        })
        .collect();
    Vec0Table::with_rows(schema, rows).expect("the rows fit the schema")
}

fn corpus(count: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut rng = Lcg(seed);
    (0..count).map(|_| rng.vector(8)).collect()
}

/// Points clustered around a few centres, which is what a recall measurement
/// needs and what uniform noise is not.
///
/// On uniform points a small-world graph has nothing to exploit and an ANN
/// search degenerates towards brute force, so recall comes out at 1.000 and the
/// number says nothing. Clustered points are what an embedding corpus actually
/// looks like and what makes a neighbour genuinely hard to reach: the ones on
/// the outside of a cluster. §tests/search.rs§ measures the same thing for the
/// same reason.
fn clustered(count: usize, clusters: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut rng = Lcg(seed);
    let centres: Vec<Vec<f64>> = (0..clusters).map(|_| rng.vector(8)).collect();
    (0..count)
        .map(|i| {
            centres[i % clusters]
                .iter()
                .map(|c| c + 0.15 * (rng.next_f64() * 2.0 - 1.0))
                .collect()
        })
        .collect()
}

/// The exact distance for one row, computed from the stored vector rather than
/// from either index, so it is a third opinion and not a comparison of the two
/// pipelines with each other.
fn true_distance(table: &Vec0Table, rowid: i64, query: &[f64]) -> f64 {
    let row = table.get(rowid).expect("the row is there").expect("it exists");
    schema()
        .metric
        .distance(query, &row.vector)
        .expect("both vectors are the declared width")
}

#[test]
fn an_ann_query_reports_every_row_at_its_true_distance() {
    let vectors = corpus(500, 0xA11CE);
    let table = table_of(vectors);
    let query = corpus(1, 0xBEEF).remove(0);

    let hits = table
        .search_with(&query, 10, SearchMode::Ann { ef: 64 })
        .expect("the query runs");

    assert_eq!(hits.len(), 10, "k rows come back");
    for hit in &hits {
        let expected = true_distance(&table, hit.id as i64, &query);
        assert_eq!(
            hit.distance, expected,
            "row {} was reported at a distance the metric does not produce",
            hit.id
        );
    }
}

#[test]
fn an_ann_query_returns_no_row_twice() {
    let table = table_of(corpus(300, 0xD00D));
    let query = corpus(1, 0xF00D).remove(0);
    let hits = table
        .search_with(&query, 20, SearchMode::Ann { ef: 128 })
        .expect("the query runs");
    let mut ids: Vec<usize> = hits.iter().map(|h| h.id).collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "a row came back twice");
}

#[test]
fn an_ann_query_never_returns_a_row_the_exact_search_would_not() {
    // Below the size at which a graph can miss anything, the two answers are
    // the same. That is the property that makes approximate search safe to
    // substitute: it can be worse, but not different in kind.
    let table = table_of(corpus(40, 0x1234));
    let query = corpus(1, 0x5678).remove(0);
    let exact = table.search(&query, 8).expect("exact search runs");
    let approx = table
        .search_with(&query, 8, SearchMode::Ann { ef: 64 })
        .expect("the query runs");
    assert_eq!(
        approx.iter().map(|h| h.id).collect::<Vec<_>>(),
        exact.iter().map(|h| h.id).collect::<Vec<_>>(),
        "on a table this small the two modes must agree exactly"
    );
}

#[test]
fn the_approximate_answer_is_measured_against_the_exact_one() {
    let table = table_of(clustered(2000, 12, 0x5EED));
    let mut rng = Lcg(0xC0FFEE);
    let mut total = 0.0f64;
    let queries = 25;
    for _ in 0..queries {
        let query = rng.vector(8);
        let exact = table.search(&query, 10).expect("exact search runs");
        let approx = table
            .search_with(&query, 10, SearchMode::Ann { ef: 64 })
            .expect("the query runs");
        total += recall_at_k(&exact, &approx, 10);
    }
    let recall = total / queries as f64;
    // Printed, not just asserted: a recall number nobody can see is a number
    // nobody can watch move when the graph changes.
    println!("recall@10 over {queries} queries, ef=64: {recall:.3}");
    assert!(
        recall >= 0.9,
        "recall {recall:.3} is below the 0.9 this configuration is expected to hold"
    );
}

#[test]
fn a_larger_pool_buys_recall_and_a_smaller_one_costs_it() {
    // The claim is directional, not absolute: ef is the knob, and turning it up
    // must not make recall worse.
    let table = table_of(clustered(2000, 12, 0x5EED));
    let mut rng = Lcg(0xC0FFEE);
    let mut queries = Vec::new();
    for _ in 0..15 {
        queries.push(rng.vector(8));
    }
    let rate = |ef: usize| -> f64 {
        let mut total = 0.0;
        for q in &queries {
            let exact = table.search(q, 10).expect("exact search runs");
            let approx = table
                .search_with(q, 10, SearchMode::Ann { ef })
                .expect("the query runs");
            total += recall_at_k(&exact, &approx, 10);
        }
        total / queries.len() as f64
    };
    let narrow = rate(16);
    let wide = rate(256);
    println!("recall@10: ef=16 -> {narrow:.3}, ef=256 -> {wide:.3}");
    assert!(
        wide >= narrow,
        "a 16x larger pool ({wide:.3}) scored below a small one ({narrow:.3})"
    );
}

#[test]
fn the_pool_is_never_smaller_than_the_answer_asked_for() {
    let table = table_of(corpus(500, 0x9E9E));
    let query = corpus(1, 0x1A1A).remove(0);
    // ef below k is a pool that cannot hold the answer. It is clamped, not
    // honoured, and the clamp is what stops it coming back short.
    let hits = table
        .search_with(&query, 20, SearchMode::Ann { ef: 1 })
        .expect("the query runs");
    assert_eq!(hits.len(), 20, "a pool smaller than k is widened to k");
}

#[test]
fn a_query_after_a_write_sees_the_write() {
    let mut table = table_of(corpus(300, 0x4242));
    let query = corpus(1, 0x2424).remove(0);
    let before = table.search(&query, 5).expect("the first query runs");
    assert!(
        !before.iter().any(|h| h.id == 999),
        "row 999 is not there yet"
    );

    // Exactly the query's own vector, so it must come back at distance zero and
    // be the nearest thing in the table.
    let mut rng = Lcg(0x2424);
    let _ = &mut rng;
    table.insert(Some(999), query.clone(), Vec::new()).expect("insert");
    let after = table.search(&query, 5).expect("the second query runs");
    let hit = after
        .iter()
        .find(|h| h.id == 999)
        .expect("the row just written is visible to the next query");
    assert_eq!(hit.distance, 0.0, "a row queried against itself is at distance zero");
    assert_eq!(after[0].id, 999, "and it is the nearest row in the table");
}

#[test]
fn a_deleted_row_stops_coming_back() {
    let mut table = table_of(corpus(200, 0x7777));
    let query = corpus(1, 0x8888).remove(0);
    let victim = table.search(&query, 1).expect("a query runs")[0].id as i64;
    assert!(table.delete(victim).expect("delete runs"), "the row was there");
    let after = table.search(&query, 5).expect("the next query runs");
    assert!(
        !after.iter().any(|h| h.id as i64 == victim),
        "a deleted row came back from the cached index"
    );
}

#[test]
fn an_update_is_visible_to_the_next_query() {
    let mut table = table_of(corpus(200, 0x3131));
    let query = corpus(1, 0x1414).remove(0);
    let victim = table.search(&query, 1).expect("a query runs")[0].id as i64;
    table
        .update(victim, query.clone())
        .expect("the update runs");
    let after = table.search(&query, 3).expect("the next query runs");
    assert_eq!(
        after[0].id as i64, victim,
        "the updated row is now the nearest, and the cache has to know it"
    );
    assert_eq!(after[0].distance, 0.0);
}

#[test]
fn repeated_queries_against_a_cached_index_return_the_same_thing() {
    let table = table_of(corpus(800, 0x6060));
    let query = corpus(1, 0x0F0F).remove(0);
    let first = table.search(&query, 12).expect("the first query runs");
    let second = table.search(&query, 12).expect("the second query runs");
    assert_eq!(
        first.iter().map(|h| h.id).collect::<Vec<_>>(),
        second.iter().map(|h| h.id).collect::<Vec<_>>(),
        "a cached index has to answer the same question the same way"
    );
    let first_ann = table
        .search_with(&query, 12, SearchMode::Ann { ef: 64 })
        .expect("an approximate query runs");
    let second_ann = table
        .search_with(&query, 12, SearchMode::Ann { ef: 64 })
        .expect("the next approximate query runs");
    assert_eq!(
        first_ann.iter().map(|h| h.id).collect::<Vec<_>>(),
        second_ann.iter().map(|h| h.id).collect::<Vec<_>>(),
        "and the same for the approximate path"
    );
}

#[test]
fn exact_search_still_misses_nothing_on_a_table_too_big_to_hold() {
    // The mode is a choice, not a replacement: Exact is the default, and a
    // caller that has not asked for an approximation must not get one.
    let table = table_of(clustered(2000, 12, 0x5EED));
    let query = corpus(1, 0x1111).remove(0);
    let hits = table.search(&query, 10).expect("the query runs");
    // Brute force over 2000 rows, so every answer is the true one.
    assert_eq!(hits.len(), 10);
    for hit in &hits {
        assert_eq!(hit.distance, true_distance(&table, hit.id as i64, &query));
    }
    assert_eq!(SearchMode::default(), SearchMode::Exact);
}

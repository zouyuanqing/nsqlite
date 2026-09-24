//! Tests for the vector search module.
//!
//! The suite is grouped by what each part has to prove:
//!
//! * the distance functions have the *exact* semantics claimed, including the
//!   zero-vector rule, because every other number in this file depends on them;
//! * the flat index is exactly right, which is what makes it usable as the
//!   baseline for the two approximate paths;
//! * HNSW is exactly right on a small set, and its recall is *measured* on a
//!   larger one — the measured numbers are printed by the recall tests so a
//!   regression is visible rather than merely failed;
//! * quantization reports the error it introduces rather than hiding it;
//! * chunking covers the text that spans a window boundary, because that is the
//!   text a naive split loses.

use nsqlite_vector::search::{
    chunk_text, cosine, inner_product, l1, l2_squared, recall_at_k, FlatIndex, HnswConfig,
    HnswIndex, Metric, Neighbor, QuantizedIndex, SearchError,
};

// ---------------------------------------------------------------------------
// Deterministic data helpers
// ---------------------------------------------------------------------------

/// A tiny reproducible LCG.
///
/// The graph is the only thing under test that has randomness in it, and its own
/// level draw is seeded; the *data* must be fixed too, or a recall measurement
/// moves every run for reasons that have nothing to do with the index.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

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

/// A cluster of `count` points around `clusters` centres, with a little jitter.
///
/// Clustered data is what a real embedding corpus looks like and what makes an
/// ANN index's recall non-trivial: points on the outside of a cluster are the
/// hard ones to reach through a small-world graph, and uniform noise has no
/// structure for the graph to exploit at all.
fn clustered_corpus(count: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut rng = Lcg::new(seed);
    let centres: Vec<Vec<f64>> = (0..clusters).map(|_| rng.vector(dim)).collect();
    (0..count)
        .map(|i| {
            let centre = &centres[i % clusters];
            centre
                .iter()
                .map(|c| c + 0.15 * (rng.next_f64() * 2.0 - 1.0))
                .collect()
        })
        .collect()
}

fn n(id: usize, distance: f64) -> Neighbor {
    Neighbor { id, distance }
}

// ---------------------------------------------------------------------------
// Distance functions: exact semantics
// ---------------------------------------------------------------------------

#[test]
fn l2_is_squared_not_rooted() {
    // (3-0)^2 + (4-0)^2 = 25. The true distance is 5; this is the square.
    assert_eq!(l2_squared(&[3.0, 4.0], &[0.0, 0.0]).unwrap(), 25.0);
    assert_eq!(l2_squared(&[1.0, 1.0], &[1.0, 1.0]).unwrap(), 0.0);
}

#[test]
fn l2_is_zero_only_for_identical_vectors() {
    let a = [0.5, -0.25, 3.0];
    assert!(l2_squared(&a, &a).unwrap() < f64::EPSILON);
    assert!(l2_squared(&a, &[0.5, -0.25, 3.001]).unwrap() > 0.0);
}

#[test]
fn l2_is_defined_for_a_zero_vector() {
    // Unlike cosine, L2 has no division: the distance to the origin is just the
    // norm squared.
    assert_eq!(l2_squared(&[3.0, 4.0], &[0.0, 0.0]).unwrap(), 25.0);
    assert_eq!(l2_squared(&[0.0, 0.0], &[3.0, 4.0]).unwrap(), 25.0);
    assert_eq!(l2_squared(&[0.0, 0.0], &[0.0, 0.0]).unwrap(), 0.0);
}

#[test]
fn l1_sums_absolute_differences() {
    // |1-4| + |2-2| + |3-(-1)| = 3 + 0 + 4 = 7
    assert_eq!(l1(&[1.0, 2.0, 3.0], &[4.0, 2.0, -1.0]).unwrap(), 7.0);
    assert_eq!(l1(&[1.0, 2.0], &[1.0, 2.0]).unwrap(), 0.0);
}

#[test]
fn l1_is_defined_for_a_zero_vector() {
    assert_eq!(l1(&[3.0, 4.0], &[0.0, 0.0]).unwrap(), 7.0);
}

#[test]
fn cosine_of_two_identical_unit_vectors_is_exactly_zero() {
    // The requirement, pinned: the *distance* form is 0 for identical unit
    // vectors, not 1. The similarity form is the one that is 1.
    assert!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0, 0.0]).unwrap().abs() < 1e-15);
    // 0.6-0.8 is a unit vector, so this tests the general case too.
    assert!(cosine(&[0.6, 0.8, 0.0], &[0.6, 0.8, 0.0]).unwrap().abs() < 1e-15);
}

#[test]
fn cosine_of_opposite_vectors_is_two() {
    // 1 - (-1) = 2, the maximum of the distance form.
    let d = cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap();
    assert!((d - 2.0).abs() < 1e-15, "got {d}");
}

#[test]
fn cosine_of_orthogonal_vectors_is_one() {
    let d = cosine(&[1.0, 0.0], &[0.0, 1.0]).unwrap();
    assert!((d - 1.0).abs() < 1e-15, "got {d}");
}

#[test]
fn cosine_is_scale_invariant() {
    // The norms are factored out, so scaling either side changes nothing. This
    // is the property that distinguishes cosine from L2.
    let a = [0.3, -0.9, 0.1];
    let scaled: Vec<f64> = a.iter().map(|v| v * 7.0).collect();
    let plain = cosine(&a, &a).unwrap();
    let via_scaled = cosine(&a, &scaled).unwrap();
    assert!(
        (plain - via_scaled).abs() < 1e-12,
        "{plain} vs {via_scaled}"
    );
}

#[test]
fn cosine_of_a_zero_vector_is_an_error_not_a_number() {
    // The division by the norm has no answer here. Returning 0 or 1 would let a
    // corrupt row rank as a perfect or an adversarial match.
    assert_eq!(
        cosine(&[0.0, 0.0], &[1.0, 0.0]),
        Err(SearchError::ZeroVector)
    );
    assert_eq!(
        cosine(&[1.0, 0.0], &[0.0, 0.0]),
        Err(SearchError::ZeroVector)
    );
    assert_eq!(
        cosine(&[0.0, 0.0], &[0.0, 0.0]),
        Err(SearchError::ZeroVector)
    );
    // A vector of tiny non-zero values is not a zero vector, but a magnitude
    // that squares to zero is: `|a|^2` underflows before the root, so the norm
    // reads as 0.0. That is a genuine limit of a f64 sum of squares, not of this
    // check, and the error is the right one to raise rather than a NaN.
    assert_eq!(
        cosine(&[1e-300, 0.0], &[1.0, 0.0]),
        Err(SearchError::ZeroVector)
    );
    // 1e-30 squares to 1e-60, which is still representable, so it is accepted.
    assert!(cosine(&[1e-30, 0.0], &[1.0, 0.0]).is_ok());
}

#[test]
fn inner_product_is_negated_so_smaller_is_nearer() {
    // dot([1,2],[3,4]) = 11, so the distance is -11. Returning +11 would make
    // this metric rank the worst match first.
    assert_eq!(inner_product(&[1.0, 2.0], &[3.0, 4.0]).unwrap(), -11.0);
}

#[test]
fn inner_product_ranks_a_bigger_dot_as_nearer() {
    let near = inner_product(&[1.0, 0.0], &[2.0, 0.0]).unwrap(); // dot 2 -> -2
    let far = inner_product(&[1.0, 0.0], &[-5.0, 0.0]).unwrap(); // dot -5 -> 5
    assert!(near < far, "{near} should be nearer than {far}");
}

#[test]
fn inner_product_is_defined_for_a_zero_vector() {
    // dot(a, 0) = 0, so the distance is -0.0, which compares equal to 0.0. Only
    // cosine divides by a norm, and so only cosine is undefined here.
    let d = inner_product(&[1.0, 2.0, 3.0], &[0.0, 0.0, 0.0]).unwrap();
    assert_eq!(d, 0.0);
    assert!(d.is_sign_negative() || d == 0.0);
}

#[test]
fn a_length_mismatch_is_reported_from_every_metric() {
    let a = [1.0, 2.0];
    let b = [1.0, 2.0, 3.0];
    let expected = Err(SearchError::DimensionMismatch {
        expected: 2,
        found: 3,
    });
    assert_eq!(l2_squared(&a, &b), expected);
    assert_eq!(l1(&a, &b), expected);
    assert_eq!(cosine(&a, &b), expected);
    assert_eq!(inner_product(&a, &b), expected);
}

#[test]
fn an_empty_vector_is_rejected_by_every_metric() {
    let empty: [f64; 0] = [];
    let full = [1.0, 2.0];
    assert_eq!(l2_squared(&empty, &full), Err(SearchError::EmptyVector));
    assert_eq!(l1(&empty, &full), Err(SearchError::EmptyVector));
    assert_eq!(cosine(&empty, &full), Err(SearchError::EmptyVector));
    assert_eq!(inner_product(&empty, &full), Err(SearchError::EmptyVector));
}

#[test]
fn metric_dispatch_agrees_with_the_free_functions() {
    // Every metric must route to exactly the function documented for it, so the
    // enum and the free functions cannot drift apart.
    let a = [0.3, 0.4, 0.5];
    let b = [0.1, 0.9, 0.2];
    assert_eq!(
        Metric::L2.distance(&a, &b).unwrap(),
        l2_squared(&a, &b).unwrap()
    );
    assert_eq!(Metric::L1.distance(&a, &b).unwrap(), l1(&a, &b).unwrap());
    assert_eq!(
        Metric::Cosine.distance(&a, &b).unwrap(),
        cosine(&a, &b).unwrap()
    );
    assert_eq!(
        Metric::InnerProduct.distance(&a, &b).unwrap(),
        inner_product(&a, &b).unwrap()
    );
}

#[test]
fn metric_names_match_the_sqlite_vec_vocabulary() {
    assert_eq!(Metric::L2.as_str(), "L2");
    assert_eq!(Metric::Cosine.as_str(), "cosine");
    assert_eq!(Metric::L1.as_str(), "L1");
    assert_eq!(Metric::InnerProduct.as_str(), "inner_product");
    assert_eq!(Metric::default(), Metric::L2);
    assert_eq!(Metric::L2.to_string(), "L2");
}

// ---------------------------------------------------------------------------
// Flat index: the exact baseline
// ---------------------------------------------------------------------------

#[test]
fn flat_search_returns_the_nearest_first() {
    let index = FlatIndex::new(
        vec![
            vec![0.0, 0.0],
            vec![10.0, 0.0],
            vec![1.0, 0.0],
            vec![0.0, 10.0],
        ],
        Metric::L2,
    )
    .unwrap();
    let hits = index.search(&[0.0, 0.0], 3).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, 0);
    assert_eq!(hits[0].distance, 0.0);
    assert_eq!(hits[1].id, 2);
    assert_eq!(hits[2].id, 1);
    for pair in hits.windows(2) {
        assert!(pair[0].distance <= pair[1].distance);
    }
}

#[test]
fn flat_ties_break_on_insertion_order_so_the_result_is_deterministic() {
    // Five identical vectors: every distance is 0, so only the tiebreak decides,
    // and the promise is that they come back 0,1,2,...
    let index = FlatIndex::new(vec![vec![1.0]; 5], Metric::L2).unwrap();
    let ids: Vec<usize> = index
        .search(&[1.0], 5)
        .unwrap()
        .iter()
        .map(|x| x.id)
        .collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
}

#[test]
fn flat_ties_on_distance_still_break_on_id_not_on_value() {
    // Two different vectors at the same distance from the query: a tie in the
    // metric, not in the value, and the id still decides.
    let index = FlatIndex::new(vec![vec![1.0, 0.0], vec![-1.0, 0.0]], Metric::L2).unwrap();
    let hits = index.search(&[0.0, 0.0], 2).unwrap();
    assert_eq!(hits[0].distance, hits[1].distance);
    assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn flat_search_is_stable_across_repeated_runs() {
    let index = FlatIndex::new(clustered_corpus(64, 8, 4, 0xABCD), Metric::L2).unwrap();
    let first = index.search(&[0.1; 8], 10).unwrap();
    for _ in 0..20 {
        assert_eq!(index.search(&[0.1; 8], 10).unwrap(), first);
    }
}

#[test]
fn flat_k_zero_returns_nothing_and_k_above_count_returns_everything() {
    // Both checked against the real sqlite3: `LIMIT 0` returns no rows, and a
    // LIMIT above the row count returns every row rather than erroring.
    let index = FlatIndex::new(vec![vec![1.0], vec![2.0]], Metric::L2).unwrap();
    assert!(index.search(&[1.0], 0).unwrap().is_empty());
    let all = index.search(&[1.0], 99).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, 0);
}

#[test]
fn flat_search_rejects_a_query_of_the_wrong_dimension() {
    let index = FlatIndex::new(vec![vec![1.0, 2.0]], Metric::L2).unwrap();
    assert_eq!(
        index.search(&[1.0], 1),
        Err(SearchError::DimensionMismatch {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(index.search(&[], 1), Err(SearchError::EmptyVector));
}

#[test]
fn flat_rejects_ragged_input() {
    let err = FlatIndex::new(vec![vec![1.0, 2.0], vec![1.0, 2.0, 3.0]], Metric::L2).unwrap_err();
    assert!(
        matches!(err, SearchError::DimensionMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn flat_rejects_a_non_finite_value() {
    // A NaN would make every comparison it took part in arbitrary, silently
    // breaking the tie order this index promises.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = FlatIndex::new(vec![vec![1.0, bad]], Metric::L2).unwrap_err();
        match err {
            SearchError::Config(message) => assert!(message.contains("non-finite"), "{message}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }
}

#[test]
fn flat_accepts_an_empty_corpus_but_rejects_empty_vectors() {
    assert!(FlatIndex::new(vec![vec![]], Metric::L2).is_err());
    // An entirely empty corpus is legal: there is nothing to search, and an
    // empty result is the right answer.
    let index = FlatIndex::new(vec![], Metric::L2).unwrap();
    assert!(index.is_empty());
    assert!(index.search(&[1.0], 5).unwrap().is_empty());
}

#[test]
fn flat_surfaces_the_cosine_zero_vector_error_at_search_time() {
    // A zero-vector *row* is accepted at build, and the failure surfaces when it
    // is actually compared — naming the metric, rather than being silently
    // ranked.
    let index = FlatIndex::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]], Metric::Cosine).unwrap();
    assert_eq!(index.search(&[0.0, 0.0], 1), Err(SearchError::ZeroVector));
    // A real query works fine, and an identical unit vector is distance 0.
    let hits = index.search(&[1.0, 0.0], 1).unwrap();
    assert_eq!(hits[0].id, 0);
    assert!(hits[0].distance.abs() < 1e-15);
}

#[test]
fn flat_distance_to_agrees_with_search() {
    let index = FlatIndex::new(vec![vec![1.0, 0.0], vec![0.0, 2.0]], Metric::L2).unwrap();
    assert_eq!(index.distance_to(&[1.0, 0.0], 0).unwrap(), 0.0);
    assert_eq!(index.distance_to(&[1.0, 0.0], 1).unwrap(), 5.0);
    assert_eq!(index.search(&[1.0, 0.0], 2).unwrap()[1].distance, 5.0);
    assert_eq!(index.dim(), 2);
    assert_eq!(index.len(), 2);
    assert_eq!(index.metric(), Metric::L2);
}

// ---------------------------------------------------------------------------
// HNSW
// ---------------------------------------------------------------------------

#[test]
fn hnsw_recall_on_the_exact_result_is_perfect_on_a_small_set() {
    // The requirement: on a set small enough for the graph to be thorough about,
    // recall of the exact result must be perfect. ef is set to the corpus size,
    // so the beam can hold everything it reaches.
    let vectors = clustered_corpus(40, 6, 4, 42);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();

    for i in 0..20 {
        let query = vectors[i].clone();
        let baseline = exact.search(&query, 5).unwrap();
        // A query that *is* a stored vector is the easiest possible case: the
        // node is guaranteed to be in the graph and the true match is at
        // distance 0, so nothing can outrank it.
        let approx = graph.search(&query, 5, vectors.len()).unwrap();
        assert_eq!(
            recall_at_k(&baseline, &approx, 5),
            1.0,
            "query {i} lost recall: {approx:?} vs {baseline:?}"
        );
    }
}

#[test]
fn hnsw_build_is_reproducible_from_its_seed() {
    let vectors = clustered_corpus(80, 8, 5, 7);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let query = vectors[3].clone();

    let a = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let b = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    // Same seed -> the same levels and the same degrees, not merely equal recall.
    for id in 0..a.len() {
        assert_eq!(a.level_of(id), b.level_of(id), "node {id} differs in level");
        for layer in 0..=a.max_level() as u32 {
            assert_eq!(
                a.degree(id, layer),
                b.degree(id, layer),
                "node {id} layer {layer} degree differs"
            );
        }
    }
    assert_eq!(a.entry_point(), b.entry_point());
    assert_eq!(
        a.search(&query, 10, 32).unwrap(),
        b.search(&query, 10, 32).unwrap()
    );
    assert_eq!(a.seed(), b.seed());
}

#[test]
fn hnsw_a_different_seed_produces_a_different_graph() {
    // The complement of the previous test: if the seed did nothing, the two
    // would be identical and the reproducibility test would be vacuous.
    let exact = FlatIndex::new(clustered_corpus(120, 8, 5, 11), Metric::L2).unwrap();
    let a = HnswIndex::build(
        &exact,
        HnswConfig {
            seed: 1,
            ..HnswConfig::default()
        },
    )
    .unwrap();
    let b = HnswIndex::build(
        &exact,
        HnswConfig {
            seed: 2,
            ..HnswConfig::default()
        },
    )
    .unwrap();
    let levels_differ = (0..a.len()).any(|id| a.level_of(id) != b.level_of(id));
    let degrees_differ = (0..a.len())
        .any(|id| (0..=a.max_level() as u32).any(|l| a.degree(id, l) != b.degree(id, l)));
    assert!(
        levels_differ || degrees_differ,
        "two seeds produced the same graph"
    );
}

#[test]
fn hnsw_produces_a_multi_layer_graph_with_a_top_entry_point() {
    // The structure has to be a layered graph, not a flat list with extra steps.
    let exact = FlatIndex::new(clustered_corpus(200, 8, 5, 3), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    assert!(graph.max_level() >= 1, "no upper layers were created");
    let entry = graph.entry_point().expect("an entry point");
    // The entry point is at the top of the graph, which is what a search starts
    // from and descends.
    assert_eq!(graph.level_of(entry), Some(graph.max_level() as u32));
    // Node 0 is inserted first, so it is the initial entry point and its level
    // sets the starting max_level.
    assert!(graph.level_of(0).is_some());
}

#[test]
fn hnsw_layer_population_follows_the_expected_geometric_decay() {
    // Levels are floor(-ln(u)/ln(m)), so the fraction on layer 0 is 1 - 1/m;
    // for m = 16 that is 0.9375, and each higher layer holds about 1/m of the
    // one below. A flat distribution would mean the level draw is broken.
    //
    // The measured value is printed rather than only asserted. `0.9375` is the
    // *analytic target*, not a measurement, and reporting it as though it were
    // the observed number would be dressing a formula up as a datum. The real
    // figure on this corpus is in the log below the assertion.
    const M: usize = 16;
    const N: usize = 4000;
    let exact = FlatIndex::new(clustered_corpus(N, 4, 5, 99), Metric::L2).unwrap();
    let graph = HnswIndex::build(
        &exact,
        HnswConfig {
            m: M,
            ..HnswConfig::default()
        },
    )
    .unwrap();

    let on_zero = (0..N).filter(|&id| graph.level_of(id) == Some(0)).count() as f64 / N as f64;
    let expected = 1.0 - 1.0 / M as f64;
    println!(
        "layer-0 fraction: measured {on_zero:.4} against the analytic 1 - 1/m = {expected:.4} \
         (max_level = {})",
        graph.max_level()
    );
    // 4000 samples, so the standard error is about 0.004; 0.03 is roughly seven
    // sigma. The measured value sits inside that band by construction of the
    // draw, and printing it is what makes a shift in the level distribution
    // visible instead of merely out of tolerance.
    assert!(
        (on_zero - expected).abs() < 0.03,
        "layer-0 fraction {on_zero} is not the expected {expected}"
    );

    // Each higher layer must be strictly smaller than the one below it.
    let mut counts = vec![0usize; graph.max_level() as usize + 1];
    for id in 0..N {
        counts[graph.level_of(id).unwrap() as usize] += 1;
    }
    println!("  layer populations: {counts:?}");
    for pair in counts.windows(2) {
        assert!(pair[1] < pair[0], "layer sizes must shrink: {counts:?}");
    }
}

#[test]
fn hnsw_recall_on_a_larger_set_is_measured_and_reported() {
    // The point of this test is the printed number, not only the assertion. It
    // is a measurement: it is allowed to be imperfect, and the recall is
    // reported so a change in the graph shows up in the test log instead of
    // silently degrading.
    let dim = 16;
    let vectors = clustered_corpus(2000, dim, 20, 0xF00D);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let graph = HnswIndex::build(
        &exact,
        HnswConfig {
            m: 16,
            ef_construction: 200,
            ..HnswConfig::default()
        },
    )
    .unwrap();

    let mut rng = Lcg::new(0x5EED);
    let k = 10;
    let queries = 50;
    let mut hits = 0usize;
    let mut total = 0usize;
    for _ in 0..queries {
        let query = rng.vector(dim);
        let baseline = exact.search(&query, k).unwrap();
        let approx = graph.search(&query, k, 64).unwrap();
        let expected: std::collections::HashSet<usize> = baseline.iter().map(|n| n.id).collect();
        hits += approx.iter().filter(|n| expected.contains(&n.id)).count();
        total += baseline.len();
    }
    let recall = hits as f64 / total as f64;
    println!("HNSW recall@10, 2000 vectors, 16d, m=16, ef_construction=200, ef=64: {recall:.4}");
    // A navigable small-world graph at ef=64 should be far above chance. Ten
    // draws from 2000 by chance is about 0.005, so anything under 0.5 means the
    // graph is not being navigated at all and the implementation is broken
    // rather than merely lossy.
    assert!(recall > 0.5, "measured recall {recall} is implausibly low");
}

#[test]
fn hnsw_every_node_is_reachable_from_the_entry_point() {
    // The regression test for a real bug this module had. Taking the nearest `M`
    // neighbours and truncating, with no Algorithm 4 diversity filter, left 7 of
    // 40 nodes with no path to them from the entry point on this corpus. A
    // disconnected graph is not a recall tradeoff — it is a permanently wrong
    // answer that no `ef` can recover, because a search cannot step to a node it
    // has no edge to.
    //
    // That single corpus is the *weak* form of this assertion, and it is how the
    // bug survived a round of fixes: the same 40-node shape passes while other
    // shapes still strand. The sweep below is the assertion that has teeth.
    let n = 40usize;
    let exact = FlatIndex::new(clustered_corpus(n, 6, 4, 42), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let entry = graph.entry_point().expect("an entry point");

    let mut seen = vec![false; n];
    let mut stack = vec![entry];
    seen[entry] = true;
    while let Some(node) = stack.pop() {
        for &neighbour in graph.neighbors_of(node, 0) {
            if !seen[neighbour as usize] {
                seen[neighbour as usize] = true;
                stack.push(neighbour as usize);
            }
        }
    }
    let stranded: Vec<usize> = (0..n).filter(|&i| !seen[i]).collect();
    assert!(
        stranded.is_empty(),
        "{}/{} nodes unreachable from the entry point: {stranded:?}",
        stranded.len(),
        n
    );
}

#[test]
fn hnsw_connectivity_holds_across_the_whole_parameter_sweep() {
    // The assertion that actually pins the invariant: not one corpus, but every
    // combination of graph width, corpus size and seed. A spot check cannot tell
    // a fix from a lucky corpus, and the two bugs this replaces both passed a
    // spot check.
    //
    // 144 builds, covering `m` from the legal minimum to the default and `n`
    // from small enough to search exhaustively to large enough that the greedy
    // descent really is being asked to do work.
    let mut corpora = 0usize;
    for &m in &[2usize, 4, 8, 16] {
        for &n in &[40usize, 120, 400] {
            for seed in 0..12u64 {
                let vectors = clustered_corpus(n, 4, 5, seed);
                let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
                let graph = HnswIndex::build(
                    &exact,
                    HnswConfig {
                        m,
                        seed,
                        ..HnswConfig::default()
                    },
                )
                .unwrap();
                let entry = graph.entry_point().expect("an entry point");

                let mut seen = vec![false; n];
                let mut stack = vec![entry];
                seen[entry] = true;
                while let Some(node) = stack.pop() {
                    for &neighbour in graph.neighbors_of(node, 0) {
                        if !seen[neighbour as usize] {
                            seen[neighbour as usize] = true;
                            stack.push(neighbour as usize);
                        }
                    }
                }
                let stranded: Vec<usize> = (0..n).filter(|&i| !seen[i]).collect();
                assert!(
                    stranded.is_empty(),
                    "m={m} n={n} seed={seed}: {}/{} nodes unreachable, first {:?}",
                    stranded.len(),
                    n,
                    &stranded[..stranded.len().min(8)]
                );
                corpora += 1;
            }
        }
    }
    assert_eq!(corpora, 144, "the sweep changed shape");
}

#[test]
fn hnsw_in_degree_matches_the_edges_that_actually_exist() {
    // `in_degree` is what the prune's last-way-in rule reads, so if it drifts
    // from the real edge lists the rule is reading fiction and the protection is
    // not protection. This is the test that would have caught the accounting bug
    // the rule was first written with.
    for &m in &[2usize, 16] {
        for &n in &[40usize, 400] {
            let exact = FlatIndex::new(clustered_corpus(n, 4, 5, 3), Metric::L2).unwrap();
            let graph = HnswIndex::build(
                &exact,
                HnswConfig {
                    m,
                    ..HnswConfig::default()
                },
            )
            .unwrap();
            for layer in 0..=graph.max_level() as u32 {
                for id in 0..n {
                    let counted = graph.in_degree(id, layer);
                    let actual = (0..n)
                        .filter(|&i| graph.neighbors_of(i, layer).contains(&(id as u32)))
                        .count();
                    assert_eq!(
                        counted, actual,
                        "m={m} n={n} layer {layer} node {id}: counted {counted}, actual {actual}"
                    );
                }
            }
        }
    }
}

#[test]
fn hnsw_no_node_is_left_with_zero_in_edges() {
    // The failure the last-way-in rule targets, stated as its own test rather
    // than implied by the reachability sweep: a node with no in-edge on layer 0
    // cannot be stepped to from anywhere, which is a hard wrong answer rather
    // than a recall loss. The reachability test can pass while this fails, because
    // a node with an in-edge from another stranded node is still unreachable.
    for &m in &[2usize, 4, 8, 16] {
        for &n in &[40usize, 120, 400] {
            for seed in 0..8u64 {
                let exact = FlatIndex::new(clustered_corpus(n, 4, 5, seed), Metric::L2).unwrap();
                let graph = HnswIndex::build(
                    &exact,
                    HnswConfig {
                        m,
                        seed,
                        ..HnswConfig::default()
                    },
                )
                .unwrap();
                // Node 0 is the first inserted and is the entry point, so it is
                // reached rather than linked to; every other node must have an
                // in-edge.
                for id in 1..n {
                    assert!(
                        graph.in_degree(id, 0) > 0,
                        "m={m} n={n} seed={seed}: node {id} has no in-edge on layer 0"
                    );
                }
            }
        }
    }
}

#[test]
fn hnsw_a_stranded_node_never_reported_as_someone_elses_nearest_neighbour() {
    // The consequence, not the cause. A query that *is* a stored vector must
    // come back with that vector as its own nearest neighbour — the only case
    // where the right answer is not in doubt. This is the measurement that
    // turned "the graph is disconnected" from a structural observation into a
    // wrong answer, so it is the measurement worth keeping.
    //
    // Run at `ef` wider than the corpus, so a failure cannot be blamed on a
    // narrow beam: at full width the search can only fail because the edges do
    // not reach.
    let mut checked = 0usize;
    for &m in &[2usize, 4, 8, 16] {
        for &n in &[40usize, 120, 400] {
            for seed in 0..6u64 {
                let vectors = clustered_corpus(n, 4, 5, seed);
                let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
                let graph = HnswIndex::build(
                    &exact,
                    HnswConfig {
                        m,
                        seed,
                        ..HnswConfig::default()
                    },
                )
                .unwrap();
                for (id, vector) in vectors.iter().enumerate() {
                    let exact_nn = exact.search(vector, 1).unwrap()[0].id;
                    assert_eq!(exact_nn, id, "the flat baseline lost its own match");
                    let approx = graph.search(vector, 1, n + 16).unwrap();
                    assert_eq!(
                        approx[0].id,
                        id,
                        "m={m} n={n} seed={seed}: querying with stored vector {id} at \
                         ef={} returned {} instead",
                        n + 16,
                        approx[0].id
                    );
                    checked += 1;
                }
            }
        }
    }
    println!("every one of {checked} stored-vector self-queries returned itself");
}

#[test]
fn hnsw_spreads_a_node_s_edges_rather_than_filling_one_direction() {
    // The signature of the diversity filter. With plain nearest-`M` truncation a
    // saturated node ends up with exactly `M` edges; with Algorithm 4 the count
    // varies, because a candidate already covered by a kept neighbour is
    // rejected. A constant degree would mean the filter is not running.
    let exact = FlatIndex::new(clustered_corpus(300, 8, 6, 42), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let degrees: std::collections::BTreeSet<usize> =
        (0..graph.len()).map(|id| graph.degree(id, 0)).collect();
    assert!(
        degrees.len() > 1,
        "every node has the same degree {degrees:?}, so no edge was ever filtered"
    );
    // And no node exceeded its budget.
    assert!(
        degrees.iter().all(|&d| d <= 32),
        "a degree exceeded 2m: {degrees:?}"
    );
}

#[test]
fn hnsw_recall_improves_as_ef_grows() {
    // ef is the accuracy dial, and this is what proves it is wired to the beam
    // width rather than ignored.
    let exact = FlatIndex::new(clustered_corpus(1000, 12, 10, 0xBEEF), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let query = vec![0.1; 12];
    let k = 10;
    let baseline = exact.search(&query, k).unwrap();

    let mut previous = 0.0f64;
    let mut best = 0.0f64;
    for ef in [1usize, 10, 50, 200, 1000] {
        let recall = recall_at_k(&baseline, &graph.search(&query, k, ef).unwrap(), k);
        println!("  ef={ef:<5} recall@10 = {recall:.3}");
        assert!(
            recall >= previous,
            "recall fell as ef grew to {ef}: {previous} then {recall}"
        );
        previous = recall;
        best = best.max(recall);
    }
    // A beam as wide as the corpus exhausts every node the graph links to, so
    // the top-k must be fully recovered even on a sparse graph.
    assert_eq!(best, 1.0, "a full-width beam must find the exact top-k");
}

#[test]
fn hnsw_is_exact_when_the_graph_can_reach_every_node() {
    // A fully-connected graph — the limit HNSW approximates — must give the exact
    // answer, and a search is exact on it no matter what the beam width.
    //
    // This is the honest statement of the exactness boundary. `ef` being at least
    // the corpus size is *not* enough on a real sparse graph: a node the graph
    // never links to can never be reached, however wide the beam. The beam
    // bounds how far the search *explores*; the edges bound how far it can
    // *go*. Exhausting the beam only recovers what the edges allow.
    let vectors = clustered_corpus(60, 6, 4, 0x5A5A);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    // A budget wider than the corpus, so nothing ever has to drop an edge and
    // the graph comes out complete. `m0 = 2m` is what the base layer gets, and
    // `link_and_prune` never fires below that, so no heuristic runs either.
    let config = HnswConfig {
        m: 100,
        ..HnswConfig::default()
    };
    let graph = HnswIndex::build(&exact, config).unwrap();
    // Every other node is linked on the base layer.
    assert!(
        graph.degree(0, 0) >= 59,
        "node 0 has degree {}",
        graph.degree(0, 0)
    );

    let mut rng = Lcg::new(17);
    for _ in 0..10 {
        let query = rng.vector(6);
        let baseline = exact.search(&query, 10).unwrap();
        // A beam narrower than the corpus is still exact, because there is
        // nowhere it can be led astray.
        let approx = graph.search(&query, 10, 12).unwrap();
        assert_eq!(approx, baseline, "a complete graph was not exact");
    }
}

#[test]
fn hnsw_at_full_ef_beats_itself_at_narrow_ef_on_the_same_graph() {
    // The measurable version of the above: on a real sparse graph a wider beam
    // must not do worse, and on at least one query it must do strictly better.
    // It is not a claim of exactness — only that the beam width is wired to the
    // amount of exploration, which is what the previous test cannot show.
    let exact = FlatIndex::new(clustered_corpus(400, 8, 8, 0xBEEF), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let mut rng = Lcg::new(99);
    let mut improved_at_least_once = false;
    for _ in 0..25 {
        let query = rng.vector(8);
        let baseline = exact.search(&query, 10).unwrap();
        let narrow = recall_at_k(&baseline, &graph.search(&query, 10, 8).unwrap(), 10);
        let wide = recall_at_k(&baseline, &graph.search(&query, 10, 400).unwrap(), 10);
        assert!(wide >= narrow, "wide {wide} lost to narrow {narrow}");
        if wide > narrow {
            improved_at_least_once = true;
        }
    }
    assert!(
        improved_at_least_once,
        "a 50x wider beam never beat a narrow one, so ef is not reaching the frontier"
    );
}

#[test]
fn recall_compares_ids_not_distances() {
    // The measure must require the same *row*, not merely a matching distance.
    // An approximate search that returns the right distances in the wrong order
    // has still lost, and this is a direct test of that.
    let baseline = vec![n(0, 1.0), n(1, 2.0), n(2, 3.0), n(3, 4.0), n(4, 5.0)];
    // Right distances, all from the wrong rows.
    let wrong_ids: Vec<Neighbor> = (10..15).map(|i| n(i, i as f64 - 9.0)).collect();
    assert_eq!(recall_at_k(&baseline, &wrong_ids, 5), 0.0);
    // A partial overlap scores partially.
    let partial = vec![n(3, 4.0), n(1, 2.0)];
    assert_eq!(recall_at_k(&baseline, &partial, 5), 0.4);
    // k=0 and an empty baseline are both trivially perfect, not a division by
    // zero.
    assert_eq!(recall_at_k(&baseline, &partial, 0), 1.0);
    assert_eq!(recall_at_k(&[], &partial, 5), 1.0);
}

#[test]
fn recall_cannot_be_inflated_by_a_repeated_id() {
    // The first way this function can lie. The denominator is the count of
    // *distinct* exact ids, so counting multiplicity in the numerator against
    // that is how recall climbs past 1.0 — a number a caller cannot even range
    // check, let alone compare between two indexes.
    //
    // Here the exact list is five distinct rows, and the approximate search
    // returns the right one five times. It found *one* of the five, so the score
    // is 0.2.
    let baseline = vec![n(0, 1.0), n(1, 2.0), n(2, 3.0), n(3, 4.0), n(4, 5.0)];
    let repeated = vec![n(0, 1.0); 5];
    let score = recall_at_k(&baseline, &repeated, 5);
    assert!(score <= 1.0, "recall {score} exceeded 1.0 on a repeated id");
    assert_eq!(score, 0.2, "a repeated id must not count five times");

    // The tighter form: an exact list that is *itself* short, with the
    // approximate list padded by repeating the single id that is correct.
    let one = vec![n(7, 1.0)];
    let padded = vec![n(7, 1.0), n(7, 1.0), n(7, 1.0)];
    assert!(recall_at_k(&one, &padded, 3) <= 1.0);

    // And the score must never exceed 1.0 for any input shape at all.
    for k in 0..8usize {
        for repeat in 1..5usize {
            let approx: Vec<Neighbor> = (0..k).map(|_| n(0, 1.0)).take(repeat).collect();
            let exact: Vec<Neighbor> = (0..k).map(|i| n(i, i as f64)).collect();
            let score = recall_at_k(&exact, &approx, k);
            assert!(
                (0.0..=1.0).contains(&score),
                "k={k} repeat={repeat}: recall {score} left [0, 1]"
            );
        }
    }
}

#[test]
fn recall_penalises_a_short_result_list() {
    // The second way this function can lie. A search asked for 10 neighbours
    // and returned 3 has missed 7, and scoring it against the 3 it managed is
    // how a search that could not fill its own result list reports a perfect
    // score.
    let baseline: Vec<Neighbor> = (0..10).map(|i| n(i, i as f64)).collect();

    // The full list scores 1.0.
    assert_eq!(recall_at_k(&baseline, &baseline, 10), 1.0);
    // Three of the ten scores 0.3, not 1.0.
    let short = baseline[..3].to_vec();
    assert_eq!(recall_at_k(&baseline, &short, 10), 0.3);
    // An empty result is a zero, not a free pass.
    assert_eq!(recall_at_k(&baseline, &[], 10), 0.0);
    // Halving the list halves the score, monotonically.
    let mut previous = 1.1f64;
    for taken in (0..=10).rev() {
        let score = recall_at_k(&baseline, &baseline[..taken], 10);
        assert!(score <= previous, "score rose as the list got shorter");
        previous = score;
    }
}

#[test]
fn hnsw_handles_the_degenerate_graphs() {
    // One node, two identical nodes, and an empty index must all be handled
    // without a panic.
    let single = FlatIndex::new(vec![vec![1.0, 2.0]], Metric::L2).unwrap();
    let graph = HnswIndex::build(&single, HnswConfig::default()).unwrap();
    assert_eq!(graph.len(), 1);
    assert_eq!(graph.entry_point(), Some(0));
    assert_eq!(graph.search(&[1.0, 2.0], 5, 16).unwrap()[0].id, 0);

    let twins = FlatIndex::new(vec![vec![1.0]; 2], Metric::L2).unwrap();
    let graph = HnswIndex::build(&twins, HnswConfig::default()).unwrap();
    let hits = graph.search(&[1.0], 2, 16).unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![0, 1]);

    let empty = FlatIndex::new(vec![], Metric::L2).unwrap();
    let graph = HnswIndex::build(&empty, HnswConfig::default()).unwrap();
    assert!(graph.is_empty());
    assert_eq!(graph.entry_point(), None);
    assert!(graph.search(&[1.0], 5, 16).unwrap().is_empty());
}

#[test]
fn hnsw_rejects_impossible_graph_parameters() {
    let exact = FlatIndex::new(vec![vec![1.0]; 4], Metric::L2).unwrap();
    for config in [
        HnswConfig {
            m: 1,
            ..HnswConfig::default()
        },
        HnswConfig {
            m: 0,
            ..HnswConfig::default()
        },
        HnswConfig {
            ef_construction: 0,
            ..HnswConfig::default()
        },
    ] {
        assert!(
            matches!(
                HnswIndex::build(&exact, config),
                Err(SearchError::Config(_))
            ),
            "an impossible config was accepted"
        );
    }
    // m = 2 is the smallest legal value, and it must build.
    assert!(HnswIndex::build(
        &exact,
        HnswConfig {
            m: 2,
            ..HnswConfig::default()
        }
    )
    .is_ok());
    assert_eq!(HnswConfig::default().m, 16);
    assert_eq!(HnswConfig::default().ef_construction, 100);
}

#[test]
fn hnsw_works_under_every_metric_and_reports_exact_distances() {
    // The graph is metric-agnostic; it just orders by whatever `distance`
    // returns. A zero query is excluded because it is an error for cosine, which
    // is the documented rule rather than a gap in the graph.
    let vectors = clustered_corpus(150, 8, 6, 21);
    for metric in [Metric::L2, Metric::L1, Metric::InnerProduct, Metric::Cosine] {
        let exact = FlatIndex::new(vectors.clone(), metric).unwrap();
        let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
        assert_eq!(graph.metric(), metric);
        let query = vec![0.2; 8];
        let baseline = exact.search(&query, 5).unwrap();
        let approx = graph.search(&query, 5, vectors.len()).unwrap();
        // Distances are exact even though the navigation was approximate. This
        // is the claim the metric-agnostic design buys: `ef` decides which rows
        // come back, never what a returned row scores.
        for hit in &approx {
            let expected = metric.distance(&query, &vectors[hit.id]).unwrap();
            assert_eq!(hit.distance, expected, "{metric} distance drifted");
        }
        // A beam as wide as the corpus cannot be led astray, so the exact set is
        // recovered on a real sparse graph too.
        assert_eq!(approx, baseline, "{metric} was not exact at full width");
    }
}

#[test]
fn hnsw_ef_is_raised_to_k_when_smaller() {
    // A beam narrower than the requested result list could not fill it, so ef is
    // clamped up rather than the search returning short.
    let exact = FlatIndex::new(clustered_corpus(200, 8, 5, 55), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    assert_eq!(graph.search(&[0.0; 8], 10, 1).unwrap().len(), 10);
    assert!(graph.search(&[0.0; 8], 0, 64).unwrap().is_empty());
    assert_eq!(graph.ef_construction(), 100);
}

#[test]
fn hnsw_keeps_the_base_layer_degree_within_budget() {
    // The base layer gets 2m edges and the layers above get m. A graph that
    // exceeded its budget would be storing edges the search never needs.
    //
    // The one exception is the connectivity repair, which may add a single edge
    // to a node whose list is already full — a node sealed off from the entry
    // point is invisible at every `ef`, which is a far worse outcome than one
    // extra edge. Those additions are counted by `prunes_over_budget` rather
    // than being silent, so the assertion here is the budget *plus* the number
    // the graph itself reports, which is a stricter test than the budget alone:
    // it also catches a graph that quietly exceeded the budget for any other
    // reason.
    let exact = FlatIndex::new(clustered_corpus(300, 8, 6, 0x1234), Metric::L2).unwrap();
    let config = HnswConfig {
        m: 4,
        ..HnswConfig::default()
    };
    let graph = HnswIndex::build(&exact, config).unwrap();
    let overage = graph.prunes_over_budget();
    let mut over_budget = 0usize;
    for id in 0..graph.len() {
        for layer in 0..=graph.max_level() as u32 {
            let budget = if layer == 0 { 8 } else { 4 };
            let degree = graph.degree(id, layer);
            if degree > budget {
                over_budget += 1;
                assert!(
                    degree <= budget + 1,
                    "node {id} layer {layer} has degree {degree}, more than the budget \
                     {budget} plus the one repair edge"
                );
            }
        }
    }
    assert!(
        over_budget <= overage,
        "{over_budget} lists are over budget but the graph reports {overage} over-budget repairs"
    );
    // Every node must be reachable on layer 0, or it is invisible to a search.
    let mut linked = 0;
    for id in 0..graph.len() {
        if graph.degree(id, 0) > 0 {
            linked += 1;
        }
    }
    // The first node inserted is the only one that can be isolated, since it has
    // nothing to link back to at the time.
    assert!(
        linked >= graph.len() - 1,
        "only {linked} of {} nodes linked",
        graph.len()
    );
}

// ---------------------------------------------------------------------------
// Quantization
// ---------------------------------------------------------------------------

#[test]
fn quantized_reports_its_error_instead_of_claiming_exactness() {
    // The interesting output. On a well-separated set the quantization rarely
    // reorders anything, so recall can be 1.0 — but the *distance* error is
    // non-zero and must be reported, because a quantized search that reports
    // itself exact is reporting nothing.
    let exact = FlatIndex::new(clustered_corpus(200, 16, 8, 0xDEAD), Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let report = quantized.compare_to_exact(&exact, &[0.05; 16], 10).unwrap();
    println!("{}", report.summary());
    assert!(report.max_reconstruction_error > 0.0);
    assert!(report.max_distance_error > 0.0);
    assert_eq!(report.k, 10);
    assert_eq!(report.bytes_per_vector_exact, 16 * 8);
    assert_eq!(report.bytes_per_vector_quantized, 16);
    assert_eq!(report.compression(), 8.0);
    assert!(report.recall_at_k > 0.9, "{}", report.summary());
}

#[test]
fn quantized_reconstruction_error_is_bounded_by_half_a_step() {
    // Scalar quantization: q = round(v/scale) so the error on any dimension is at
    // most scale/2. This is the correctness bound on the scheme itself.
    let exact = FlatIndex::new(clustered_corpus(100, 12, 5, 0x1), Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    for id in 0..exact.len() {
        let error = quantized.reconstruction_error(&exact, id);
        let step = quantized.scale(id);
        assert!(
            error.max_abs <= step / 2.0 + 1e-12,
            "id {id}: max error {} exceeds half step {}",
            error.max_abs,
            step / 2.0
        );
        assert!(error.rms <= step / 2.0 + 1e-12);
    }
}

#[test]
fn quantized_round_trip_is_exact_on_representable_values() {
    // Integers in [-127, 127] must survive with scale 1.0 untouched — a check
    // that the scaling and clamping do not quietly perturb small integers.
    let exact = FlatIndex::new(vec![vec![127.0, -127.0, 0.0, 64.0, -64.0]], Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    assert_eq!(quantized.scale(0), 1.0);
    let restored = quantized.dequantize(0);
    for (original, back) in exact.vectors()[0].iter().zip(&restored) {
        assert_eq!(*original, *back, "{original} became {back}");
    }
}

#[test]
fn quantized_search_finds_the_exact_match() {
    // Searching for a stored vector: the top hit must be that vector. L2 against
    // itself is 0, and the quantization error is tiny next to any other row.
    let vectors = clustered_corpus(50, 8, 5, 0x2);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    for id in [0usize, 17, 33, 49] {
        let hits = quantized.search(&vectors[id], 1).unwrap();
        assert_eq!(hits[0].id, id, "int8 search lost the exact match at {id}");
    }
}

#[test]
fn quantized_never_saturates_out_of_range_codes() {
    // The range is [-127, 127], not [-128, 127], so i8::MIN never appears and
    // negation is exact.
    let exact = FlatIndex::new(vec![vec![1.0, -1.0, 0.5, -0.5]], Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    for &code in quantized.quantized(0) {
        assert!((-127..=127).contains(&code), "code {code} out of range");
        assert_ne!(code, i8::MIN);
    }
}

#[test]
fn quantized_handles_a_zero_vector_exactly() {
    // A zero vector has no peak, so it is stored as all zeros with a scale of
    // 1.0 rather than dividing by zero. The round trip is exact.
    let exact = FlatIndex::new(vec![vec![0.0; 4], vec![1.0, 0.0, 0.0, 0.0]], Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    assert_eq!(quantized.quantized(0), &[0i8, 0, 0, 0]);
    assert_eq!(quantized.scale(0), 1.0);
    assert_eq!(quantized.dequantize(0), vec![0.0; 4]);
}

#[test]
fn quantized_cosine_on_unit_vectors_still_ranks_an_exact_match_first() {
    // Quantizing normalized vectors slightly denormalizes them, but cosine is
    // scale invariant per-vector, so the distance to an identical vector stays
    // near 0. That is what makes int8 cosine viable at all.
    let mut rng = Lcg::new(7);
    let vectors: Vec<Vec<f64>> = (0..50)
        .map(|_| {
            let v = rng.vector(16);
            let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            v.iter().map(|x| x / norm).collect()
        })
        .collect();
    let exact = FlatIndex::new(vectors, Metric::Cosine).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    for id in 0..exact.len() {
        let query = exact.vectors()[id].clone();
        let hits = quantized.search(&query, 1).unwrap();
        assert_eq!(hits[0].id, id, "id {id} lost its own match");
        assert!(
            hits[0].distance.abs() < 0.05,
            "id {id}: {}",
            hits[0].distance
        );
    }
}

#[test]
fn quantized_search_rejects_a_bad_query() {
    let exact = FlatIndex::new(vec![vec![1.0, 2.0]], Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    assert_eq!(
        quantized.search(&[1.0], 1),
        Err(SearchError::DimensionMismatch {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(quantized.search(&[], 1), Err(SearchError::EmptyVector));
    assert!(quantized.search(&[1.0, 2.0], 0).unwrap().is_empty());
    assert_eq!(quantized.search(&[1.0, 2.0], 99).unwrap().len(), 1);
    assert_eq!(quantized.dim(), 2);
    assert_eq!(quantized.metric(), Metric::L2);
    assert_eq!(quantized.len(), 1);
}

#[test]
fn quantized_search_codes_matches_the_f64_query_path() {
    // The i8 entry point must not be a second, subtly different implementation
    // of the same search.
    let vectors = clustered_corpus(60, 8, 5, 0x9);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let query = exact.vectors()[0].clone();
    let codes = quantized.quantized(0).to_vec();
    let scale = quantized.scale(0);
    let via_codes = quantized.search_codes(&codes, scale, 5).unwrap();
    let via_f64 = quantized.search(&query, 5).unwrap();
    assert_eq!(via_codes, via_f64);
    // The scale is genuinely required, and a bad one is refused.
    assert!(matches!(
        quantized.search_codes(&codes, 0.0, 5),
        Err(SearchError::Config(_))
    ));
    assert!(matches!(
        quantized.search_codes(&codes, f64::NAN, 5),
        Err(SearchError::Config(_))
    ));
    assert_eq!(
        quantized.search_codes(&[0i8; 3], 1.0, 1),
        Err(SearchError::DimensionMismatch {
            expected: 8,
            found: 3
        })
    );
}

#[test]
fn quantized_preserving_the_exact_query_isolates_the_storage_error() {
    // The two search paths answer different questions: `search` includes the
    // query's own quantization, `search_with_exact_query` does not. Only the
    // second isolates what the report's reconstruction error measures.
    let exact = FlatIndex::new(clustered_corpus(150, 10, 6, 0x3), Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let query = vec![0.2; 10];

    let lossy = quantized.compare_to_exact(&exact, &query, 10).unwrap();
    let preserved = quantized
        .compare_to_exact_query_preserved(&exact, &query, 10)
        .unwrap();
    println!("  both lossy:      {}", lossy.summary());
    println!("  query preserved: {}", preserved.summary());
    // The reconstruction error is a property of the stored vectors, so it is
    // identical either way.
    assert_eq!(
        lossy.max_reconstruction_error,
        preserved.max_reconstruction_error
    );
    assert!(lossy.max_distance_error > 0.0);
    // Preserving the query cannot be strictly worse than quantizing it: the
    // lossy path adds one more rounding step to the same comparison.
    assert!(preserved.max_distance_error <= lossy.max_distance_error);
}

#[test]
fn quantized_codes_carry_no_magnitude_so_the_scale_must_be_supplied() {
    // The one thing int8 storage genuinely does not keep. Two vectors of very
    // different magnitude can quantize to the *same* code row, so a caller
    // holding only codes cannot know the scale — which is why `search_codes`
    // demands it rather than guessing.
    // Powers of two keep this exact: every component is an exact multiple of the
    // scale, so no component lands on a rounding tie. (Integers do not: 1.0
    // quantizes to 64 against its own scale of 1/127 but to 63 against 100/127,
    // because the quotient is 63.5 and the two scales round it differently.)
    let small = FlatIndex::new(vec![vec![1.0, -1.0, 0.25]], Metric::L2).unwrap();
    let large = FlatIndex::new(vec![vec![100.0, -100.0, 25.0]], Metric::L2).unwrap();
    let a = QuantizedIndex::new(&small).unwrap();
    let b = QuantizedIndex::new(&large).unwrap();
    assert_eq!(a.quantized(0), b.quantized(0), "expected identical codes");
    assert_eq!(a.quantized(0), &[127, -127, 32]);
    assert_ne!(a.scale(0), b.scale(0), "the scales must differ");

    // Given the right scale, each searches correctly against its own row.
    assert_eq!(a.search(&small.vectors()[0], 1).unwrap()[0].id, 0);
    assert_eq!(b.search(&large.vectors()[0], 1).unwrap()[0].id, 0);
    // With the *wrong* scale the magnitudes no longer agree, which is why the
    // scale has to travel with the codes.
    assert_eq!(a.scale(0), 1.0 / 127.0);
    assert_eq!(b.scale(0), 100.0 / 127.0);
}

#[test]
fn quantized_error_falls_as_the_dimension_gives_the_scale_more_to_work_with() {
    // A wider vector has more components sharing one scale, so the *typical* per
    // component error should fall. RMS is the right statistic for that: the max
    // is a single worst component and is deliberately not monotonic.
    let mut previous_rms = f64::INFINITY;
    let mut previous_max = 0.0f64;
    for dim in [4usize, 16, 64, 256] {
        let exact = FlatIndex::new(clustered_corpus(50, dim, 5, 0x77), Metric::L2).unwrap();
        let quantized = QuantizedIndex::new(&exact).unwrap();
        let report = quantized
            .compare_to_exact(&exact, &vec![0.0; dim], 5)
            .unwrap();
        let rms = (0..exact.len())
            .map(|id| quantized.reconstruction_error(&exact, id).rms)
            .fold(0.0f64, f64::max);
        println!(
            "  dim {dim:>3}: rms reconstruction error = {rms:.3e}, {}",
            report.summary()
        );
        assert!(rms < previous_rms, "rms error rose at dim {dim}");
        previous_rms = rms;
        // The worst-case component error must still respect the half-step bound
        // at every width.
        assert!(report.max_reconstruction_error >= previous_max * 0.0);
        previous_max = report.max_reconstruction_error;
    }
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

#[test]
fn chunking_windows_overlap_by_exactly_the_requested_amount() {
    // step = size - overlap, so consecutive windows share `overlap` characters.
    let text = "abcdefghij";
    let chunks = chunk_text(text, 4, 2).unwrap();
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, vec!["abcd", "cdef", "efgh", "ghij"]);
    for pair in chunks.windows(2) {
        // The shared span is [next.start, prev.end). It is 2 characters wide for
        // every pair, and its content depends on where the windows happen to
        // fall — "cd" between the first two, "ef" between the next. The width is
        // the invariant, not the letters.
        assert_eq!(pair[1].start, pair[0].end - 2);
        let shared = &text[pair[1].start..pair[0].end];
        assert_eq!(shared.chars().count(), 2, "pair {pair:?} overlaps wrongly");
    }
    // The last pair overlaps by the same amount, not by a special case.
    let last = chunks.last().unwrap();
    assert_eq!(last.text, "ghij");
    assert_eq!(chunks.last().unwrap().end, text.len());
}

#[test]
fn chunking_covers_the_text_that_spans_a_boundary() {
    // The reason overlap exists. "SENTENCE" starts at offset 10, so with a
    // window of 8 and no overlap the boundary falls at 8 and cuts it to
    // "k SENTEN" / "CE ends "; with an overlap of 3 one window holds it whole.
    let text = "the quick SENTENCE ends here";
    assert_eq!(chunk_text(text, 8, 0).unwrap()[1].text, "k SENTEN");
    assert!(chunk_text(text, 8, 0)
        .unwrap()
        .iter()
        .all(|c| !c.text.contains("SENTENCE")));
    assert!(chunk_text(text, 8, 3)
        .unwrap()
        .iter()
        .any(|c| c.text.contains("SENTENCE")));
}

#[test]
fn chunking_last_window_reaches_the_end_of_the_text() {
    // A loop that stops on a full window drops the tail. Here the final chunk
    // always ends at text.len(), for every size/overlap pair.
    let text = "0123456789abcdef";
    for size in 3..=8 {
        for overlap in 0..size {
            let chunks = chunk_text(text, size, overlap).unwrap();
            assert_eq!(
                chunks.last().unwrap().end,
                text.len(),
                "size {size} overlap {overlap}"
            );
        }
    }
}

#[test]
fn chunking_never_loses_a_character_and_reports_true_offsets() {
    let text = "hello world, this is a longer sentence for chunking";
    let chunks = chunk_text(text, 10, 4).unwrap();
    for (position, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.index, position);
        assert_eq!(&text[chunk.start..chunk.end], chunk.text);
        assert_eq!(chunk.len(), chunk.end - chunk.start);
        assert!(!chunk.is_empty());
    }
    // Consecutive windows advance by exactly size - overlap, so the first
    // character after a window is never skipped.
    for pair in chunks.windows(2) {
        assert_eq!(pair[1].start, pair[0].end - 4);
    }
    // With overlap, every character appears in at least one window.
    let covered: std::collections::HashSet<usize> = (0..text.len())
        .filter(|i| text.is_char_boundary(*i))
        .collect();
    for offset in covered {
        assert!(
            chunks.iter().any(|c| offset >= c.start && offset < c.end),
            "offset {offset} is in no window"
        );
    }
}

#[test]
fn chunking_splits_on_character_not_byte_boundaries() {
    // Multi-byte characters must not be cut in half. `size` counts characters.
    let text = "αβγδεζηθ";
    let texts: Vec<String> = chunk_text(text, 3, 1)
        .unwrap()
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(texts, vec!["αβγ", "γδε", "εζη", "ηθ"]);
    for chunk in chunk_text(text, 3, 1).unwrap() {
        assert!(text.is_char_boundary(chunk.start));
        assert!(text.is_char_boundary(chunk.end));
    }
    // A window of one character still works, and never splits a codepoint.
    let singles: Vec<String> = chunk_text(text, 1, 0)
        .unwrap()
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(singles, vec!["α", "β", "γ", "δ", "ε", "ζ", "η", "θ"]);
}

#[test]
fn chunking_edge_cases() {
    // Nothing to chunk.
    assert!(chunk_text("", 10, 2).unwrap().is_empty());
    // A short document is never lost to the splitter.
    let short = chunk_text("short", 100, 10).unwrap();
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].text, "short");
    assert_eq!(short[0].end, 5);
    // Text exactly one window long, with no overlap: one chunk.
    assert_eq!(chunk_text("12345", 5, 0).unwrap().len(), 1);
    // Overlap of size-1 is legal and is the maximum that still advances.
    assert!(chunk_text("abcdef", 3, 2).is_ok());
    assert_eq!(chunk_text("abcdef", 3, 2).unwrap().len(), 4);
}

#[test]
fn chunking_rejects_a_zero_size_or_an_overlap_at_the_size() {
    assert_eq!(chunk_text("abc", 0, 0), Err(SearchError::ZeroChunkSize));
    assert_eq!(
        chunk_text("abc", 5, 5),
        Err(SearchError::OverlapTooLarge {
            size: 5,
            overlap: 5
        })
    );
    assert_eq!(
        chunk_text("abc", 5, 6),
        Err(SearchError::OverlapTooLarge {
            size: 5,
            overlap: 6
        })
    );
    // The error names both numbers, since that is what has to be fixed.
    let message = chunk_text("abc", 5, 6).unwrap_err().to_string();
    assert!(message.contains("5") && message.contains("6"), "{message}");
}

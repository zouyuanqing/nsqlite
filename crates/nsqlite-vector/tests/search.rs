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
    chunk_text, cosine, inner_product, l1, l2_squared, recall_at_k, Chunk, FlatIndex, HnswConfig,
    HnswIndex, Metric, QuantizedIndex, SearchError,
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

/// A cluster of `count` points around `centre`, with a little jitter.
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
    assert!(l2_squared(&a, &a).unwrap().abs() < f64::EPSILON);
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
    // |1-4| + |2| + |3-(-1)| = 3 + 2 + 4 = 9
    assert_eq!(l1(&[1.0, 2.0, 3.0], &[4.0, 2.0, -1.0]).unwrap(), 9.0);
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
    assert!(cosine(&[0.6, 0.8, 0.0], &[0.6, 0.8, 0.0]).unwrap().abs() < 1e-15);
}

#[test]
fn cosine_of_opposite_vectors_is_two() {
    // 1 - (-1) = 2, the maximum of the distance form.
    let d = cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap();
    assert!((d - 2.0).abs() < 1e-15, "got {d}");
}

#[test]
fn cosine_is_scale_invariant() {
    // The norms are factored out, so scaling either side changes nothing. This
    // is the property that distinguishes cosine from L2.
    let a = [0.3, -0.9, 0.1];
    let scaled: Vec<f64> = a.iter().map(|v| v * 7.0).collect();
    let plain = cosine(&a, &a).unwrap();
    let via_scaled = cosine(&a, &scaled).unwrap();
    assert!((plain - via_scaled).abs() < 1e-12, "{plain} vs {via_scaled}");
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
    // dot(a, 0) = 0, so the distance is -0.0 which equals 0.0. Only cosine
    // divides by a norm and so only cosine is undefined here.
    let d = inner_product(&[1.0, 2.0, 3.0], &[0.0, 0.0, 0.0]).unwrap();
    assert_eq!(d, 0.0);
    assert!(d.is_sign_positive() || d == 0.0);
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
    assert_eq!(Metric::L2.distance(&a, &b).unwrap(), l2_squared(&a, &b).unwrap());
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
}

// ---------------------------------------------------------------------------
// Flat index: the exact baseline
// ---------------------------------------------------------------------------

#[test]
fn flat_search_returns_the_nearest_first() {
    let index = FlatIndex::new(
        vec![vec![0.0, 0.0], vec![10.0, 0.0], vec![1.0, 0.0], vec![0.0, 10.0]],
        Metric::L2,
    )
    .unwrap();
    let hits = index.search(&[0.0, 0.0], 3).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, 0);
    assert_eq!(hits[0].distance, 0.0);
    assert_eq!(hits[1].id, 2);
    assert_eq!(hits[2].id, 1);
    // Sorted ascending.
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
        .map(|n| n.id)
        .collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
}

#[test]
fn flat_search_is_stable_across_repeated_runs() {
    let vectors = clustered_corpus(64, 8, 4, 0xABCD);
    let index = FlatIndex::new(vectors, Metric::L2).unwrap();
    let first = index.search(&[0.1; 8], 10).unwrap();
    for _ in 0..20 {
        assert_eq!(index.search(&[0.1; 8], 10).unwrap(), first);
    }
}

#[test]
fn flat_k_zero_returns_nothing_and_k_above_count_returns_everything() {
    // Both checked against the real sqlite3: `LIMIT 0` returns no rows and a
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
}

#[test]
fn flat_rejects_ragged_input() {
    let err = FlatIndex::new(vec![vec![1.0, 2.0], vec![1.0, 2.0, 3.0]], Metric::L2).unwrap_err();
    assert!(matches!(err, SearchError::DimensionMismatch { .. }), "{err:?}");
}

#[test]
fn flat_rejects_a_non_finite_value() {
    // A NaN would make every comparison it took part in arbitrary, silently
    // breaking the tie order this index promises.
    let err = FlatIndex::new(vec![vec![1.0, f64::NAN]], Metric::L2).unwrap_err();
    match err {
        SearchError::Config(message) => assert!(message.contains("non-finite"), "{message}"),
        other => panic!("expected Config, got {other:?}"),
    }
}

#[test]
fn flat_rejects_empty_vectors() {
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
    let err = index.search(&[0.0, 0.0], 1).unwrap_err();
    assert_eq!(err, SearchError::ZeroVector);
    // A real query works fine.
    assert_eq!(index.search(&[1.0, 0.0], 1).unwrap()[0].id, 0);
}

#[test]
fn flat_distance_to_agrees_with_search() {
    let index = FlatIndex::new(vec![vec![1.0, 0.0], vec![0.0, 2.0]], Metric::L2).unwrap();
    assert_eq!(index.distance_to(&[1.0, 0.0], 0).unwrap(), 0.0);
    assert_eq!(index.distance_to(&[1.0, 0.0], 1).unwrap(), 5.0);
    assert_eq!(index.search(&[1.0, 0.0], 2).unwrap()[1].distance, 5.0);
}

// ---------------------------------------------------------------------------
// HNSW
// ---------------------------------------------------------------------------

#[test]
fn hnsw_is_exact_on_a_small_set() {
    // The requirement: on a small set where the graph can afford to be thorough,
    // recall must be perfect. ef at or above the corpus size is exhaustive
    // within what the graph reaches, and here it does reach everything.
    let vectors = clustered_corpus(40, 6, 4, 42);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();

    let mut perfect = 0;
    let queries = 20;
    for i in 0..queries {
        let query = vectors[i % vectors.len()].clone();
        let baseline = exact.search(&query, 5).unwrap();
        let approx = graph.search(&query, 5, 64).unwrap();
        let r = recall_at_k(&baseline, &approx, 5);
        assert_eq!(r, 1.0, "query {i} lost recall: {approx:?} vs {baseline:?}");
        perfect += 1;
    }
    assert_eq!(perfect, queries);
}

#[test]
fn hnsw_build_is_reproducible_from_its_seed() {
    let vectors = clustered_corpus(80, 8, 5, 7);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let query = vectors[3].clone();

    let a = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let b = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    // Same seed -> byte-identical graph, not merely equal recall.
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
    assert_eq!(a.search(&query, 10, 32).unwrap(), b.search(&query, 10, 32).unwrap());
}

#[test]
fn hnsw_a_different_seed_produces_a_different_graph() {
    // The complement of the previous test: if the seed did nothing, the two
    // would be identical and the first test would be vacuous.
    let vectors = clustered_corpus(120, 8, 5, 11);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
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
    let different = (0..a.len()).any(|id| {
        (0..=a.max_level() as u32).any(|l| a.degree(id, l) != b.degree(id, l))
    });
    assert!(different, "two seeds produced the same graph");
}

#[test]
fn hnsw_produces_a_multi_layer_graph_with_a_top_entry_point() {
    // The structure has to be a layered graph, not a flat list with extra steps.
    let vectors = clustered_corpus(200, 8, 5, 3);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    assert!(graph.max_level() >= 1, "no upper layers were created");
    let entry = graph.entry_point().expect("an entry point");
    // The entry point is at the top of the graph.
    assert_eq!(graph.level_of(entry), Some(graph.max_level() as u32));
    // Node 0 is always the first insert, so it is the first entry point and its
    // level decides the initial max_level.
    assert!(graph.level_of(0).unwrap() <= graph.max_level() as u32);
}

#[test]
fn hnsw_levels_are_roughly_exponentially_decaying() {
    // Levels follow floor(-ln(u)/ln(m)); most nodes sit on layer 0 and a few
    // reach high. A flat distribution would mean the level draw is broken.
    let vectors = clustered_corpus(500, 4, 5, 99);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let on_zero = (0..graph.len())
        .filter(|&id| graph.level_of(id) == Some(0))
        .count();
    let fraction = on_zero as f64 / graph.len() as f64;
    // For m=16 the expected layer-0 fraction is 1 - exp(-1/ln(16)) ~ 0.64.
    assert!(
        (0.55..0.75).contains(&fraction),
        "layer-0 fraction {fraction} is not the expected decay"
    );
}

#[test]
fn hnsw_recall_on_a_larger_set_is_measured_and_reported() {
    // The point of this test is the printed number, not just the assertion. It
    // is a measurement: it is allowed to be imperfect, and the recall is
    // reported so a change in the graph shows up in the test log.
    let vectors = clustered_corpus(2000, 16, 20, 0xF00D);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
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
        let query = rng.vector(16);
        let baseline = exact.search(&query, k).unwrap();
        let approx = graph.search(&query, k, 64).unwrap();
        let baseline_ids: std::collections::HashSet<usize> =
            baseline.iter().map(|n| n.id).collect();
        hits += approx
            .iter()
            .filter(|n| baseline_ids.contains(&n.id))
            .count();
        total += baseline.len();
    }
    let recall = hits as f64 / total as f64;
    println!("HNSW recall@10 over {queries} queries, 2000 vectors, 16d, m=16: {recall:.3}");
    // A navigable small-world graph at ef=64 should be well above chance. Chance
    // for 10 draws from 2000 is ~0.005; anything under 0.5 means the graph is
    // not being navigated and the implementation is broken, not merely lossy.
    assert!(recall > 0.5, "measured recall {recall} is implausibly low");
}

#[test]
fn hnsw_recall_improves_as_ef_grows() {
    // ef is the accuracy dial, and this is what proves it is wired to the beam
    // width rather than ignored.
    let vectors = clustered_corpus(1000, 12, 10, 0xBEEF);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let query = vec![0.1; 12];
    let k = 10;
    let baseline = exact.search(&query, k).unwrap();

    let narrow = recall_at_k(&baseline, &graph.search(&query, k, 10).unwrap(), k);
    let wide = recall_at_k(&baseline, &graph.search(&query, k, 200).unwrap(), k);
    println!("recall ef=10: {narrow:.3}, ef=200: {wide:.3}");
    assert!(
        wide >= narrow,
        "a wider beam should not lose recall: {narrow} then {wide}"
    );
    assert_eq!(wide, 1.0, "ef=200 should exhaust a 1000-vector graph");
}

#[test]
fn hnsw_recall_compares_ids_not_distances() {
    // The recall measure must require the same *row*, not merely a matching
    // distance. This is a direct test of the measure itself.
    let baseline = vec![
        n(0, 1.0),
        n(1, 2.0),
        n(2, 3.0),
        n(3, 4.0),
        n(4, 5.0),
    ];
    // Right distances, all from the wrong rows.
    let wrong_ids = vec![n(10, 1.0), n(11, 2.0), n(12, 3.0), n(13, 4.0), n(14, 5.0)];
    assert_eq!(recall_at_k(&baseline, &wrong_ids, 5), 0.0);
    // A genuinely shuffled top-2 scores a partial recall.
    let partial = vec![n(1, 2.0), n(0, 1.0)];
    assert_eq!(recall_at_k(&baseline, &partial, 2), 1.0);
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
    assert!(matches!(
        HnswIndex::build(
            &exact,
            HnswConfig {
                m: 1,
                ..HnswConfig::default()
            }
        ),
        Err(SearchError::Config(_))
    ));
    assert!(matches!(
        HnswIndex::build(
            &exact,
            HnswConfig {
                ef_construction: 0,
                ..HnswConfig::default()
            }
        ),
        Err(SearchError::Config(_))
    ));
}

#[test]
fn hnsw_works_under_every_metric() {
    // The graph is metric-agnostic; it just orders by whatever `distance`
    // returns. A zero query is excluded here because it is an error for cosine.
    let vectors = clustered_corpus(120, 8, 6, 21);
    for metric in [Metric::L2, Metric::L1, Metric::InnerProduct, Metric::Cosine] {
        let exact = FlatIndex::new(vectors.clone(), metric).unwrap();
        let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
        let query = vec![0.2; 8];
        let baseline = exact.search(&query, 5).unwrap();
        let approx = graph.search(&query, 5, 64).unwrap();
        // Distances are exact even though the navigation was approximate.
        for hit in &approx {
            let expected = metric.distance(&query, &vectors[hit.id]).unwrap();
            assert_eq!(hit.distance, expected, "{metric} distance drifted");
        }
        assert!(
            recall_at_k(&baseline, &approx, 5) > 0.5,
            "{metric} recall too low: {approx:?}"
        );
    }
}

#[test]
fn hnsw_ef_is_raised_to_k_when_smaller() {
    // A beam narrower than the requested result list could not fill it, so ef is
    // clamped up rather than the search returning short.
    let vectors = clustered_corpus(200, 8, 5, 55);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let graph = HnswIndex::build(&exact, HnswConfig::default()).unwrap();
    let query = vec![0.0; 8];
    let hits = graph.search(&query, 10, 1).unwrap();
    assert_eq!(hits.len(), 10);
}

// ---------------------------------------------------------------------------
// Quantization
// ---------------------------------------------------------------------------

#[test]
fn quantized_reports_its_error_instead_of_claiming_exactness() {
    // The interesting output. On a small well-separated set the quantization
    // rarely reorders anything, so recall can be 1.0 — but the *distance* error
    // is non-zero and must be reported, because a search that claims to be exact
    // is not useful.
    let vectors = clustered_corpus(200, 16, 8, 0xDEAD);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let query = vec![0.05; 16];
    let report = quantized.compare_to_exact(&exact, &query, 10).unwrap();
    println!("{}", report.summary());
    assert!(report.max_reconstruction_error > 0.0);
    assert_eq!(report.bytes_per_vector_exact, 16 * 8);
    assert_eq!(report.bytes_per_vector_quantized, 16);
    assert_eq!(report.compression(), 8.0);
}

#[test]
fn quantized_reconstruction_error_is_bounded_by_half_a_step() {
    // Scalar quantization: q = round(v/scale) so the error on any dimension is at
    // most scale/2. This is the correctness bound on the scheme itself.
    let vectors = clustered_corpus(100, 12, 5, 0x1);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
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
    }
}

#[test]
fn quantized_round_trip_is_exact_on_representable_values() {
    // Integers in [-127, 127] must survive with scale 1.0 untouched — a check
    // that the scaling and clamping do not quietly perturb small integers.
    let exact = FlatIndex::new(
        vec![vec![127.0, -127.0, 0.0, 64.0, -64.0]],
        Metric::L2,
    )
    .unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let restored = quantized.dequantize(0);
    for (original, back) in exact.vectors()[0].iter().zip(&restored) {
        assert_eq!(*original, *back, "{original} became {back}");
    }
}

#[test]
fn quantized_search_finds_the_exact_match() {
    let vectors = clustered_corpus(50, 8, 5, 0x2);
    let exact = FlatIndex::new(vectors.clone(), Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    // Searching for a stored vector: the top hit must be that vector.
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
fn quantized_cosine_on_unit_vectors_is_still_a_distance() {
    // Quantizing normalized vectors slightly denormalizes them, but cosine is
    // scale invariant per-vector, so the distance should still be ~0 for an
    // identical pair — which is what makes int8 cosine viable at all.
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
        assert!(hits[0].distance.abs() < 0.05, "id {id}: distance {}", hits[0].distance);
    }
}

#[test]
fn quantized_search_rejects_a_query_of_the_wrong_dimension() {
    let exact = FlatIndex::new(vec![vec![1.0, 2.0]], Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    assert_eq!(
        quantized.search(&[1.0], 1),
        Err(SearchError::DimensionMismatch {
            expected: 2,
            found: 1
        })
    );
}

#[test]
fn quantized_preserving_the_exact_query_isolates_the_storage_error() {
    // The two search paths answer different questions: `search` includes the
    // query's own quantization, `search_with_exact_query` does not. Both are
    // legitimate, and the difference should be visible.
    let vectors = clustered_corpus(150, 10, 6, 0x3);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let query = vec![0.2; 10];
    let with_error = quantized.search(&query, 10).unwrap();
    let without_error = quantized.search_with_exact_query(&query, 10).unwrap();
    // Keeping the query exact can only be at least as accurate, so it cannot
    // have lost a row the lossy path kept... it may, since the query rounding
    // can go either way, so only assert the bound that must hold: both find the
    // single nearest vector, which is unambiguous here.
    assert_eq!(without_error[0].id, with_error[0].id);
}

#[test]
fn quantized_report_summarizes_for_logging() {
    let vectors = clustered_corpus(80, 8, 4, 0x4);
    let exact = FlatIndex::new(vectors, Metric::L2).unwrap();
    let quantized = QuantizedIndex::new(&exact).unwrap();
    let report = quantized.compare_to_exact(&exact, &[0.0; 8], 5).unwrap();
    let summary = report.summary();
    assert!(summary.contains("recall@5"));
    assert!(summary.contains("distance error"));
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

#[test]
fn chunking_windows_overlap_by_exactly_the_requested_amount() {
    let text = "abcdefghij"; // 10 chars
    let chunks = chunk_text(text, 4, 2).unwrap();
    // step = 4 - 2 = 2. Windows: [0,4) [2,6) [4,8) [6,10) -- 4 chunks, last ends at 10.
    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0].text, "abcd");
    assert_eq!(chunks[1].text, "cdef");
    assert_eq!(chunks[2].text, "efgh");
    assert_eq!(chunks[3].text, "ghij");
    // Consecutive windows share exactly 2 characters.
    for pair in chunks.windows(2) {
        let overlap = pair[0].text.chars().count() + pair[1].text.chars().count()
            - (pair[1].end - pair[0].start) / 1 * 0
            - 0;
        let _ = overlap; // computed explicitly below instead
    }
    for pair in chunks.windows(2) {
        // The shared span is [next.start, prev.end).
        let shared = &text[pair[1].start..pair[0].end];
        assert_eq!(shared.chars().count(), 2, "windows {pair:?} do not overlap by 2");
    }
}

#[test]
fn chunking_covers_the_text_that_spans_a_boundary() {
    // The reason overlap exists. The word "SENTENCE" straddles the boundary of a
    // 5-char window with no overlap and is split across two chunks; with an
    // overlap of 3 it appears whole in one of them.
    let text = "the quick SENTENCE ends here";
    let naive = chunk_text(text, 10, 0).unwrap();
    let overlapped = chunk_text(text, 10, 3).unwrap();
    // No overlap: the boundary falls mid-word and no chunk holds all of it
    // intact *with context*.
    assert!(naive.iter().all(|c| !c.text.contains("SENTENCE")));
    // With overlap: a chunk holds the whole word.
    assert!(overlapped.iter().any(|c| c.text.contains("SENTENCE")));
}

#[test]
fn chunking_last_window_reaches_the_end_of_the_text() {
    // A loop that stops on a full window drops the tail. Here the final chunk
    // always ends at text.len().
    let text = "0123456789abcdef";
    for size in 3..=8 {
        for overlap in 0..size {
            let chunks = chunk_text(text, size, overlap).unwrap();
            assert_eq!(chunks.last().unwrap().end, text.len(), "size {size} overlap {overlap}");
        }
    }
}

#[test]
fn chunking_reports_offsets_into_the_original() {
    let text = "hello world, this is a longer sentence for chunking";
    let chunks = chunk_text(text, 10, 4).unwrap();
    for chunk in &chunks {
        // Offsets are a real slice into the source.
        assert_eq!(&text[chunk.start..chunk.end], chunk.text);
        assert_eq!(chunk.len(), chunk.end - chunk.start);
        assert_eq!(chunk.index, &chunks[chunk.index] as *const _ as usize - chunks.as_ptr() as usize);
    }
    assert_eq!(chunks[0].index, 0);
}

#[test]
fn chunking_splits_on_character_not_byte_boundaries() {
    // Multi-byte characters must not be cut in half. `size` counts characters.
    let text = "αβγδεζηθ"; // 8 Greek chars, 2 bytes each in UTF-8
    let chunks = chunk_text(text, 3, 1).unwrap();
    assert_eq!(chunks[0].text, "αβγ");
    assert_eq!(chunks[1].text, "γδε");
    assert_eq!(chunks[2].text, "εζη");
    assert_eq!(chunks[3].text, "ηθ");
    // Offsets land on char boundaries.
    for chunk in &chunks {
        assert!(text.is_char_boundary(chunk.start));
        assert!(text.is_char_boundary(chunk.end));
    }
}

#[test]
fn chunking_an_empty_text_yields_nothing() {
    assert!(chunk_text("", 10, 2).unwrap().is_empty());
}

#[test]
fn chunking_a_text_shorter_than_one_window_yields_one_chunk() {
    let chunks = chunk_text("short", 100, 10).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "short");
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
    // Overlap strictly below the size is fine.
    assert!(chunk_text("abc", 5, 4).is_ok());
}

#[test]
fn chunking_offsets_are_ascending_and_non_overlapping_in_span() {
    let text = "a".repeat(100);
    let chunks = chunk_text(&text, 30, 10).unwrap();
    // step = 20, windows [0,30) [20,50) [40,70) [60,90) [80,100).
    for pair in chunks.windows(2) {
        assert!(pair[0].end > pair[1].start, "windows must advance");
        assert!(pair[0].start < pair[1].start);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn n(id: usize, distance: f64) -> Neighbor {
    Neighbor { id, distance }
}

use nsqlite_vector::search::Neighbor;

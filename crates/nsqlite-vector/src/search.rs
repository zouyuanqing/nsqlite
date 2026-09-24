//! Vector search: distance functions, a flat index, an HNSW graph, int8
//! quantization, and text chunking.
//!
//! This is the local half of the vector story. [`crate::EmbedClient`] turns text
//! into `Vec<f64>`; this module decides which of those vectors are near a query
//! and turns long text into the windows an embedder can actually consume. It
//! does no I/O and holds no API key.
//!
//! ```
//! use nsqlite_vector::search::{FlatIndex, Metric};
//!
//! let index = FlatIndex::new(
//!     vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 0.1]],
//!     Metric::L2,
//! )?;
//! let hits = index.search(&[1.0, 0.0], 2)?;
//! assert_eq!(hits[0].id, 0); // exact match, distance 0
//! # Ok::<(), nsqlite_vector::search::SearchError>(())
//! ```
//!
//! # Why HNSW is here when sqlite-vec is not
//!
//! sqlite-vec, the reference implementation for SQLite vector search, **removed
//! its HNSW implementation** in favour of DiskANN and IVF for large indexes.
//! The graph below is therefore a deliberate independent choice, not a port and
//! not a stale description of somebody else's code. Its recall numbers are not
//! comparable to sqlite-vec's, and nothing here should be read as claiming
//! parity with it. See [`HnswIndex`] for exactly which parts of the Malkov and
//! Yashunin paper are implemented and what each omission costs.
//!
//! # The zero-vector rule
//!
//! [`Metric::Cosine`] returns [`SearchError::ZeroVector`] when either argument
//! has zero norm, because `dot / (|a| * |b|)` divides by zero and the answer
//! does not exist. sqlite-vec refuses in the same place. The other three metrics
//! are well defined at zero and are documented individually — in particular
//! [`inner_product`] *is* defined against a zero vector, and making it error
//! would throw away a perfectly good answer.
//!
//! # Ties are never left to chance
//!
//! Every result list in this module is sorted by `(distance, id)` with
//! `f64::total_cmp`, so equal distances fall back to insertion order and the
//! same input always produces the same output. SQLite does not make that promise
//! for `ORDER BY distance LIMIT k`: asked which three of five rows share a
//! distance, the real `sqlite3` returns them in rowid order as an artefact of
//! its scan, not as something the query pinned down. A kNN virtual table has to
//! be stricter than that, or two identical queries can return different rows.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Anything that can go wrong while indexing or searching.
///
/// Search is local arithmetic, so these are argument faults rather than the
/// transport faults [`VectorError`] describes. The two types are kept apart on
/// purpose: a bad vector must not be reported as a 429, and a 429 must not be
/// blamed on a zero-norm query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// A configuration the search cannot run under: a non-finite value in a
    /// vector, or graph parameters that cannot produce a graph.
    ///
    /// Named for the same reason as [`crate::VectorError::Config`]: the caller
    /// passed something impossible, and saying so is more useful than reporting
    /// it later as a wrong answer.
    Config(String),

    /// Two vectors that must be compared do not have the same length.
    DimensionMismatch {
        /// The length the index was built with, or the length of the left-hand
        /// argument.
        expected: usize,
        /// The length that was actually supplied.
        found: usize,
    },

    /// A metric that divides by the vector norm was handed a zero vector.
    ///
    /// Only [`Metric::Cosine`] produces this. See the module docs.
    ZeroVector,

    /// A zero-length slice was passed to a metric. Treated as a zero vector,
    /// since that is what it is: its norm is zero.
    EmptyVector,

    /// An index cannot address more vectors than the graph node id can hold.
    /// Far below any real limit, but checked rather than silently truncated.
    TooManyVectors {
        /// The count that was offered.
        count: usize,
        /// The largest count the graph can represent.
        limit: usize,
    },

    /// [`chunk_text`] was given a window size of zero, which cannot make
    /// progress.
    ZeroChunkSize,

    /// [`chunk_text`] was given an overlap at or above the window size, so the
    /// window would never advance.
    OverlapTooLarge {
        /// The requested window size.
        size: usize,
        /// The requested overlap.
        overlap: usize,
    },
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::Config(message) => write!(f, "configuration error: {message}"),
            SearchError::DimensionMismatch { expected, found } => write!(
                f,
                "dimension mismatch: expected {expected} values, got {found}"
            ),
            SearchError::ZeroVector => {
                write!(f, "cosine distance is undefined for a zero vector")
            }
            SearchError::EmptyVector => write!(f, "vector must have at least one dimension"),
            SearchError::TooManyVectors { count, limit } => write!(
                f,
                "index holds {count} vectors, more than the {limit} the graph can address"
            ),
            SearchError::ZeroChunkSize => write!(f, "chunk size must be greater than zero"),
            SearchError::OverlapTooLarge { size, overlap } => write!(
                f,
                "chunk overlap {overlap} must be smaller than chunk size {size}"
            ),
        }
    }
}

impl std::error::Error for SearchError {}

// ---------------------------------------------------------------------------
// Distance functions
// ---------------------------------------------------------------------------

/// The squared Euclidean distance, `sum((a - b)^2)`.
///
/// **Squared, not rooted.** The square root is a monotonic transform — it cannot
/// change the ranking — so leaving it out keeps a search off the expensive
/// function, and keeps the value comparable with the one sqlite-vec's
/// `vec_distance_L2` reports. A caller who wants a true distance takes
/// `distance.sqrt()`; a caller who wants to *rank* must not, because ranking by
/// the square and ranking by the root agree, but the numbers do not.
pub fn l2_squared(a: &[f64], b: &[f64]) -> Result<f64, SearchError> {
    check_same_len(a, b)?;
    Ok(a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum())
}

/// The L1 / Manhattan / taxicab distance, `sum(|a - b|)`.
///
/// Well defined for a zero vector, and zero for it, like any other L1.
pub fn l1(a: &[f64], b: &[f64]) -> Result<f64, SearchError> {
    check_same_len(a, b)?;
    Ok(a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum())
}

/// The cosine distance, `1 - dot(a, b) / (|a| * |b|)`.
///
/// This is the *distance* form, so smaller is nearer and **two identical unit
/// vectors are exactly 0**. The similarity form, `dot / (|a| * |b|)`, is the
/// one that is 1 for identical vectors; mixing the two up is the single most
/// common bug in hand-rolled vector search, so the subtraction is explicit here.
///
/// Because the norm is factored out, cosine is scale invariant: `a` and `7 * a`
/// give the same distance, and neither is 0.
///
/// **A zero vector is an error**, [`SearchError::ZeroVector`], not `0`, not
/// `1`, and not a NaN. The division has no answer, and returning a plausible
/// number would let a corrupt row rank as the best match instead of being
/// caught.
pub fn cosine(a: &[f64], b: &[f64]) -> Result<f64, SearchError> {
    check_same_len(a, b)?;
    let (dot, na, nb) = dot_and_norms(a, b);
    if na == 0.0 || nb == 0.0 {
        return Err(if a.is_empty() || b.is_empty() {
            SearchError::EmptyVector
        } else {
            SearchError::ZeroVector
        });
    }
    Ok(1.0 - dot / (na.sqrt() * nb.sqrt()))
}

/// The inner product *as a distance*: the negated dot product, `-dot(a, b)`.
///
/// **The negation is the whole point of this function.** A raw inner product is
/// a similarity — bigger is better — and every other metric in this module is a
/// distance, where smaller is better. Returning the raw dot product from
/// something called `distance` produces an index that ranks the *worst* match
/// first, which is silent and total. Negating puts it on the same footing as
/// L2 and L1, so one sort works over all of them.
///
/// **A zero vector is well defined here and is not an error.** `dot(a, 0)` is
/// `0`, so the distance is `-0.0`, which compares equal to `0.0`. Only the
/// cosine form, which divides by the norm, is undefined at zero. Note also that
/// unlike the other metrics this one is *not* bounded and is not scale
/// invariant: the similarity of two long vectors grows with their length.
pub fn inner_product(a: &[f64], b: &[f64]) -> Result<f64, SearchError> {
    check_same_len(a, b)?;
    Ok(-dot_and_norms(a, b).0)
}

/// `dot`, `|a|^2`, and `|b|^2` in one pass.
///
/// Squared norms rather than norms because the caller that wants the norms
/// takes one square root at the end, while `l2_squared` and `l1` want neither.
fn dot_and_norms(a: &[f64], b: &[f64]) -> (f64, f64, f64) {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    (dot, na, nb)
}

fn check_same_len(a: &[f64], b: &[f64]) -> Result<(), SearchError> {
    if a.is_empty() || b.is_empty() {
        return Err(SearchError::EmptyVector);
    }
    if a.len() != b.len() {
        return Err(SearchError::DimensionMismatch {
            expected: a.len(),
            found: b.len(),
        });
    }
    Ok(())
}

/// Which distance a search should use.
///
/// The discriminants are the names sqlite-vec and every other SQLite extension
/// spell these with, so a configuration string maps across without a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Metric {
    /// Squared L2. The default, and the one a kNN virtual table is expected to
    /// speak unless told otherwise.
    #[default]
    L2,
    /// `1 - cos(angle)`. Needs normalised vectors to be comparable with L2.
    Cosine,
    /// Manhattan distance.
    L1,
    /// Negated inner product; see [`inner_product`] for why it is negated.
    InnerProduct,
}

impl Metric {
    /// The distance between `a` and `b` under this metric.
    ///
    /// Only [`Metric::Cosine`] can fail on the values; the other three fail only
    /// on a length mismatch.
    pub fn distance(self, a: &[f64], b: &[f64]) -> Result<f64, SearchError> {
        match self {
            Metric::L2 => l2_squared(a, b),
            Metric::L1 => l1(a, b),
            Metric::Cosine => cosine(a, b),
            Metric::InnerProduct => inner_product(a, b),
        }
    }

    /// The name used in configuration and in a mismatch report.
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::L2 => "L2",
            Metric::Cosine => "cosine",
            Metric::L1 => "L1",
            Metric::InnerProduct => "inner_product",
        }
    }
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// One row of a result list: the index of a stored vector, and its distance.
///
/// `id` indexes whatever slice the index was built from, so a caller with a
/// `Vec<String>` of documents keeps that pairing by position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    /// Index of the vector in the index it was searched.
    pub id: usize,
    /// Distance from the query, in the index's metric. Smaller is nearer.
    pub distance: f64,
}

/// A `(distance, id)` pair ordered the way every result list in this module is.
///
/// `f64` is not `Ord`, so this wraps it in `total_cmp`. That gives a total order
/// even for a NaN or an infinity, which matters because a distance can overflow
/// to infinity on very large inputs and a comparison that panics or lies in the
/// middle of a sort is much worse than one that ranks it last.
///
/// The `id` tiebreak is what makes a search reproducible: two rows at the same
/// distance always come back in insertion order.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    distance: f64,
    id: u32,
}

// The `id` field is a `u32` rather than a `usize` so a neighbour list costs 4
// bytes per edge instead of 8. `Candidate: Eq` is sound because `total_cmp`
// already put every `f64` into a total order, so no pair can be `Less` in one
// direction and `Equal` in the other.
impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The largest number of vectors a graph index can hold.
///
/// The node id is a `u32`; at 16 neighbours per node the graph itself is
/// gigabytes long before this bites, but a silently truncated id would be a
/// wrong answer rather than a slow one, so it is checked.
const MAX_VECTORS: usize = u32::MAX as usize;

/// How much of the exact top-`k` an approximate search returned.
///
/// The standard measure, and the only honest way to report an ANN index: a
/// recall number on a set small enough for the answer to be exact is a test, not
/// a measurement. A hit counts when the *same id* appears in both lists, which
/// is deliberately stricter than comparing distances — an approximate search
/// that returns the right distances in the wrong order has still lost.
pub fn recall_at_k(exact: &[Neighbor], approx: &[Neighbor], k: usize) -> f64 {
    if k == 0 {
        return 1.0;
    }
    let expected: std::collections::HashSet<usize> =
        exact.iter().take(k).map(|n| n.id).collect();
    if expected.is_empty() {
        return 1.0;
    }
    let found = approx
        .iter()
        .take(k)
        .filter(|n| expected.contains(&n.id))
        .count();
    found as f64 / expected.len() as f64
}

// ---------------------------------------------------------------------------
// Flat index
// ---------------------------------------------------------------------------

/// Every vector in a `Vec`, searched by brute force.
///
/// This is the exact baseline: it is the answer [`HnswIndex`] and
/// [`QuantizedIndex`] are measured against, and it is the only one of the three
/// that cannot be wrong about a result it returns. Build it first, measure the
/// others against it, and if a dataset is small enough that brute force is fast
/// enough there is no reason to build anything else.
///
/// Ties break on insertion order, so the output is a function of the input
/// alone.
#[derive(Debug, Clone)]
pub struct FlatIndex {
    vectors: Vec<Vec<f64>>,
    dim: usize,
    metric: Metric,
}

impl FlatIndex {
    /// Builds an index over `vectors`.
    ///
    /// Rejects ragged input (the search would otherwise have to pick which
    /// length to believe), non-finite values, an empty slice, and a count past
    /// [`MAX_VECTORS`].
    ///
    /// A zero-vector row is **accepted**. It is only a problem under
    /// [`Metric::Cosine`], where it makes a query fail; rejecting it here would
    /// also reject a legitimate query vector, which is a normal thing to pass
    /// one. The failure surfaces at the search, naming the metric, instead.
    pub fn new(vectors: Vec<Vec<f64>>, metric: Metric) -> Result<Self, SearchError> {
        if vectors.len() > MAX_VECTORS {
            return Err(SearchError::TooManyVectors {
                count: vectors.len(),
                limit: MAX_VECTORS,
            });
        }
        let dim = vectors.first().map_or(0, Vec::len);
        if dim == 0 && !vectors.is_empty() {
            return Err(SearchError::EmptyVector);
        }
        for (id, vector) in vectors.iter().enumerate() {
            if vector.len() != dim {
                return Err(SearchError::DimensionMismatch {
                    expected: dim,
                    found: vector.len(),
                });
            }
            if let Some(bad) = vector.iter().position(|v| !v.is_finite()) {
                // A NaN poisons every comparison it takes part in, so it would
                // make the sort order arbitrary and silently break the tiebreak
                // this index promises.
                return Err(SearchError::Config(format!(
                    "vector {id} has a non-finite value at dimension {bad}"
                )));
            }
        }
        Ok(FlatIndex {
            vectors,
            dim,
            metric,
        })
    }

    /// The number of vectors.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// The dimensionality every vector has.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The metric this index searches under.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The stored vectors, in insertion order.
    pub fn vectors(&self) -> &[Vec<f64>] {
        &self.vectors
    }

    /// The `k` nearest vectors to `query`, nearest first.
    ///
    /// A `k` of zero returns nothing, and a `k` above the vector count returns
    /// everything — the same two cases the real `sqlite3` gives for
    /// `ORDER BY d LIMIT k`, checked against it rather than assumed.
    ///
    /// The heap is bounded by `k` so the cost is `O(n log k)`, but the returned
    /// list is fully sorted, including the ties, which is the part a caller
    /// actually depends on.
    pub fn search(&self, query: &[f64], k: usize) -> Result<Vec<Neighbor>, SearchError> {
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let k = k.min(self.len());
        check_query_dim(self.dim, query)?;

        let mut heap: BinaryHeap<Candidate> = BinaryHeap::with_capacity(k + 1);
        for (id, vector) in self.vectors.iter().enumerate() {
            let candidate = Candidate {
                distance: self.metric.distance(query, vector)?,
                id: id as u32,
            };
            if heap.len() < k {
                heap.push(candidate);
            } else if let Some(&worst) = heap.peek() {
                if candidate < worst {
                    heap.pop();
                    heap.push(candidate);
                }
            }
        }
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|c| Neighbor {
                id: c.id as usize,
                distance: c.distance,
            })
            .collect())
    }

    /// The distance from `query` to one stored vector, without ranking anything.
    pub fn distance_to(&self, query: &[f64], id: usize) -> Result<f64, SearchError> {
        let vector = self.vectors.get(id).ok_or(SearchError::TooManyVectors {
            count: id + 1,
            limit: self.vectors.len(),
        })?;
        check_query_dim(self.dim, query)?;
        self.metric.distance(query, vector)
    }
}

fn check_query_dim(dim: usize, query: &[f64]) -> Result<(), SearchError> {
    if query.is_empty() {
        return Err(SearchError::EmptyVector);
    }
    if query.len() != dim {
        return Err(SearchError::DimensionMismatch {
            expected: dim,
            found: query.len(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HNSW
// ---------------------------------------------------------------------------

/// Build-time parameters for [`HnswIndex`].
///
/// The defaults are `m = 16`, `efConstruction = 100`, which are the values
/// hnswlib uses and a reasonable starting point. `m` trades memory for recall:
/// each node keeps up to `2 * m` edges on the base layer, so doubling it
/// roughly doubles the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HnswConfig {
    /// Neighbours per node per layer above the base layer. Must be at least 2,
    /// because a graph with one neighbour per node cannot be navigated back
    /// along an edge it was not built with.
    pub m: usize,
    /// Size of the candidate list held while inserting. Larger means a better
    /// graph and a slower build.
    pub ef_construction: usize,
    /// Seed for the level assignment. Fixed by default so a build is
    /// reproducible; see [`HnswIndex::build`].
    pub seed: u64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        HnswConfig {
            m: 16,
            ef_construction: 100,
            seed: 0x5EED_C0DE_BA5E,
        }
    }
}

impl HnswConfig {
    /// A config with the given `m` and everything else at its default.
    pub fn with_m(m: usize) -> Self {
        HnswConfig {
            m,
            ..HnswConfig::default()
        }
    }
}

/// An approximate index built as a navigable small-world graph.
///
/// # What is implemented
///
/// The structure is Malkov and Yashunin's HNSW: nodes carry an exponentially
/// decaying number of layers, the entry point is the top of the tallest one, a
/// query descends greedily to the base layer and then does a bounded beam
/// search there. That is the part that makes it fast, and it is here in full.
///
/// # What is simplified, and what it costs
///
/// 1. **No heuristic neighbour selection.** The paper's Algorithm 4 picks the `M`
///    neighbours of a link by a diversity rule that keeps a node from filling up
///    with near-duplicates in the same direction. This keeps plain nearest `M`
///    and truncates. Consequence: clusters are less well separated, so recall
///    at low `ef` is measurably worse than a full implementation's. Raising `ef`
///    recovers most of it, which is the usual trade and the reason `ef` is a
///    per-query parameter at all.
/// 2. **No `extend_neighbors` / false-connection removal** at the upper layers.
///    Same direction of loss, smaller magnitude, since those layers are small.
/// 3. **Single-threaded, build-then-search.** There is no `mark_deleted`, no
///    concurrent insertion, and no in-place update. A node's neighbours are only
///    written during the build, which is what makes the search read-only and
///    `&self`.
/// 4. **`ef` is a search argument, not a stored one.** It must be at least `k`;
///    a smaller value is raised to `k` rather than rejected, because clamping is
///    what a kNN virtual table has to do to honour `LIMIT k` anyway.
/// 5. **f64 storage, f64 arithmetic, no SIMD.** Correct and simple, not fast per
///    comparison. The number of comparisons is what makes the search cheap, and
///    that part is real.
/// 6. **In memory only.** No serialization, no mmap, no incremental build.
///
/// # Reproducibility
///
/// A node's level is drawn from a seeded [`SplitMix64`], in insertion order, so
/// the same input and the same [`HnswConfig::seed`] give byte-identical graphs
/// and identical results. That is a deliberate difference from hnswlib, whose
/// level draw depends on its own internal RNG state. A test that measures recall
/// twice needs the graph to be the same graph.
#[derive(Debug, Clone)]
pub struct HnswIndex {
    vectors: Vec<Vec<f64>>,
    dim: usize,
    metric: Metric,
    m: usize,
    m0: usize,
    ef_construction: usize,
    levels: Vec<u32>,
    /// `neighbors[layer][node]` — the edges of one node on one layer. A node
    /// that is not present on a layer has an empty list there, so the table is
    /// dense and needs no lookup to see whether a node exists.
    neighbors: Vec<Vec<Vec<u32>>>,
    entry_point: u32,
    max_level: i32,
    seed: u64,
}

impl HnswIndex {
    /// Builds a graph over `index`.
    ///
    /// Takes a borrow rather than the vectors so the caller keeps its
    /// [`FlatIndex`] and can use it as the exact baseline the graph is measured
    /// against. The copy is of the vectors only; the graph itself is built here.
    pub fn build(index: &FlatIndex, config: HnswConfig) -> Result<Self, SearchError> {
        if config.m < 2 {
            return Err(SearchError::Config(format!(
                "HNSW m must be at least 2, got {}",
                config.m
            )));
        }
        if config.ef_construction < 1 {
            return Err(SearchError::Config(
                "HNSW ef_construction must be at least 1".to_string(),
            ));
        }

        let n = index.len();
        let mut levels = Vec::with_capacity(n);
        let mut rng = SplitMix64::new(config.seed);
        for _ in 0..n {
            levels.push(assign_level(&mut rng, config.m));
        }
        let max_level = levels.iter().copied().max().unwrap_or(0) as i32;
        let layer_count = (max_level + 1) as usize;
        let neighbors = vec![vec![Vec::new(); n]; layer_count];

        let mut graph = HnswIndex {
            vectors: index.vectors().to_vec(),
            dim: index.dim(),
            metric: index.metric(),
            m: config.m,
            m0: config.m * 2,
            ef_construction: config.ef_construction,
            levels,
            neighbors,
            entry_point: u32::MAX,
            max_level: -1,
            seed: config.seed,
        };
        for id in 0..n as u32 {
            graph.insert(id, config.ef_construction)?;
        }
        Ok(graph)
    }

    /// The number of vectors.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether the graph holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// The dimensionality every node holds.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The metric this graph searches under.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The seed the level assignment was drawn from.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The candidate-list width the graph was built with. Recorded for
    /// diagnostics: changing it on a live graph does nothing, because the edges
    /// are already chosen.
    pub fn ef_construction(&self) -> usize {
        self.ef_construction
    }

    /// The highest layer any node reaches. `-1` on an empty graph.
    pub fn max_level(&self) -> i32 {
        self.max_level
    }

    /// The node a search enters at, or `None` on an empty graph.
    ///
    /// The paper's entry point: the topmost node, which every search starts
    /// from. Exposed so a test can assert that a build actually produced a
    /// multi-layer graph rather than a flat list with extra steps.
    pub fn entry_point(&self) -> Option<usize> {
        if self.max_level < 0 {
            None
        } else {
            Some(self.entry_point as usize)
        }
    }

    /// The level assigned to one node.
    pub fn level_of(&self, id: usize) -> Option<u32> {
        self.levels.get(id).copied()
    }

    /// The number of edges on one layer of one node.
    pub fn degree(&self, id: usize, layer: u32) -> usize {
        self.neighbors
            .get(layer as usize)
            .and_then(|l| l.get(id))
            .map_or(0, Vec::len)
    }

    /// The `k` nearest vectors to `query`, using a beam of width `ef`.
    ///
    /// `ef` is the accuracy/speed dial. A small `ef` examines few candidates and
    /// can miss a true neighbour that the graph connects to only through a poor
    /// route; a large one examines more and misses less, for proportionally
    /// more distance computations. It is raised to `k` when smaller, because a
    /// beam narrower than the result list could not fill it.
    ///
    /// Returned distances are recomputed against the stored `f64` vectors rather
    /// than carried out of the search, so they are exact even if the navigation
    /// that found the node was approximate. `ef` therefore affects *which rows*
    /// come back, never *what they score*.
    pub fn search(&self, query: &[f64], k: usize, ef: usize) -> Result<Vec<Neighbor>, SearchError> {
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let k = k.min(self.len());
        check_query_dim(self.dim, query)?;
        let ef = ef.max(k).max(1);

        let mut scratch = SearchScratch::new(self.len());

        // Down the upper layers: greedy, no beam. Those layers hold few nodes and
        // the job here is only to arrive at a good starting point for layer 0.
        let mut current = self.entry_point;
        for layer in (1..=self.max_level.max(0) as u32).rev() {
            current = self.greedy_descend(query, current, layer as usize)?;
        }

        // On layer 0: the real beam search.
        let found = self.search_layer(query, &[current], 0, ef, &mut scratch)?;
        let mut out = Vec::with_capacity(k.min(found.len()));
        for candidate in found.iter().take(k) {
            out.push(Neighbor {
                id: candidate.id as usize,
                distance: self.metric.distance(query, &self.vectors[candidate.id as usize])?,
            });
        }
        out.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    fn distance(&self, query: &[f64], id: u32) -> Result<f64, SearchError> {
        self.metric
            .distance(query, &self.vectors[id as usize])
    }

    /// Greedy walk of one upper layer: keep stepping to the nearest neighbour
    /// until none of them is nearer than where we already are.
    fn greedy_descend(
        &self,
        query: &[f64],
        entry: u32,
        layer: usize,
    ) -> Result<u32, SearchError> {
        let mut current = entry;
        let mut current_distance = self.distance(query, current)?;
        loop {
            let mut improved = false;
            for &neighbour in &self.neighbors[layer][current as usize] {
                let candidate = self.distance(query, neighbour)?;
                if candidate < current_distance {
                    current = neighbour;
                    current_distance = candidate;
                    improved = true;
                }
            }
            if !improved {
                return Ok(current);
            }
        }
    }

    /// The paper's beam search on one layer.
    ///
    /// `candidates` is a min-heap of nodes still to expand; `results` is a
    /// max-heap of the best `ef` seen so far. A node is expanded while it can
    /// still reach a result better than the worst one held.
    fn search_layer(
        &self,
        query: &[f64],
        entry_points: &[u32],
        layer: usize,
        ef: usize,
        scratch: &mut SearchScratch,
    ) -> Result<Vec<Candidate>, SearchError> {
        if entry_points.is_empty() {
            return Ok(Vec::new());
        }
        scratch.new_generation();

        let mut candidates: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(ef);
        let mut results: BinaryHeap<Candidate> = BinaryHeap::with_capacity(ef + 1);

        for &entry in entry_points {
            if scratch.mark(entry) {
                continue;
            }
            let candidate = Candidate {
                distance: self.distance(query, entry)?,
                id: entry,
            };
            candidates.push(Reverse(candidate));
            results.push(candidate);
        }

        while let Some(Reverse(current)) = candidates.pop() {
            // Strictly worse, not merely no better: a candidate at exactly the
            // current cut-off is still expanded. hnswlib makes the same choice,
            // and it is the conservative one — it costs a few comparisons and
            // keeps a tied row from being dropped on a rounding accident.
            if results.len() >= ef {
                if let Some(&worst) = results.peek() {
                    if current > worst {
                        break;
                    }
                }
            }
            for &neighbour in &self.neighbors[layer][current.id as usize] {
                if scratch.mark(neighbour) {
                    continue;
                }
                let candidate = Candidate {
                    distance: self.distance(query, neighbour)?,
                    id: neighbour,
                };
                if results.len() < ef {
                    candidates.push(Reverse(candidate));
                    results.push(candidate);
                } else if let Some(&worst) = results.peek() {
                    if candidate < worst {
                        results.pop();
                        candidates.push(Reverse(candidate));
                        results.push(candidate);
                    }
                }
            }
        }

        Ok(results.into_sorted_vec())
    }

    /// Links one node into the graph.
    ///
    /// The node is not yet in any neighbour list, so it cannot be rediscovered as
    /// its own neighbour while it is being inserted — which is why `search_layer`
    /// needs no special case to exclude it.
    fn insert(&mut self, id: u32, ef_construction: usize) -> Result<(), SearchError> {
        if self.max_level < 0 {
            self.entry_point = id;
            self.max_level = self.levels[id as usize] as i32;
            return Ok(());
        }

        let query = self.vectors[id as usize].clone();
        let level = self.levels[id as usize];
        let mut entry = self.entry_point;

        // Above the new node's own top layer there is nothing to link, only a
        // route down to where the linking starts.
        for layer in (level + 1..=self.max_level as u32).rev() {
            entry = self.greedy_descend(&query, entry, layer as usize)?;
        }

        let top = level.min(self.max_level as u32);
        for layer in (0..=top).rev() {
            let found = self.search_layer(
                &query,
                &[entry],
                layer as usize,
                ef_construction.max(self.m),
                &mut SearchScratch::new(self.len()),
            )?;
            let limit = self.m_of_layer(layer as usize);
            let selected: Vec<u32> = found
                .iter()
                .take(limit)
                .map(|c| c.id)
                .filter(|&neighbour| neighbour != id)
                .collect();

            for &neighbour in &selected {
                self.neighbors[layer as usize][id as usize].push(neighbour);
                self.link_and_prune(layer as usize, neighbour, id, &query, limit)?;
            }
            if let Some(closest) = found.first() {
                entry = closest.id;
            }
        }

        if level as i32 > self.max_level {
            self.max_level = level as i32;
            self.entry_point = id;
        }
        Ok(())
    }

    /// The edge budget for one layer: the base layer gets twice the others, as
    /// in the paper, because it carries every search.
    fn m_of_layer(&self, layer: usize) -> usize {
        if layer == 0 {
            self.m0
        } else {
            self.m
        }
    }

    /// Adds the back edge `neighbour -> id` and trims `neighbour` back to its
    /// budget if it overflowed.
    ///
    /// This is where simplification 1 is visible: the trim keeps the nearest
    /// `limit` and drops the rest, with no diversity heuristic.
    fn link_and_prune(
        &mut self,
        layer: usize,
        neighbour: u32,
        id: u32,
        query: &[f64],
        limit: usize,
    ) -> Result<(), SearchError> {
        let list = &mut self.neighbors[layer][neighbour as usize];
        if !list.contains(&id) {
            list.push(id);
        }
        if list.len() <= limit {
            return Ok(());
        }
        let mut kept: Vec<Candidate> = list
            .iter()
            .copied()
            .map(|node| Ok(Candidate {
                distance: self.metric.distance(query, &self.vectors[node as usize])?,
                id: node,
            }))
            .collect::<Result<Vec<_>, SearchError>>()?;
        kept.sort_unstable();
        kept.truncate(limit);
        *list = kept.into_iter().map(|c| c.id).collect();
        Ok(())
    }
}

/// The largest level a node can be assigned.
///
/// A level is `floor(-ln(u) * mL)` with `u` in `(0, 1)`, and the uniform this
/// draw comes from cannot get closer to zero than `2^-54`, so the value is
/// bounded in practice. The cap is a backstop for a caller who asks for a very
/// small `m`, not a limit the arithmetic actually reaches.
const MAX_LEVEL: u32 = 32;

/// Draws one node's level: `floor(-ln(u) * mL)`, with `mL = 1 / ln(m)`.
fn assign_level(rng: &mut SplitMix64, m: usize) -> u32 {
    let multiplier = 1.0 / (m as f64).ln();
    let level = (-rng.next_f64_unit().ln() * multiplier).floor();
    if !level.is_finite() || level < 0.0 {
        0
    } else {
        (level as u32).min(MAX_LEVEL)
    }
}

/// The visited set for one beam search, stamped by generation.
///
/// A `Vec<bool>` would have to be cleared between layers and between queries,
/// which is `O(n)` each time and would dominate the cost of a narrow `ef`. A
/// generation counter makes "reset" an integer increment.
#[derive(Debug)]
struct SearchScratch {
    visited: Vec<u32>,
    generation: u32,
}

impl SearchScratch {
    fn new(n: usize) -> Self {
        SearchScratch {
            // `u32::MAX` is not a generation this struct ever hands out, so a
            // fresh vector reads as wholly unvisited.
            visited: vec![u32::MAX; n],
            generation: 0,
        }
    }

    fn new_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == u32::MAX {
            // Wrapped onto the sentinel. Clear, then restart below it.
            self.visited.fill(u32::MAX);
            self.generation = 0;
        }
    }

    /// Marks `id` visited. Returns `true` if it already was.
    fn mark(&mut self, id: u32) -> bool {
        let slot = &mut self.visited[id as usize];
        if *slot == self.generation {
            return true;
        }
        *slot = self.generation;
        false
    }
}

/// A small, fast, fully specified PRNG for the level draw.
///
/// SplitMix64 is used rather than a dependency because the only thing needed is
/// a reproducible uniform in `(0, 1)`, and adding `rand` to a crate that is
/// otherwise three HTTP dependencies to get one would be a poor trade. It is
/// fully specified, so the graph is reproducible from the seed alone — no
/// dependency version can change the answer.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform in `(0, 1)`, never 0 and never 1.
    ///
    /// Strictly positive matters: the level draw takes `ln` of it, and `ln(0)` is
    /// negative infinity. The half-step keeps the endpoints out even when the
    /// 53-bit mantissa lands on 0.
    fn next_f64_unit(&mut self) -> f64 {
        const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
        let mantissa = (self.next_u64() >> 11) as f64;
        (mantissa + 0.5) * SCALE
    }
}

// ---------------------------------------------------------------------------
// Quantization
// ---------------------------------------------------------------------------

/// The per-vector error between a stored vector and its int8 reconstruction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconstructionError {
    /// Largest absolute error over all dimensions.
    pub max_abs: f64,
    /// Root-mean-square error over all dimensions.
    pub rms: f64,
}

/// What int8 quantization cost, measured rather than assumed.
///
/// Every field is a comparison against the exact [`FlatIndex`], computed by
/// [`QuantizedIndex::compare_to_exact`]. A quantized search that reports itself
/// exact is reporting nothing, so the report is the point of the type: it says
/// how many of the true top-`k` survived and how far the returned *distances*
/// moved.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantizationReport {
    /// The `k` the comparison was made at.
    pub k: usize,
    /// How many of the exact top-`k` appear in the quantized top-`k`.
    pub exact_hits: usize,
    /// `exact_hits / k`, from [`recall_at_k`].
    pub recall_at_k: f64,
    /// Largest distance error over the rows both searches returned, comparing
    /// the quantized distance with the exact distance for the *same* row.
    pub max_distance_error: f64,
    /// Mean of those distance errors.
    pub mean_distance_error: f64,
    /// Largest reconstruction error over every stored vector, before any search.
    pub max_reconstruction_error: f64,
    /// Bytes per vector as `f64`.
    pub bytes_per_vector_exact: usize,
    /// Bytes per vector as int8, excluding the per-vector scale.
    pub bytes_per_vector_quantized: usize,
}

impl QuantizationReport {
    /// The compression factor, `exact / quantized`. A bare `f64` of 8 bytes
    /// against an `i8` of 1 is 8; the 8 here is the ideal and this is the
    /// realised one, identical apart from the scales.
    pub fn compression(&self) -> f64 {
        self.bytes_per_vector_exact as f64 / self.bytes_per_vector_quantized as f64
    }

    /// One line, for a log or a test failure message.
    pub fn summary(&self) -> String {
        format!(
            "recall@{} = {}/{} ({:.3}), max |distance error| = {:.6e}, \
             mean = {:.6e}, max reconstruction error = {:.6}e, \
             {} B -> {} B per vector",
            self.k,
            self.exact_hits,
            self.k,
            self.recall_at_k,
            self.max_distance_error,
            self.mean_distance_error,
            self.max_reconstruction_error,
            self.bytes_per_vector_exact,
            self.bytes_per_vector_quantized,
        )
    }
}

/// A [`FlatIndex`] re-encoded as int8, one scale per vector.
///
/// # The scheme
///
/// Scalar quantization: `scale = max|v| / 127` per vector, then
/// `q = round(v / scale)` clamped to `[-127, 127]`. Dequantization is
/// `q * scale`, so the error on any one dimension is at most half a step,
/// `scale / 2`.
///
/// A single per-vector scale is chosen over a per-dimension or global one
/// because it is one `f64` per vector against `dim` of them, and because a
/// corpus of embeddings usually shares a scale across dimensions. It does cost
/// accuracy: one outlier component sets the step for all the others. That shows
/// up in [`QuantizationReport::max_reconstruction_error`].
///
/// The range is `[-127, 127]`, not `[-128, 127]`, so negation is exact and
/// `i8::MIN` never appears in an index.
///
/// # What is not claimed
///
/// The stored vectors are dequantized to `f64` before the metric runs, rather
/// than accumulated in integer arithmetic. That is a real cost — a production
/// int8 kernel would dot the `i8`s directly — and it is also what makes the
/// error in [`QuantizationReport`] attributable. Every number in the report is
/// caused by the quantization, because no integer accumulation, no saturation,
/// and no f32 rounding is in the path to perturb it. An int8 kernel would add
/// its own, separate error on top.
#[derive(Debug, Clone)]
pub struct QuantizedIndex {
    quantized: Vec<Vec<i8>>,
    scales: Vec<f64>,
    dim: usize,
    metric: Metric,
}

impl QuantizedIndex {
    /// Quantizes `index`.
    pub fn new(index: &FlatIndex) -> Result<Self, SearchError> {
        if index.dim() == 0 && !index.is_empty() {
            return Err(SearchError::EmptyVector);
        }
        let mut quantized = Vec::with_capacity(index.len());
        let mut scales = Vec::with_capacity(index.len());
        for vector in index.vectors() {
            let (row, scale) = quantize_vector(vector)?;
            quantized.push(row);
            scales.push(scale);
        }
        Ok(QuantizedIndex {
            quantized,
            scales,
            dim: index.dim(),
            metric: index.metric(),
        })
    }

    /// The number of vectors.
    pub fn len(&self) -> usize {
        self.quantized.len()
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.quantized.is_empty()
    }

    /// The dimensionality every stored vector has.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The metric this index searches under.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The scale factor for one vector — the step between two int8 codes.
    pub fn scale(&self, id: usize) -> f64 {
        self.scales[id]
    }

    /// The stored int8 codes for one vector.
    pub fn quantized(&self, id: usize) -> &[i8] {
        &self.quantized[id]
    }

    /// The `f64` reconstruction of one stored vector.
    pub fn dequantize(&self, id: usize) -> Vec<f64> {
        let scale = self.scales[id];
        self.quantized[id]
            .iter()
            .map(|&q| q as f64 * scale)
            .collect()
    }

    /// How far one stored vector is from the original, per dimension.
    pub fn reconstruction_error(&self, exact: &FlatIndex, id: usize) -> ReconstructionError {
        let stored = &exact.vectors()[id];
        let scale = self.scales[id];
        let mut sum_squares = 0.0;
        let mut max_abs: f64 = 0.0;
        for (original, &q) in stored.iter().zip(&self.quantized[id]) {
            let error = original - q as f64 * scale;
            sum_squares += error * error;
            max_abs = max_abs.max(error.abs());
        }
        ReconstructionError {
            max_abs,
            rms: (sum_squares / stored.len() as f64).sqrt(),
        }
    }

    /// The `k` nearest vectors, quantizing the query as well.
    ///
    /// This is the honest end-to-end path: the query goes through the same lossy
    /// encoding as the stored vectors, so the error includes the query's own.
    /// A caller who wants to isolate the storage error asks for
    /// [`QuantizedIndex::search_with_exact_query`].
    pub fn search(&self, query: &[f64], k: usize) -> Result<Vec<Neighbor>, SearchError> {
        let (row, _) = quantize_vector(query)?;
        self.search_codes(&row, k)
    }

    /// The `k` nearest vectors, keeping the query in `f64`.
    ///
    /// Isolates the error introduced by the *stored* vectors, which is the part
    /// [`QuantizationReport::max_reconstruction_error`] measures.
    pub fn search_with_exact_query(
        &self,
        query: &[f64],
        k: usize,
    ) -> Result<Vec<Neighbor>, SearchError> {
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let k = k.min(self.len());
        check_query_dim(self.dim, query)?;
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::with_capacity(k + 1);
        for id in 0..self.quantized.len() {
            let distance = self.metric.distance(query, &self.dequantize(id))?;
            push_candidate(&mut heap, Candidate { distance, id: id as u32 }, k);
        }
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|c| Neighbor {
                id: c.id as usize,
                distance: c.distance,
            })
            .collect())
    }

    /// [`QuantizedIndex::search`] over already-quantized query codes.
    pub fn search_codes(&self, query: &[i8], k: usize) -> Result<Vec<Neighbor>, SearchError> {
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let k = k.min(self.len());
        if query.len() != self.dim {
            return Err(SearchError::DimensionMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        // The query is dequantized once, not per row: it has a single scale, and
        // recomputing it inside the loop would be the same number every time.
        let scale = query_scale(query);
        let dequantized: Vec<f64> = query.iter().map(|&q| q as f64 * scale).collect();
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::with_capacity(k + 1);
        for id in 0..self.quantized.len() {
            let distance = self.metric.distance(&dequantized, &self.dequantize(id))?;
            push_candidate(
                &mut heap,
                Candidate {
                    distance,
                    id: id as u32,
                },
                k,
            );
        }
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|c| Neighbor {
                id: c.id as usize,
                distance: c.distance,
            })
            .collect())
    }

    /// Searches, and reports what the quantization cost against `exact`.
    ///
    /// This is the interesting output. Both searches are run, the two result
    /// lists are compared by id, and the distance each one reports for a
    /// *shared* row is subtracted, so the distance error is a like-for-like
    /// comparison rather than two lists of unrelated numbers.
    pub fn compare_to_exact(
        &self,
        exact: &FlatIndex,
        query: &[f64],
        k: usize,
    ) -> Result<QuantizationReport, SearchError> {
        let approx = self.search(query, k)?;
        let baseline = exact.search(query, k)?;
        build_report(self, exact, &baseline, &approx, k)
    }

    /// [`QuantizedIndex::compare_to_exact`] with the query kept exact.
    pub fn compare_to_exact_query_preserved(
        &self,
        exact: &FlatIndex,
        query: &[f64],
        k: usize,
    ) -> Result<QuantizationReport, SearchError> {
        let approx = self.search_with_exact_query(query, k)?;
        let baseline = exact.search(query, k)?;
        build_report(self, exact, &baseline, &approx, k)
    }
}

fn push_candidate(heap: &mut BinaryHeap<Candidate>, candidate: Candidate, k: usize) {
    if heap.len() < k {
        heap.push(candidate);
    } else if let Some(&worst) = heap.peek() {
        if candidate < worst {
            heap.pop();
            heap.push(candidate);
        }
    }
}

/// The step size shared by a vector's codes, recovered from the codes alone.
///
/// A quantized query is a row of `i8` with no scale beside it, so the scale has
/// to come back out of the row. `max|q| / 127` inverts the encoding exactly when
/// the largest component saturated to 127, which it does whenever the vector
/// has any component at its own maximum — which is the definition of the scale.
/// A row of all zeros is the exception and has no scale to recover; it gets 1.0
/// and dequantizes to all zeros, which is the right answer.
fn query_scale(query: &[i8]) -> f64 {
    let peak = query.iter().map(|q| (*q as i32).abs()).max().unwrap_or(0);
    if peak == 0 {
        1.0
    } else {
        peak as f64 / 127.0
    }
}

fn build_report(
    quantized: &QuantizedIndex,
    exact: &FlatIndex,
    baseline: &[Neighbor],
    approx: &[Neighbor],
    k: usize,
) -> Result<QuantizationReport, SearchError> {
    let k = k.min(exact.len());
    let baseline_ids: std::collections::HashSet<usize> =
        baseline.iter().take(k).map(|n| n.id).collect();
    let exact_hits = approx
        .iter()
        .take(k)
        .filter(|n| baseline_ids.contains(&n.id))
        .count();

    // Distance error is only meaningful for a row both searches returned: two
    // different rows have different distances for reasons that have nothing to
    // do with quantization.
    let approx_by_id: std::collections::HashMap<usize, f64> =
        approx.iter().map(|n| (n.id, n.distance)).collect();
    let mut max_distance_error: f64 = 0.0;
    let mut sum_distance_error = 0.0;
    let mut compared = 0usize;
    for base in baseline.iter().take(k) {
        if let Some(&distance) = approx_by_id.get(&base.id) {
            let error = (distance - base.distance).abs();
            max_distance_error = max_distance_error.max(error);
            sum_distance_error += error;
            compared += 1;
        }
    }
    let mean_distance_error = if compared == 0 {
        0.0
    } else {
        sum_distance_error / compared as f64
    };

    let max_reconstruction_error = (0..exact.len())
        .map(|id| quantized.reconstruction_error(exact, id).max_abs)
        .fold(0.0f64, f64::max);

    Ok(QuantizationReport {
        k,
        exact_hits,
        recall_at_k: recall_at_k(baseline, approx, k),
        max_distance_error,
        mean_distance_error,
        max_reconstruction_error,
        bytes_per_vector_exact: exact.dim() * std::mem::size_of::<f64>(),
        bytes_per_vector_quantized: quantized.dim(),
    })
}

/// Quantizes one vector to `i8` and returns its scale.
///
/// An all-zero vector gets a scale of `1.0` rather than `0.0`: with a zero
/// scale every component would be `0 / 0`, and the codes would be NaN cast to
/// `i8`, which is zero in Rust but is a documented trap rather than a decision.
/// A scale of 1.0 makes the round trip exact for a zero vector, which is the
/// answer we actually want.
fn quantize_vector(vector: &[f64]) -> Result<(Vec<i8>, f64), SearchError> {
    if vector.is_empty() {
        return Err(SearchError::EmptyVector);
    }
    if let Some(bad) = vector.iter().position(|v| !v.is_finite()) {
        return Err(SearchError::Config(format!(
            "cannot quantize a vector with a non-finite value at dimension {bad}"
        )));
    }
    let peak = vector.iter().fold(0.0f64, |acc, v| acc.max(v.abs()));
    if peak == 0.0 {
        return Ok((vec![0; vector.len()], 1.0));
    }
    let scale = peak / 127.0;
    let row = vector
        .iter()
        .map(|&v| (v / scale).round().clamp(-127.0, 127.0) as i8)
        .collect();
    Ok((row, scale))
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

/// One window of a document, with the byte range it came from.
///
/// The offsets are the useful part. A retrieved chunk is a *pointer into* the
/// original document, so a caller can highlight the exact span, run a second
/// pass over just that span, or stitch two chunks back together, none of which
/// is possible from the text alone once it has been copied out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position of this chunk in the returned list, from 0.
    pub index: usize,
    /// Byte offset of the first character of the chunk in the source text.
    pub start: usize,
    /// Byte offset one past the last character of the chunk.
    pub end: usize,
    /// The chunk's text.
    pub text: String,
}

impl Chunk {
    /// The chunk's length in bytes, matching `end - start`.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the chunk is empty. Only possible for an empty source.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Splits `text` into overlapping windows of `size` characters.
///
/// Windows advance by `size - overlap` characters, so consecutive chunks share
/// exactly `overlap` characters. That overlap is the entire reason this function
/// exists: a sentence that straddles a boundary is not half in each window, it
/// is **whole in both**. Without an overlap the boundary itself is lost, and the
/// text spanning one is precisely the text a retrieval system cannot find.
///
/// Two details a naive split gets wrong, and which are pinned by the tests:
///
/// * **The last window runs to the end of the text.** A loop that stops when the
///   window is full, rather than when it reaches the end, drops the tail. Here
///   the final chunk may be shorter than `size` and always ends at
///   `text.len()`.
/// * **Windows are counted in characters, and split on character boundaries.**
///   Counting bytes would cut a multi-byte character in half and panic. `size`
///   means characters, so `size` is a meaningful upper bound on what a single
///   embedder call sees.
///
/// An empty input yields no chunks. An input shorter than one window yields
/// exactly one, so a short document is never lost to the splitter.
pub fn chunk_text(text: &str, size: usize, overlap: usize) -> Result<Vec<Chunk>, SearchError> {
    if size == 0 {
        return Err(SearchError::ZeroChunkSize);
    }
    if overlap >= size {
        return Err(SearchError::OverlapTooLarge { size, overlap });
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }

    let boundaries: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
    let total = boundaries.len();
    let step = size - overlap;

    let mut chunks = Vec::new();
    let mut start_index = 0usize;
    loop {
        let end_index = start_index.saturating_add(size).min(total);
        let start_byte = boundaries[start_index];
        // `total` is the char count, so `end_index == total` means the window
        // reaches the last character and the byte end is the string's own end.
        let end_byte = if end_index < total {
            boundaries[end_index]
        } else {
            text.len()
        };
        chunks.push(Chunk {
            index: chunks.len(),
            start: start_byte,
            end: end_byte,
            text: text[start_byte..end_byte].to_string(),
        });
        if end_index == total {
            break;
        }
        start_index += step;
    }
    Ok(chunks)
}

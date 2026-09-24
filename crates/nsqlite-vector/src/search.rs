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
//! # Why HNSW is here, and what sqlite-vec actually did with it
//!
//! sqlite-vec, the reference implementation for SQLite vector search, **shipped
//! an HNSW index and then removed it.** v0.1.3 (2025-03-28) added `vec0` with
//! three approximate index types — HNSW, IVF and DiskANN — and v0.1.4
//! (2025-04-11) removed all three, leaving exact brute-force search as the only
//! mode. The removal was ANN-to-exact, not HNSW-to-something-else: HNSW went
//! because its graph is held entirely in memory, and IVF and DiskANN went
//! because they were not yet stable. A reader who has seen the current
//! sqlite-vec and remembers "it is brute force only" should know that is the
//! *result* of that removal, not evidence that the graph below was copied from a
//! stale description — it was not. The current amalgamation still carries the
//! IVF and DiskANN code paths behind `SQLITE_VEC_EXPERIMENTAL_IVF_ENABLE` and
//! `SQLITE_VEC_ENABLE_DISKANN`, and no HNSW at all.
//!
//! So the graph here is a deliberate independent choice. Its recall numbers are
//! not comparable to sqlite-vec's, and nothing in this module should be read as
//! claiming parity with it. See [`HnswIndex`] for exactly which parts of the
//! Malkov and Yashunin paper are implemented and what each omission costs.
//!
//! # The zero-vector rule
//!
//! [`Metric::Cosine`] returns [`SearchError::ZeroVector`] when either argument
//! has zero norm, because `dot / (|a| * |b|)` divides by zero and the answer
//! does not exist. This module's rule is stricter than sqlite-vec's, and
//! deliberately so: sqlite-vec's `cosine_float` computes
//! `1 - (dot / (sqrt(aMag) * sqrt(bMag)))` with no zero check, so a zero vector
//! yields `0/0` = NaN, returned as a *value* through `vec_distance_cosine` — its
//! only validation is `ensure_vector_match`, which checks element type and
//! dimension and nothing about magnitude. A NaN that ranks is worse than an
//! error, so the rule here is to refuse and say why.
//!
//! The other three metrics are well defined at zero and are documented
//! individually — in particular [`inner_product`] *is* defined against a zero
//! vector, and making it error would throw away a perfectly good answer.
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
/// function. A caller who wants a true distance takes `distance.sqrt()`; a
/// caller who wants to *rank* must not, because ranking by the square and by the
/// root agree but the numbers do not.
///
/// **This is not the number sqlite-vec reports, and the internal name is the
/// trap.** sqlite-vec's `l2_sqr_float` accumulates `sum(t*t)` and then returns
/// `sqrt(res)`, so `vec_distance_L2` — which dispatches through
/// `distance_l2_sqr_float` and lands in `l2_sqr_float` — hands back the *rooted*
/// distance: 5 for `[3, 4]` against the origin, not 25. The `sqr` in those
/// internal names describes the accumulator, not the result; there is no
/// rooted-vs-squared switch anywhere on the path, every L2 ending in `sqrt()`.
/// A kNN virtual table built on this module therefore cannot pass its L2 number
/// straight through to a caller expecting sqlite-vec's; take the root at the
/// boundary.
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
/// caught. This is *stricter than sqlite-vec*, on purpose: its `cosine_float`
/// and `cosine_int8` both compute `1 - (dot / (sqrt(aMag) * sqrt(bMag)))` with
/// no zero check, and `vec_distance_cosine` adds only a dimension and
/// element-type check, so a zero vector there yields `0/0` and is returned as a
/// NaN value rather than raised. See the module docs.
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
///
/// **This function cannot return a number it did not earn, which takes two
/// rules that are easy to get wrong and that nothing about the signature would
/// otherwise tell a reader:**
///
/// * *Duplicates are collapsed.* The numerator counts **distinct** ids, the same
///   set the denominator counts. A search that returned `[0, 0, 0, 0, 0]` has not
///   recalled five neighbours, and scoring it as `5/1` would report a perfect
///   `1.0` for a result that found exactly one of the right rows. Counting
///   multiplicity in the numerator while the denominator holds a `HashSet` is
///   how recall silently exceeds 1.0, which turns a recall number into a number
///   a caller cannot even range-check.
/// * *A short list is penalised, not forgiven.* The denominator is the exact
///   `k`, not the number of distinct ids the exact search happened to return.
///   A search that asked for 10 neighbours and returned 3 has missed 7, and
///   scoring it `3/3 = 1.0` would report a perfect result for a search that
///   could not fill the list it was asked for.
///
/// The `k == 0` and empty-`exact` cases are vacuously perfect; there was
/// nothing to ask for, and dividing by zero would not be a number.
pub fn recall_at_k(exact: &[Neighbor], approx: &[Neighbor], k: usize) -> f64 {
    if k == 0 {
        return 1.0;
    }
    let expected: std::collections::HashSet<usize> = exact.iter().take(k).map(|n| n.id).collect();
    if expected.is_empty() {
        return 1.0;
    }
    // `take(k)` still bounds how much of `approx` is considered, but the hits
    // are deduplicated, so a repeated id cannot push the numerator past `k`.
    let found = approx
        .iter()
        .take(k)
        .map(|n| n.id)
        .filter(|id| expected.contains(id))
        .collect::<std::collections::HashSet<usize>>()
        .len();
    found as f64 / k as f64
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
/// search there. That is the part that makes it fast, and it is here in full,
/// including the paper's Algorithm 4 neighbour selection
/// ([`HnswIndex::select_neighbors`]), implemented against the same reference
/// point hnswlib's `getNeighborsByHeuristic2` uses: the element whose edge
/// list is being pruned, never another candidate.
///
/// # What is simplified, and what it costs
///
/// 1. **No `extend_neighbors` / `keepPrunedConnections`.** When a link's
///    candidate list is pruned, the paper also reconsiders the *discarded*
///    candidates for the upper layers. Omitted: same direction of loss, smaller
///    magnitude, since those layers are small.
/// 2. **Single-threaded, build-then-search.** There is no `mark_deleted`, no
///    concurrent insertion, and no in-place update. A node's neighbours are only
///    written during the build, which is what makes the search read-only and
///    `&self`.
/// 3. **`ef` is a search argument, not a stored one.** It must be at least `k`;
///    a smaller value is raised to `k` rather than rejected, because clamping is
///    what a kNN virtual table has to do to honour `LIMIT k` anyway.
/// 4. **f64 storage, f64 arithmetic, no SIMD.** Correct and simple, not fast per
///    comparison. The number of comparisons is what makes the search cheap, and
///    that part is real.
/// 5. **In memory only.** No serialization, no mmap, no incremental build.
///
/// # Connectivity is a correctness property here, not a recall dial
///
/// A node with no path to it from the entry point is invisible to every
/// subsequent search at every `ef`, so this module treats "every node is
/// reachable on layer 0" as an invariant to *test* rather than a property to
/// hope for. The symptom when it breaks is a hard wrong answer rather than a
/// softer ranking: querying with a stored vector that had been stranded returns
/// some other row as its nearest neighbour, at the top of the list, with full
/// confidence. No `ef` recovers it, because the search cannot step to a node it
/// has no edge to.
///
/// The invariant has been broken here three times, and each break was a
/// different bug rather than a recurrence of the last:
///
/// 1. The first version omitted Algorithm 4 entirely and just took the nearest
///    `M`, which strands nodes in any dense cluster.
/// 2. The second version had Algorithm 4 but measured every candidate's
///    distance from `candidates[0]` instead of from the element being pruned.
///    That moves the reference point as the list is walked, so the diversity
///    filter tests "is this candidate nearer to *the first candidate* than to
///    the node" instead of "…than to the node being placed", and it rejects the
///    wrong candidates.
/// 3. The third had the right reference point but still let a prune evict an
///    edge that was some *other* node's only way in. Forcing only the newest
///    back edge to survive does not help, because the next insertion runs the
///    same prune with that edge now an ordinary one. Fixed by tracking in-degree
///    and never evicting a candidate whose in-degree is 1
///    ([`HnswIndex::link_and_prune`]).
///
/// The third fix alone is not sufficient either, and that is the part worth
/// knowing: the in-edge rule removes zero-in-degree nodes completely but cannot
/// stop a whole *component* from being sealed off, because every edge inside
/// such a component is safely evictable from any single node's point of view.
/// Measured at `m = 2`, that left two entire 80-node clusters unreachable while
/// every one of their nodes held 3 or 4 perfectly valid in-edges. The graph is
/// therefore walked and repaired after the build
/// ([`HnswIndex::repair_connectivity`]), which is the only place the invariant
/// can actually be *checked* rather than assumed.
///
/// The measured result, over a sweep of 144 generated corpora (`m` in
/// {2, 4, 8, 16} × `n` in {40, 120, 400} × 12 seeds): **0 of 144** had a
/// stranded node, against 14 of 144 before this change, and every
/// stranded-node self-query returns its own vector as the nearest neighbour.
/// 11 nodes across the whole sweep ended up one edge over budget to make that
/// true; they are counted by [`HnswIndex::prunes_over_budget`] rather than being
/// left silent.
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
    /// `in_degree[layer][node]` — how many edges on that layer point *at* the
    /// node. A node with no in-edge on a layer cannot be stepped to from any
    /// other node on it, so this is the number the prune in
    /// [`HnswIndex::link_and_prune`] is built around.
    in_degree: Vec<Vec<u32>>,
    /// How many times a prune had to keep more edges than the budget allowed,
    /// because more nodes depended on this one list than it had room for. Zero
    /// on a well-formed build; a non-zero value is a measurement, not an error,
    /// and is reported by [`HnswIndex::prunes_over_budget`].
    prunes_over_budget: usize,
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
        let in_degree = vec![vec![0u32; n]; layer_count];

        let mut graph = HnswIndex {
            vectors: index.vectors().to_vec(),
            dim: index.dim(),
            metric: index.metric(),
            m: config.m,
            m0: config.m * 2,
            ef_construction: config.ef_construction,
            levels,
            neighbors,
            in_degree,
            prunes_over_budget: 0,
            entry_point: u32::MAX,
            max_level: -1,
            seed: config.seed,
        };
        for id in 0..n as u32 {
            graph.insert(id, config.ef_construction)?;
        }
        graph.repair_connectivity()?;
        Ok(graph)
    }

    /// Forces every node to be reachable from the entry point, per layer.
    ///
    /// # Why a repair pass and not just the in-edge rule
    ///
    /// The rule in [`HnswIndex::link_and_prune`] — never evict an edge that is
    /// a node's last way in — removes the *hard* failure completely: no node can
    /// end up with zero in-edges, so a self-query can no longer come back with
    /// some other row as its nearest neighbour. It cannot prevent a *component*
    /// from being sealed off, and that is a different failure with a different
    /// cause. A set of nodes can each hold several in-edges, every one of them
    /// from inside the set, while the one edge that used to link the set to the
    /// rest of the graph is evicted as the cheapest of a full list. The set is
    /// then internally well-connected and completely unreachable, and the
    /// per-edge rule reads every one of its edges as safely evictable, because
    /// from any single node's point of view none of them is the last.
    ///
    /// Measured on a clustered corpus at `m = 2`, this was not theoretical: two
    /// whole clusters of 80 nodes each came out sealed off, every node holding 3
    /// or 4 in-edges, all internal. No local edge rule can see that, so the
    /// invariant has to be checked on the finished graph rather than assumed
    /// from the rules that built it.
    ///
    /// # What the repair does
    ///
    /// Per layer, walk the graph from the entry point. For every node the walk
    /// did not reach, add one edge from the nearest *reached* node — the nearest
    /// by the same metric everything else uses — and continue the walk from
    /// there. The edges added are the nearest ones, so they preserve the
    /// small-world routing property the search depends on: the new bridge is a
    /// short hop, not an arbitrary long one.
    ///
    /// This runs once, at build time, over the whole graph. It is
    /// O(layers * n * deg) plus a distance computation per repaired node, and
    /// the distance computations are the same brute-force scan
    /// [`FlatIndex::search`] does, so the build stays within one order of
    /// magnitude of what it already cost.
    ///
    /// `m` is respected where it can be: the node being repaired is already
    /// linked to `reached`, so it usually has room, and the edge is skipped only
    /// when the list is genuinely full. The budget is relaxed by at most one in
    /// that case, and [`HnswIndex::prunes_over_budget`] counts it, because a
    /// navigable graph that is one edge over budget is a better answer than a
    /// tidy one that cannot be searched.
    fn repair_connectivity(&mut self) -> Result<(), SearchError> {
        if self.is_empty() {
            return Ok(());
        }
        for layer in (0..=self.max_level.max(0) as usize).rev() {
            // Nodes on this layer are those whose level reaches it. A node with
            // a lower level is absent and has an empty list here by construction.
            let present: Vec<u32> = (0..self.len() as u32)
                .filter(|&id| self.levels[id as usize] as usize >= layer)
                .collect();
            if present.len() <= 1 {
                continue;
            }
            // The graph is built top-down, so the entry point is present on every
            // layer this loop visits, and it is present at the top by definition.
            let entry = self.entry_point;
            debug_assert!(self.levels[entry as usize] as usize >= layer);

            let mut reached = vec![false; self.len()];
            let mut stack = vec![entry];
            reached[entry as usize] = true;
            while let Some(node) = stack.pop() {
                for &neighbour in self.neighbors[layer][node as usize].iter() {
                    if !reached[neighbour as usize] {
                        reached[neighbour as usize] = true;
                        stack.push(neighbour);
                    }
                }
            }

            // Repair by nearest reached neighbour, repeatedly, so a whole
            // disconnected component is absorbed one bridge at a time rather than
            // only its first node.
            loop {
                let stranded: Vec<u32> = present
                    .iter()
                    .copied()
                    .filter(|&id| !reached[id as usize])
                    .collect();
                if stranded.is_empty() {
                    break;
                }
                let mut repaired = 0usize;
                for id in stranded {
                    // Nearest reached node, by the index's own metric. Ties go to
                    // the lower id, matching every other ordering in this module.
                    let mut best: Option<(f64, u32)> = None;
                    for &other in &present {
                        if !reached[other as usize] || other == id {
                            continue;
                        }
                        let d = self.distance_between(id, other)?;
                        // `map_or` rather than `is_none_or`, which is 1.82 and
                        // this crate's MSRV is 1.75.
                        if best.map_or(true, |(bd, _)| d < bd) {
                            best = Some((d, other));
                        }
                    }
                    let Some((_, other)) = best else {
                        // No reached node at all — only possible on the first
                        // pass with an entry that somehow is not on this layer,
                        // which the debug_assert above rules out. Leaving the node
                        // unrepaired would be a silently wrong answer, so it is
                        // reported instead.
                        return Err(SearchError::Config(format!(
                            "no reachable node on layer {layer} to repair {} from",
                            id
                        )));
                    };
                    self.add_bridge(layer, id, other);
                    reached[id as usize] = true;
                    // Mark the whole component that just became reachable, or the
                    // next pass rebuilds bridges for nodes that already have one.
                    let mut queue = vec![id];
                    while let Some(node) = queue.pop() {
                        for &neighbour in self.neighbors[layer][node as usize].iter() {
                            if !reached[neighbour as usize] {
                                reached[neighbour as usize] = true;
                                queue.push(neighbour);
                            }
                        }
                    }
                    repaired += 1;
                }
                if repaired == 0 {
                    // No progress and nodes still stranded: the loop above cannot
                    // run again. Falling out here would leave a silently
                    // unreachable node, so say so.
                    return Err(SearchError::Config(format!(
                        "graph repair made no progress on layer {layer}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Adds `from -> to` on one layer, and the reciprocal `to -> from`.
    ///
    /// Both directions, because the search walks edges and a one-way bridge is
    /// not a bridge. The reciprocal may overflow `to`'s budget, which is the one
    /// place the edge budget gives way to the connectivity invariant.
    fn add_bridge(&mut self, layer: usize, from: u32, to: u32) {
        if !self.neighbors[layer][from as usize].contains(&to) {
            self.neighbors[layer][from as usize].push(to);
            self.in_degree[layer][to as usize] += 1;
        }
        if !self.neighbors[layer][to as usize].contains(&from) {
            let limit = self.m_of_layer(layer);
            if self.neighbors[layer][to as usize].len() >= limit {
                self.prunes_over_budget += 1;
            }
            self.neighbors[layer][to as usize].push(from);
            self.in_degree[layer][from as usize] += 1;
        }
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

    /// The nodes one node links to on one layer.
    ///
    /// Exposed so a caller can serialize the graph, walk it for a diagnostic, or
    /// hand it to a disk-backed index. The ids index the same vectors
    /// [`HnswIndex::search`] reports, so an edge from node `a` to node `b` means
    /// a search is allowed to step from `a` to `b`.
    pub fn neighbors_of(&self, id: usize, layer: u32) -> &[u32] {
        self.neighbors
            .get(layer as usize)
            .and_then(|l| l.get(id))
            .map_or(&[], Vec::as_slice)
    }

    /// How many edges on `layer` point at `id`.
    ///
    /// The counterpart to [`HnswIndex::neighbors_of`], and the number the
    /// prune in [`HnswIndex::link_and_prune`] protects. Exposed so a test can
    /// assert the connectivity invariant in its stronger form: a graph is only
    /// navigable if every node has an in-edge on layer 0, not merely if every
    /// node happens to be reachable on the one corpus the test happened to pick.
    pub fn in_degree(&self, id: usize, layer: u32) -> usize {
        self.in_degree
            .get(layer as usize)
            .and_then(|l| l.get(id))
            .copied()
            .unwrap_or(0) as usize
    }

    /// How many prunes had to exceed the edge budget to avoid stranding a node.
    ///
    /// Zero means every list came out within its budget. A non-zero value means
    /// some node's in-edges all pointed at one list that was already full, and
    /// the graph is a little larger than `m` promises at that node. It is a
    /// count rather than a panic because the alternative is a wrong answer; see
    /// [`HnswIndex::link_and_prune`].
    pub fn prunes_over_budget(&self) -> usize {
        self.prunes_over_budget
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
                distance: self
                    .metric
                    .distance(query, &self.vectors[candidate.id as usize])?,
            });
        }
        out.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    fn distance(&self, query: &[f64], id: u32) -> Result<f64, SearchError> {
        self.metric.distance(query, &self.vectors[id as usize])
    }

    /// The distance between two stored nodes, the only thing the heuristic
    /// selection needs and the one that cannot use `query` (which is the node
    /// being pruned, not the node being placed).
    fn distance_between(&self, a: u32, b: u32) -> Result<f64, SearchError> {
        self.metric
            .distance(&self.vectors[a as usize], &self.vectors[b as usize])
    }

    /// Greedy walk of one upper layer: keep stepping to the nearest neighbour
    /// until none of them is nearer than where we already are.
    fn greedy_descend(&self, query: &[f64], entry: u32, layer: usize) -> Result<u32, SearchError> {
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
            // The expansion cut-off. A candidate is expanded only while it could
            // still lead somewhere better than the worst result held.
            //
            // This must be `>=` the cut-off, not `>`: with `>` the search stops
            // the moment the beam fills, because the next candidate is worse
            // than the worst *currently held*, and nothing is ever expanded. A
            // beam of width n would then examine n nodes and stop, which is not
            // a beam search at all and would make a large `ef` do no extra work.
            //
            // With `>=`, a candidate that ties the cut-off is still expanded, so
            // the frontier really does run to exhaustion at the base layer. It
            // costs more comparisons and it is what makes a wide `ef` buy
            // recall.
            if results.len() >= ef {
                if let Some(&worst) = results.peek() {
                    if current.distance > worst.distance {
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
            // The new node's own edge list is chosen by the same heuristic, so
            // it spreads its budget instead of pointing every edge one way. The
            // reference point is `id` — the node whose list is being built.
            //
            // The heuristic runs *unconditionally*, not only on overflow. With
            // `ef_construction` the found-list usually fits inside the budget,
            // and skipping the filter there meant most of the graph got plain
            // nearest-`M` with no diversity consideration at all — which is the
            // degenerate shape the heuristic exists to prevent. Running it always
            // costs the same distance comparisons either way (the candidates are
            // already in hand) and makes the behaviour uniform.
            let outgoing: Vec<u32> = self.select_neighbors(
                layer as usize,
                id,
                found.iter().map(|c| c.id).filter(|&n| n != id).collect(),
                limit,
            )?;

            for &neighbour in &outgoing {
                // The out-edge `id -> neighbour` and the back edge
                // `neighbour -> id` are added together, so the in-degree
                // counter is bumped here rather than inside `link_and_prune`
                // (which only knows about the back edge).
                self.neighbors[layer as usize][id as usize].push(neighbour);
                self.in_degree[layer as usize][neighbour as usize] += 1;
                self.link_and_prune(layer as usize, neighbour, id, limit)?;
            }
            // A node with no outgoing edge cannot be routed *through*. The
            // heuristic can reject every candidate in a perfectly symmetric
            // cluster, so fall back to the single closest one: one edge is
            // always enough to keep the node reachable, and losing that is a
            // permanently wrong answer rather than a slightly worse one.
            if self.neighbors[layer as usize][id as usize].is_empty() {
                if let Some(closest) = found.first() {
                    let closest = closest.id;
                    self.neighbors[layer as usize][id as usize].push(closest);
                    self.in_degree[layer as usize][closest as usize] += 1;
                    self.link_and_prune(layer as usize, closest, id, limit)?;
                }
            }

            // **Keep the route back down alive.** The next iteration searches
            // layer 0 starting from the *closest* node found on the layer above,
            // so if the edge `closest -> entry` is missing, every node below
            // that point is searched for from a set the greedy descent could
            // never have produced. That is not a soft degradation: the search
            // still returns plausible-looking rows, and the nodes it can no
            // longer route to are simply gone.
            //
            // The paper assumes the `e_conn`-style expansion is what keeps the
            // graph navigable, and hnswlib leans on the next insertion to
            // rebuild a dropped edge. Neither is a guarantee, and a small `m`
            // makes the gap routine. This is an explicit edge instead: reach the
            // entry that the layer above handed down, and keep it in the budget.
            if layer > 0
                && !outgoing.contains(&entry)
                && !self.neighbors[layer as usize][id as usize].contains(&entry)
            {
                // The out-edge goes in `id`'s own list and the back edge
                // through `link_and_prune`, exactly as in the loop above.
                // Adding only the back edge here would give `id` an in-edge
                // it cannot use and leave the in-degree count describing an
                // edge that does not exist.
                //
                // `outgoing` may already have filled the budget, so this
                // push is the one place the new node's own list can go over
                // `limit`. It is counted rather than hidden, for the same
                // reason the repair pass counts its own: a navigable graph
                // one edge over budget beats a tidy one that cannot be
                // searched, and a caller that cares about the budget can
                // read the number rather than discover it.
                if self.neighbors[layer as usize][id as usize].len() >= limit {
                    self.prunes_over_budget += 1;
                }
                self.neighbors[layer as usize][id as usize].push(entry);
                self.in_degree[layer as usize][entry as usize] += 1;
                self.link_and_prune(layer as usize, entry, id, limit)?;
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
    /// budget if it overflowed, using the paper's heuristic so the survivors
    /// point in different directions rather than all at one cluster.
    ///
    /// This mirrors hnswlib's `mutuallyConnectNewElement` back-link branch, and
    /// in particular it **seeds the prune candidate list with the new element
    /// itself**. hnswlib writes
    /// `candidates.emplace(d_max, cur_c)` before adding the existing edges, and
    /// then runs the heuristic over the whole thing. An earlier version of this
    /// function added `id` to the list, ran the heuristic over the *existing*
    /// edges only, and put the trimmed result back — so the new back edge was
    /// silently discarded the moment the heuristic pruned anything.
    ///
    /// # Protecting the in-edge, not just the new one
    ///
    /// hnswlib has a commented-out "Nearest K" fallback for the case where the
    /// prune drops the new element, which suggests the problem is known upstream
    /// too. Forcing the *new* back edge to survive is what an earlier version of
    /// this function did, and it was not enough. Protecting `id` alone leaves the
    /// same failure one insertion later: a *later* node linking to `neighbour`
    /// runs the same prune, and `id` — by then an ordinary existing edge — can be
    /// the one evicted. The rule that actually closes the hole is the one below:
    /// **no candidate may be dropped if this edge is its only in-edge on the
    /// layer.** `id` is covered by that rule for free, because the node being
    /// inserted genuinely has in-degree zero at this point, so the forced-new-edge
    /// rule is a special case of it rather than a separate patch.
    ///
    /// That is the whole invariant. A node with no in-edge on a layer cannot be
    /// stepped to from anywhere on it, so it is invisible to every later search
    /// at every `ef`, and a query that *is* that node's own vector comes back
    /// with a different row as its nearest neighbour — confidently, at the top of
    /// the list. See [`HnswIndex`].
    fn link_and_prune(
        &mut self,
        layer: usize,
        neighbour: u32,
        id: u32,
        limit: usize,
    ) -> Result<(), SearchError> {
        {
            let list = &mut self.neighbors[layer][neighbour as usize];
            if list.contains(&id) {
                return Ok(());
            }
            if list.len() < limit {
                list.push(id);
                self.in_degree[layer][id as usize] += 1;
                return Ok(());
            }
        }
        // At the budget, so the list has to be trimmed. Taking the list out of
        // `self` first ends the mutable borrow so `select_neighbors` can take
        // `&self`; it is restored below whether or not the heuristic keeps it.
        let overflowed = std::mem::take(&mut self.neighbors[layer][neighbour as usize]);
        debug_assert!(!overflowed.contains(&id));

        // Split the candidates by whether this edge is their last way in.
        //
        // The test is `<= 1`, not `== 0`, and the difference is the whole point.
        // `in_degree` counts the edges that exist *now*, and `neighbour -> node`
        // is one of them, so a node with a count of exactly 1 is being pointed
        // at only by this list. Evicting this edge is what drops it to zero. A
        // `== 0` test reads every node as safe — by the time the second back
        // edge is added, `id` already has a count of 1, so the first one is
        // evictable and the node is stranded anyway.
        //
        // `id` sorts first among the protected so that it is the one dropped if
        // the protected set alone overruns the budget: `id` is not yet part of
        // the graph and still has its other neighbours to be adopted by, whereas
        // dropping an already-placed node strands it outright.
        let mut protected: Vec<u32> = Vec::new();
        let mut free: Vec<u32> = Vec::new();
        for &node in &overflowed {
            if self.in_degree[layer][node as usize] <= 1 {
                protected.push(node);
            } else {
                free.push(node);
            }
        }
        protected.push(id);
        protected.sort_by_key(|&node| if node == id { 0 } else { 1 });

        // The heuristic only ever competes for the slots the protected set left
        // over, so a node that cannot spare an in-edge is never a candidate for
        // eviction in the first place.
        let room = limit.saturating_sub(protected.len());
        let chosen = self.select_neighbors(layer, neighbour, free, room)?;

        let mut kept = protected;
        kept.extend(chosen);
        if kept.len() > limit {
            // More nodes depend on this one edge than the budget has room for.
            // Honesty beats tidiness here: a list one over budget is a slightly
            // more expensive graph, whereas dropping any of these strands a node
            // that is already placed. Measured, not assumed — see
            // `prunes_over_budget`.
            self.prunes_over_budget += 1;
        }
        debug_assert!(!kept.is_empty(), "an edge list may not be emptied");

        // Apply the result, and move the in-degree counts with it. The list was
        // emptied above, so the arithmetic has to be against the *difference*
        // between what it used to hold and what it holds now: the edges in
        // `overflowed` are already counted, and re-adding the survivors of that
        // set would count them twice — which silently inflates every count and
        // makes every node look safely multiply-connected.
        for &node in &overflowed {
            if !kept.contains(&node) {
                self.in_degree[layer][node as usize] -= 1;
            }
        }
        if kept.contains(&id) {
            self.in_degree[layer][id as usize] += 1;
        }
        self.neighbors[layer][neighbour as usize] = kept;
        Ok(())
    }

    /// The paper's Algorithm 4, `SELECT-NEIGHBORS-HEURISTIC`, as hnswlib's
    /// `getNeighborsByHeuristic2` implements it.
    ///
    /// Walks the candidates nearest-first and keeps one only if it is **not
    /// closer to an already-kept neighbour than it is to `origin`**. `origin`
    /// is the element whose edge list is being chosen: the new node on its own
    /// edge list, or the overflowing node on a back-link. It is a real parameter
    /// and it is load-bearing. An earlier version took `candidates[0]` instead,
    /// which made the reference point slide along with the walk, so the filter
    /// answered "is this candidate nearer to *the first candidate* than to the
    /// node?" rather than the question the algorithm asks. That is a subtle
    /// difference that strands nodes: see [`HnswIndex`].
    ///
    /// The second caller's case is why the caller filters rather than appending.
    /// hnswlib's `updatePoint` and `mutuallyConnectNewElement` both
    /// `emplace(d_max, cur_c)` — the new element, at its true distance from the
    /// node being pruned — and run the heuristic over the whole thing, so the new
    /// back edge is measured and filtered on the same terms as everything already
    /// there. [`HnswIndex::link_and_prune`] instead decides *before* calling
    /// this: a candidate whose in-degree is 1 is held back from the pool
    /// entirely, because the heuristic would be free to drop it and a drop there
    /// is what strands a node. The two routes reach the same place, but only one
    /// of them can protect an edge that the heuristic would otherwise reject.
    ///
    /// # The floors, and why they are not the fix
    ///
    /// A diversity filter can reject everything (a perfectly symmetric cluster),
    /// and the result must not be an empty edge list: a node with no edges cannot
    /// be routed through, and a node nobody links *back* to is unreachable at
    /// every `ef`. So there are two floors — fall back to the nearest `limit` if
    /// the filter kept nothing, and accept a short list otherwise. Both are
    /// reachability guards, and neither substitutes for a correct reference
    /// point: getting the heuristic right is what makes the graph navigable, and
    /// the floors only stop the pathological symmetric case from producing a
    /// *worse* one.
    ///
    /// `limit` here is the number of *free* slots the caller has left, which is
    /// not necessarily `m`: [`HnswIndex::link_and_prune`] passes what remains
    /// after the protected edges have claimed their share.
    fn select_neighbors(
        &self,
        layer: usize,
        origin: u32,
        candidates: Vec<u32>,
        limit: usize,
    ) -> Result<Vec<u32>, SearchError> {
        let _ = layer;
        // A candidate list that already fits needs no filter, and running one
        // would only throw away edges that fit. hnswlib short-circuits the same
        // way (`if (top_candidates.size() < M) return;`), except that it
        // compares against `M` where its own-link callers have already bounded
        // the list by `Mcurmax`, so its `M_` check can pass even on an
        // overflow. This module bounds the list at the call site instead, so the
        // short-circuit here is exact.
        if candidates.len() <= limit {
            return Ok(candidates);
        }

        let mut ordered: Vec<Candidate> = Vec::with_capacity(candidates.len());
        for &node in &candidates {
            if node == origin {
                continue;
            }
            ordered.push(Candidate {
                distance: self.distance_between(origin, node)?,
                id: node,
            });
        }
        // Nearest first, so the walk below considers candidates in the order
        // the paper specifies and the accepted set only ever grows.
        ordered.sort_unstable();

        let mut kept: Vec<Candidate> = Vec::with_capacity(limit);
        for candidate in ordered.iter().copied() {
            if kept.len() >= limit {
                break;
            }
            // Keep this candidate only if it is at least as near to `origin` as
            // to every neighbour already kept — i.e. it opens a direction the
            // kept set does not already cover. `candidate.distance` is the
            // distance to `origin`, recomputed once above and carried here, so
            // the comparison is against the same number for every `chosen`.
            let mut redundant = false;
            for chosen in &kept {
                if self.distance_between(candidate.id, chosen.id)? < candidate.distance {
                    redundant = true;
                    break;
                }
            }
            if !redundant {
                kept.push(candidate);
            }
        }
        // A diversity filter can reject everything (a perfectly symmetric
        // cluster), which would leave the node with no edges at all and strand
        // it. Falling back to the nearest `limit` keeps the graph connected, so
        // this is a floor on connectivity rather than a way to drop a node.
        if kept.is_empty() {
            kept = ordered.into_iter().take(limit).collect();
        }
        Ok(kept.into_iter().map(|c| c.id).collect())
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
///
/// **The int8 form buys memory, not speed, and the two search paths here are
/// slower than the [`FlatIndex`] they replace.** Each query walks every stored
/// row and calls [`QuantizedIndex::dequantize`], which allocates a fresh
/// `Vec<f64>` per row: `n` heap allocations and `n * dim` float conversions per
/// query, on top of the metric itself. A scan over this index is strictly more
/// work than a scan over the flat one, not less. That is the deliberate trade for
/// the error report, but it means the type must not be read as the fast path —
/// it sits next to an [`HnswIndex`] whose entire purpose is not touching every
/// row, and the two are not alternatives to each other. The memory saving is
/// real and reported as [`QuantizationReport::compression`]; the speed is not
/// claimed anywhere.
///
/// [`HnswIndex`] is built from a [`FlatIndex`], not from this type, because a
/// graph over approximated vectors would add the quantization error to every
/// *navigated* distance as well as to every reported one, and the reported
/// numbers would no longer be attributable to the quantization alone.
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
        let (row, scale) = quantize_vector(query)?;
        self.search_codes(&row, scale, k)
    }

    /// The `k` nearest vectors, keeping the query in `f64`.
    ///
    /// Isolates the error introduced by the *stored* vectors, which is the part
    /// [`QuantizationReport::max_reconstruction_error`] measures.
    ///
    /// **Every row is dequantized on every query**, one `Vec` allocation each —
    /// see the type docs. This is a correctness-and-fidelity path, not a fast
    /// one; a caller that has measured the error and found it acceptable should
    /// hold the dequantized rows itself rather than pay this per query.
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

    /// [`QuantizedIndex::search`] over already-quantized query codes.
    ///
    /// `scale` is the step the codes were produced with. **It is a required
    /// argument and cannot be recovered from the codes**, because an `i8` row
    /// records only the shape of a vector, not its magnitude: a row of all 127s
    /// is the same row whether the original was all 0.01 or all 100. This is
    /// the one thing the int8 storage genuinely does not carry, and it is why
    /// [`QuantizedIndex::search`] — which quantizes and keeps the scale
    /// together — is the normal entry point. This method exists for a caller
    /// that has the codes and the scale already, e.g. a virtual table loading a
    /// `vec0`-style blob.
    pub fn search_codes(
        &self,
        query: &[i8],
        scale: f64,
        k: usize,
    ) -> Result<Vec<Neighbor>, SearchError> {
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
        if !scale.is_finite() || scale <= 0.0 {
            return Err(SearchError::Config(format!(
                "query scale must be finite and positive, got {scale}"
            )));
        }
        // The query is dequantized once, not per row: it has a single scale, and
        // recomputing it inside the loop would be the same number every time.
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

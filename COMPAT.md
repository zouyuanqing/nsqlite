# nsqlite compatibility with SQLite

`nsqlite` is a from-scratch embedded SQL engine that reads and writes SQLite's
own file format. This document records where it agrees with SQLite, where it
does not, and how to re-measure every claim in it.

It tracks **SQLite 3.53.4**. That is the version of `sqlite3` the differential
corpora and the official TCL suite are run against, and the version
`tools/fetch_sqlite_suite.sh` fetches and verifies against the Fossil manifest.

**Every number here was produced by a command, on the date it names.** A claim
without a command that produced it does not belong in this file. Where a
statement is a fact about the code rather than a measurement, it cites the
line instead.

## Guarantees

1. You can always go back to SQLite. A `.db` file `nsqlite` writes is readable
   by `sqlite3`, and `PRAGMA integrity_check` on it returns `ok`.
2. You can read a database SQLite wrote. `tests/interop.rs` reads fixtures that
   the real shell produced, including negative rowids, every storage class and
   rows that spill into overflow pages.
3. A divergence is documented here rather than discovered by a caller.
4. `nsqlite` is single-writer. It does not support multi-process access to one
   file, which is also the one place `nsqlite` is more restrictive than SQLite.

## Reproducing this file

```sh
# the reference this file is measured against
sqlite3 --version                      # 3.53.4

# the differential corpora, against that reference
DIFFTEST_CASES=150 cargo test -p nsqlite --test differential

# the official TCL suite
tools/fetch_sqlite_suite.sh && tools/setup_tclsuite.sh && tools/run_suite.sh
```

The differential test refuses to run against a `nsqlited` older than the
sources that built it, and says so rather than reporting on a stale build. See
`crates/nsqlite/tests/differential.rs`.

## Known defects

Everything in this section was measured on **2026-09-30** against SQLite
3.53.4. Each entry gives the reproduction.

### Silent row loss in a multi-row `VALUES` insert

The most serious thing in this file. A single `INSERT` whose value list is long
enough to force a leaf page split writes only part of its rows, **and reports
the full count it was given**. Nothing is printed but the count, the exit code
is zero, and the rows are absent from the file.

```sh
nsqlited x.db "CREATE TABLE t(a INTEGER, b TEXT);"
nsqlited x.db "INSERT INTO t VALUES (1,'r1'),(2,'r2'),...;"   # 800 tuples
nsqlited x.db "SELECT count(*) FROM t;"                        # -> 323, not 800
sqlite3  x.db "SELECT count(*) FROM t;"                        # -> 323. The rows are gone.
```

| tuples | the engine reports | rows actually in the table |
|--------:|-------------------:|---------------------------:|
| 470 | 470 | 470 |
| **480** | 480 | **321** |
| 500 | 500 | 341 |
| 600 | 600 | 441 |
| 800 | 800 | 323 |

The cliff is where the write crosses a page boundary, so the loss is in the
split path rather than in the value list itself, which is consistent with the
schema defect fixed in b8b7210. What is *not* consistent, and is still open:
separate statements are correct — 500 of them insert 500 rows, and five
statements of 100 tuples insert 500 rows — so the loss is specific to one
statement's value list.

Real SQLite is unaffected. It is worth knowing that a statement above roughly
32,000 characters never reaches either program at all on Windows, because that
is the command line length limit; both shells insert nothing and say nothing.
That is the environment, not a defect, and it is why the threshold above is
reached with short column names.

### `CHECK` constraints are parsed, stored, and never evaluated

The DDL is correct and the constraint survives a round trip — reading
`sqlite_schema` back gives the `CREATE TABLE` text verbatim — but no statement
ever tests it.

```sql
CREATE TABLE t(a INT, b TEXT, c INT CHECK(length(c) <= 5));
INSERT INTO t(a,b,c) VALUES(1,'x',999);   -- sqlite3: CHECK constraint failed
SELECT count(*) FROM t;                   -- nsqlited: 1.  sqlite3: 0.
```

`UPDATE` is equally unchecked. This is the first disagreement the fifth
differential corpus reports.

## Deliberate divergences

### `EXPLAIN` returns its column names and no rows

```sql
EXPLAIN SELECT 1;   -- nsqlited: 8 column names, zero rows
```

There is no VDBE in this engine, so there is no program to list. A fabricated
opcode sequence would describe a machine that does not exist. `EXPLAIN QUERY
PLAN` is real and does produce a plan; only the opcode listing is empty. The
argument, including the two rejected alternatives, is in the module docs at
`crates/nsqlite/src/explain.rs`.

### Text is not restricted to UTF-8

`Value` carries either a `String` or an opaque byte string, so a text value may
hold bytes that are not valid UTF-8, as it may in SQLite.

```sql
SELECT hex(CAST(x'96' AS TEXT));   -- nsqlited: 96     sqlite3: 96
```

Binding a non-UTF-8 string through the C API is the narrower case that does
diverge: it is replaced with U+FFFD and still returns `SQLITE_OK`, because
`sqlite3_bind_text` takes a `const char *` and has nowhere to put a length this
shim can honour.

## Not implemented

| Feature | Measured as |
|---|---|
| A subquery in `FROM` | `a subquery in FROM is not supported yet` |
| FTS3/4/5 | `no such module: fts5` |
| Loadable extensions | `no such function: load_extension` |
| `savepoint`, nested `BEGIN` | `BEGIN` is a two-state flag; there is no nesting |
| Concurrency | every entry point takes `&mut self`; no WAL, no locking |

`UNION`, `UNION ALL`, `EXCEPT` and `INTERSECT` **are** implemented and match
SQLite; an earlier revision of `docs/testing.md` listed `UNION` as open, and
that line is stale.

## Where this engine is behind

### An index is maintained but not used to answer a query

This is the largest gap and it is a performance one, not a correctness one.
`CREATE INDEX` writes a real b-tree, `INSERT`/`UPDATE`/`DELETE` maintain it,
and `UNIQUE` is enforced from it. But a query always reads the table.

```sh
# 20000 rows, an index on a, and a query that matches one row
nsqlited x.db "EXPLAIN QUERY PLAN SELECT count(*) FROM t WHERE a = 19999;"
#   SEARCH t USING COVERING INDEX ix (a=?)
# the plan names the index. The engine then scans the table anyway:
#   with the index:    17 ms
#   without the index: 14 ms
```

The plan is computed from the catalog and is correct as a plan; the executor
does not consume it. `crates/nsqlite/src/connection.rs:3017` is the line.

### Float text rendering

```sql
SELECT 1.0/3.0;   -- nsqlited: 0.3333333333333333
                  -- sqlite3:  0.33333333333333332
```

SQLite 3.53 renders the shortest form that round-trips. `quote()` agrees,
because both sides quote the same rounded value, so the differential corpora
see this only where text is produced by concatenation rather than by `quote()`.

### The official TCL suite

As recorded in `docs/testing.md` section 5.2, against the suite fetched at
tag `version-3.53.4`. These are the project's own numbers, not re-measured
for this document; `tools/run_suite.sh` is the command that produces them.

| | |
|---|---:|
| files in the suite | 1,190 |
| files passing | 29 |
| files reaching at least one test | 524 (24,192 cases) |
| files skipped whole, behind a false capability | 544 |
| files aborting before their first test | 117 |

A skipped file is not a pass. `tools/run_suite.sh` reports `SKIPPED` and exits
non-zero for one, because the suite prints `0 errors out of 0 tests` and a
harness that counted that as success would be measuring nothing.

The five differential corpora are the other half of the evidence, and they are
in much better shape: the fifth, at 341 statements, reports 336 agreements and
no wrong answers, and this document's defects are the four that remain.

### The C API

46 symbols, which is the subset the official suite's shim needs: open and
close, prepare, step, bind, column, error, and the change counters.
Deliberately absent, and declared out of scope at
`crates/nsqlite-capi/src/lib.rs:109`: user-defined functions, collations, the
backup API, `load_extension`, the authorizer, progress and trace hooks,
`sqlite3_value` and `sqlite3_context`, the blob and VFS interfaces, and
serialization.

`sqlite3_column_decltype` and `sqlite3_column_table_origin` return NULL rather
than being missing, so a caller links and then reads nothing.

## What this engine does that SQLite does not

Nothing yet. Every divergence above costs SQLite compatibility and none buys
anything back yet. The `vec0` virtual table is the intended exception: it is
modelled on `fts5` and `rtree` rather than invented, and `crates/nsqlited/src/vec0.rs`
holds the adapter. Its own contract, including the parts the engine does not
implement yet, is `crates/nsqlite/src/vec0_bridge.rs`.

## Coverage of the TCL suite, and why it is low

The suite is not a uniform measure of engine quality, and reading the 29 as
"29 percent of SQLite" overstates how little works. Of the 544 files skipped
whole, 125 are gated on `vtab` and nothing else, but a large share of those are
gated on testfixture modules — `echo` 55 files, `tcl` 48, `fuzzer` 38,
`unionvtab` 26, `csv` 12 — which the reference binary used for the comparison
does not have either. Those are harness limits, and closing them would not
change one line of the engine.

The remaining gap is real and is tracked in `docs/testing.md` section 5.3,
which is the engineering roadmap: 14 items, of which the last measurement had
9 closed, 2 partly done and 2 open.

# Testing nsqlite against the official SQLite TCL suite

This document covers where the suite comes from, the two ways to wire it to an
engine, the capability gate that decides what runs, a first milestone list, and
what pass rate to actually expect.

Tooling lives in `tools/`:

| File | What it does |
| --- | --- |
| `tools/fetch_sqlite_suite.sh` | Downloads the pinned suite, verifies it |
| `tools/run_suite.sh` | Runs a subset against a Tcl interpreter |
| `tools/capabilities.tcl` | The `$::sqlite_options` array for a minimal build |

Quick start:

```sh
tools/fetch_sqlite_suite.sh        # ~13 MB, verified against the manifest
tools/run_suite.sh 'select1.test'  # exits 3: no interpreter yet (see below)
```

---

## 1. Where the suite comes from

The suite is the Tcl regression test suite from the official SQLite source
tree. The canonical copy is a Fossil repository at `sqlite.org`, mirrored to
GitHub as [`sqlite/sqlite`](https://github.com/sqlite/sqlite). We take the
mirror, because a pinned tag there is a plain tarball over HTTPS and needs no
Fossil.

`tools/fetch_sqlite_suite.sh` downloads:

```
https://codeload.github.com/sqlite/sqlite/tar.gz/refs/tags/version-3.53.4
```

Pinned to the tag `version-3.53.4`, never `master`. A tag is immutable, so the
suite cannot shift underneath a test run; that is the whole reason for pinning.

Three things are extracted, and nothing else:

```
test/sqlite-suite/test/      1190 .test files + 31 .tcl harness files
test/sqlite-suite/manifest   fossil hash manifest
test/sqlite-suite/VERSION    the release version string
```

`src/` is deliberately **not** extracted. The suite tests an engine through a
Tcl binding, not by building SQLite, and shipping the C sources alongside it
only invites someone to try to build the reference engine by accident.

### Verification

The `manifest` is a Fossil hash manifest covering the whole repository, and it
uses **two** hash algorithms. Files predating Fossil's 2022 switch to SHA3-256
carry a 40-hex SHA-1; newer ones carry 64-hex SHA3-256. In the 3.53.4 `test/`
tree that is 416 SHA-1 entries and 869 SHA3-256 entries, so the verifier picks
the algorithm by hash length:

```
416  40  -> sha1
869  64  -> sha3-256
```

`openssl dgst` handles both (`sha3sum` is not present on this machine). All
1283 files under `test/` plus `VERSION` are checked, and the fetch stamps
`.fetched-version` only after they all pass. A stamp therefore always means
"this tree hashes clean".

This is verified end to end, not assumed:

```
$ tools/fetch_sqlite_suite.sh
fetching sqlite/sqlite tag version-3.53.4
  downloaded 13163206 bytes
  verified 1283 files against manifest (SHA-1 + SHA3-256)

sqlite suite 3.53.4 installed at .../test/sqlite-suite
  1190 .test files
  31 .tcl harness files
```

The script is idempotent: a second run verifies and exits without downloading.
`--verify` checks without downloading, `--force` discards and re-fetches.

### Layout

```
test/sqlite-suite/test/
  tester.tcl          2626 lines. The harness: db helpers, do_test, ifcapable.
  testrunner.tcl      1968 lines. Job scheduling, permutations, --jobs.
  permutations.test   Defines the test suites (veryquick, quick, full, ...).
  test_config.c       NOT extracted. Defines sqlite_options at build time.
  *.test              1190 test files.
```

One detail worth knowing: every `.test` file derives its own working directory
from its own path:

```tcl
set testdir [file dirname $argv0]
source $testdir/tester.tcl
```

All 1190 of them do this. So the directory is implicit in the invocation, and
`TESTDIR` in the environment is **not** how the suite finds its files. It is
set by `run_suite.sh` anyway, because the harness and several `.test` files read
it, but the positional path is what actually matters.

---

## 2. The two harness strategies

The suite never talks to the engine over a subprocess. `tester.tcl` calls Tcl
commands like `sqlite3_initialize` and `install_malloc_faultsim` before the
first test executes. Something has to provide those. There are two sane ways.

### (a) Tcl extension exposing the C API

Build a loadable Tcl extension that binds the SQLite C API and registers a
`sqlite3` command. The stock harness then runs unmodified, with full access to
per-statement state, `sqlite3_stmt` handles, and the test-only commands.

**What must exist in the engine first:**

- The whole C API surface `tester.tcl` uses — about 40 commands. Not just the
  public ones. The list includes `sqlite3_exec_nr`, `sqlite3_expanded_sql`,
  `sqlite3_connection_pointer`, `sqlite3_column_text`, `sqlite3_errcode`,
  `sqlite3_extended_errcode`, `sqlite3_next_stmt`, `sqlite3_finalize`,
  `sqlite3_config`, `sqlite3_config_memstatus`, `sqlite3_status`.
- **The test-only commands, which are the hard part.** A stock `sqlite3` Tcl
  extension is *not enough*. `tester.tcl` line 102 calls
  `sqlite3_test_control_pending_byte`, which does not exist in a normal build;
  it comes from `SQLITE_TESTCTRL_PENDING_BYTE`, a test-only build flag. Also
  needed: `install_malloc_faultsim`, `autoinstall_test_functions`,
  `unregister_devsim`, `unregister_jt_vfs`, `unregister_demovfs`, `vfslog`,
  `vdbe_coverage`. These are a fault-injection and devsim layer, not an API.
- A fault-injection malloc. `install_malloc_faultsim` lets the suite inject OOM
  at allocation N and verify the engine survives. A from-scratch engine needs a
  real allocator seam for this, and it must report memory accounting.
- The test VFSs: `devsim`, `jt` (jailer), `demo`, `crash`, `nul`, `memdb`.
- Per-connection handles as first-class Tcl objects, so `sqlite3_finalize` can
  be called on a specific statement.

**What is permanently lost:**

- The ability to run the suite **unmodified**. The further along the engine
  gets, the more it will diverge from the C API's shape — the C API leaks its
  own design decisions (a `sqlite3_stmt*`, a row-id, a column index). Binding
  that faithfully is a second implementation of the C API in Tcl bindings, and
  the tests it enables are mostly about C-level behaviour (refcounting,
  `sqlite3_reset` vs `sqlite3_clear_bindings`, error-code precision) that a Rust
  design may not have a concept for at all.
- The fault-injection and devsim work is pure test scaffolding. It is real work
  and it is not reusable by anything else.

**This is the strategy that keeps the suite honest**, because it is the only one
where the `.test` files are the unmodified originals. It is also the most
expensive. It requires the engine to have a C-compatible API surface, which
constrains the engine's design from the start.

### (b) A `tester.tcl` shim driving a CLI binary

Replace the harness with a shim that implements the same command vocabulary by
shelling out to a CLI binary over `exec`. This is the approach Turso takes.

Confirmed in Turso's tree, `testing/system/tester.tcl`:

```tcl
set sqlite_exec [expr {[info exists env(SQLITE_EXEC)] ? $env(SQLITE_EXEC) : "sqlite3"}]
...
proc evaluate_sql {sqlite_exec db_name sql} {
    ...
    append load_commands ".load $extensions($name)\n"
```

It runs the engine as a subprocess, feeds it SQL, and compares output. There is
also a vendored copy of the upstream harness at
`sqlite/conformance/upstream/tester.tcl`, and a `tcl_converter` under
`testing/sqltest/src/` that converts TCL tests for the sqltest runner.

**What must exist in the engine first:**

- A CLI that reads SQL and writes results in a parseable format. This is a much
  smaller ask than a Tcl extension: a line-oriented mode is enough, and most
  engines get one for free.
- Error messages that match SQLite's text exactly. This is the real cost. The
  suite compares error strings as expected values — `select1-1.1` expects
  `{no such table: test1}` verbatim — so a shim cannot paper over differences
  the way a C API binding can.
- Correct handling of process exit and partial output. Anything that crashes
  mid-statement has to be distinguishable from a clean error.

**What is permanently lost:**

- **Everything that needs in-process access to engine internals.** The
  fault-injection suite (`*malloc*`, `*fault*` — the bulk of `crash*.test` and
  friends) cannot work, because the shim has no way to make the engine fail an
  allocation on demand. Neither can anything needing `sqlite3_stmt` handles,
  test-control, or a specific VFS.
- Per-permutation PRAGMA and compile-option variation, which the stock harness
  applies in-process.
- Exact per-statement status and row-count checks where the CLI flattens them.

**The trade-off in one line:** (b) gets the `.test` files running sooner and
with far less engine surface area, and permanently forfeits the fault-injection
and internal-state half of the suite. (a) costs much more up front and keeps
the option of running the suite verbatim.

A reasonable order is: start with (b) to get real signal on SQL semantics early,
and treat (a) as the goal if the fault-injection tests are ever going to run.

---

## 3. The capability gate

### How it works

Every `.test` file decides what to run by reading the global
`$::sqlite_options` array, which the engine's test fixture populates at
startup. It is read two ways.

**Directly:**

```tcl
if {0==$::sqlite_options(memdebug)} { ... }
set AUTOVACUUM $sqlite_options(default_autovacuum)
```

**Through `ifcapable`, which rewrites the expression.** `tester.tcl` line 1697:

```tcl
proc fix_ifcapable_expr {expr} {
  set ret ""
  set state 0
  for {set i 0} {$i < [string length $expr]} {incr i} {
    set char [string range $expr $i $i]
    set newstate [expr {[string is alnum $char] || $char eq "_"}]
    if {$newstate && !$state} {
      append ret {$::sqlite_options(}
    }
    if {!$newstate && $state} {
      append ret )
    }
    append ret $char
    set state $newstate
  }
  if {$state} {append ret )}
  return $ret
}
```

It is a character scanner, not a parser. It walks the expression and, at every
run of alphanumerics and underscores, wraps that run in `$::sqlite_options(...)`
and a closing paren. The `#regsub` on the line below is the original one-liner
form, kept for reference.

So this in a test file:

```tcl
ifcapable  fts5 { ... }
ifcapable !default_autovacuum { ... }
ifcapable  fts3 || json1 { ... }
```

becomes, before `expr` ever sees it:

```tcl
if {$::sqlite_options(fts5)} { ... }
if {!$::sqlite_options(default_autovacuum)} { ... }
if {$::sqlite_options(fts3) || $::sqlite_options(json1)} { ... }
```

The `!` and the `||` are untouched. The scanner only wraps the bare words.

Two consequences follow, and both are load-bearing:

- **`capable` and `ifcapable` evaluate the rewritten string with `expr`.** The
  variable must exist and hold something boolean-evaluable. An unset entry
  produces `can't read "::sqlite_options(fts5)": no such variable`, which kills
  the test file. It does **not** produce a skip.
- Nothing in the suite ever sets a missing entry. So an engine that defines only
  the entries it likes will crash rather than skip on the ones it skipped.

### The 128 entries

The array is defined by SQLite's test fixture in `src/test_config.c`, in
`set_options()`. It has **128** entries at tag `version-3.53.4`. Of these, 105
are referenced directly by `.test` files through `ifcapable`/`capable`; the
remaining 23 are read only as plain array variables, or only by `tester.tcl`
itself.

(Counting this needs care. `ifcapable` accepts `ifcapable fts5`, `ifcapable
!fts5` and `ifcapable {fts5 && ...}`, so a regex has to allow an optional `!`
and brace. A first pass that assumed names begin with a letter silently dropped
`8_3_names`; a second that allowed digits picked up `database`, `of` and
`shared_chache` from prose and a comment typo in `e_uri.test`. The 105 figure is
from a pattern that matches all three call forms and discards non-option tokens
by checking each against the 128.)

`tools/capabilities.tcl` defines all 128 and asserts the count at source time,
so a drift from the upstream list fails immediately rather than 200 files deep:

```tcl
namespace eval ::nsqlite {
    variable capability_count [array size ::sqlite_options]
    if {$capability_count != 128} {
        return -code error "sqlite_options has $capability_count entries, expected 128. ..."
    }
}
```

The full list was extracted mechanically from `test_config.c` at the pinned tag
and diffed against `tools/capabilities.tcl`; the two sets are identical.

One trap worth flagging, since it bit this extraction: `8_3_names` starts with a
digit. A regex that assumes names begin with a letter or underscore silently
drops it, and the resulting 127-entry list looks entirely plausible. It is
referenced by five test files — `8_3_names.test`, `delete_db.test`,
`mjournal.test`, `multiplex.test` and `multiplex3.test` — which would then die
on a missing entry.

### Feature macros per entry

Each entry is a `#ifdef` around a `Tcl_SetVar2` call, so the macro that drives
it is the one in that `#ifdef`. The `SQLITE_OMIT_*` macros are the negative
form: the entry is 1 unless the macro says to leave it out.

**`SQLITE_OMIT_*` — the core SQL language:**

| Option | Macro |
| --- | --- |
| `altertable` | `SQLITE_OMIT_ALTERTABLE` |
| `analyze` | `SQLITE_OMIT_ANALYZE` |
| `attach` | `SQLITE_OMIT_ATTACH` |
| `auth` | `SQLITE_OMIT_AUTHORIZATION` |
| `autoinc` | `SQLITE_OMIT_AUTOINCREMENT` |
| `autoindex` | `SQLITE_OMIT_AUTOMATIC_INDEX` |
| `autoreset` | `SQLITE_OMIT_AUTORESET` |
| `autovacuum` | `SQLITE_OMIT_AUTOVACUUM` |
| `between_opt` | `SQLITE_OMIT_BETWEEN_OPTIMIZATION` |
| `bloblit` | `SQLITE_OMIT_BLOB_LITERAL` |
| `cast` | `SQLITE_OMIT_CAST` |
| `check` | `SQLITE_OMIT_CHECK` |
| `compound` | `SQLITE_OMIT_COMPOUND_SELECT` |
| `complete` | `SQLITE_OMIT_COMPLETE` |
| `cte` | `SQLITE_OMIT_CTE` |
| `decltype` | `SQLITE_OMIT_DECLTYPE` |
| `deprecated` | `SQLITE_OMIT_DEPRECATED` |
| `deserialize` | `SQLITE_OMIT_DESERIALIZE` |
| `diskio` | `SQLITE_OMIT_DISKIO` |
| `explain` | `SQLITE_OMIT_EXPLAIN` |
| `floatingpoint` | `SQLITE_OMIT_FLOATING_POINT` |
| `foreignkey` | `SQLITE_OMIT_FOREIGN_KEY` |
| `gettable` | `SQLITE_OMIT_GET_TABLE` |
| `incrblob` | `SQLITE_OMIT_INCRBLOB` |
| `json1` | `SQLITE_OMIT_JSON` |
| `like_opt` | `SQLITE_OMIT_LIKE_OPTIMIZATION` |
| `load_ext` | `SQLITE_OMIT_LOAD_EXTENSION` |
| `localtime` | `SQLITE_OMIT_LOCALTIME` |
| `lookaside` | `SQLITE_OMIT_LOOKASIDE` |
| `memorydb` | `SQLITE_OMIT_MEMORYDB` |
| `or_opt` | `SQLITE_OMIT_OR_OPTIMIZATION` |
| `pager_pragmas` | `SQLITE_OMIT_PAGER_PRAGMAS` |
| `pragma` | `SQLITE_OMIT_PRAGMA` |
| `progress` | `SQLITE_OMIT_PROGRESS_CALLBACK` |
| `reindex` | `SQLITE_OMIT_REINDEX` |
| `schema_pragmas` | `SQLITE_OMIT_SCHEMA_PRAGMAS` |
| `schema_version` | `SQLITE_OMIT_SCHEMA_VERSION_PRAGMAS` |
| `shared_cache` | `SQLITE_OMIT_SHARED_CACHE` |
| `subquery` | `SQLITE_OMIT_SUBQUERY` |
| `tclvar` | `SQLITE_OMIT_TCL_VARIABLE` |
| `trace` | `SQLITE_OMIT_TRACE` |
| `trigger` | `SQLITE_OMIT_TRIGGER` |
| `truncate_opt` | `SQLITE_OMIT_TRUNCATE_OPTIMIZATION` |
| `utf16` | `SQLITE_OMIT_UTF16` |
| `vacuum` | `SQLITE_OMIT_VACUUM` |
| `view` | `SQLITE_OMIT_VIEW` |
| `vtab` | `SQLITE_OMIT_VIRTUALTABLE` |
| `wal` | `SQLITE_OMIT_WAL` |
| `windowfunc` | `SQLITE_OMIT_WINDOWFUNC` |
| `wsd` | `SQLITE_OMIT_WSD` |

**`SQLITE_ENABLE_*` — optional modules and extensions:**

| Option | Macro |
| --- | --- |
| `api_armor` | `SQLITE_ENABLE_API_ARMOR` |
| `atomicwrite` | `SQLITE_ENABLE_ATOMIC_WRITE` |
| `8_3_names` | `SQLITE_ENABLE_8_3_NAMES` |
| `carray` | `SQLITE_ENABLE_CARRAY` |
| `columnmetadata` | `SQLITE_ENABLE_COLUMN_METADATA` |
| `cursorhints` | `SQLITE_ENABLE_CURSOR_HINTS` |
| `fts3` | `SQLITE_ENABLE_FTS3` |
| `fts3_unicode` | `SQLITE_ENABLE_FTS3` (unicode61 tokenizer) |
| `fts4_deferred` | `SQLITE_DISABLE_FTS4_DEFERRED` (inverted sense) |
| `fts5` | `SQLITE_ENABLE_FTS5` |
| `geopoly` | `SQLITE_ENABLE_GEOPOLY` |
| `hiddencolumns` | `SQLITE_ENABLE_HIDDEN_COLUMNS` |
| `icu` | `SQLITE_ENABLE_ICU` |
| `icu_collations` | `SQLITE_ENABLE_ICU_COLLATIONS` |
| `mathlib` | `SQLITE_ENABLE_MATH_FUNCTIONS` |
| `memorymanage` | `SQLITE_ENABLE_MEMORY_MANAGEMENT` |
| `normalize` | `SQLITE_ENABLE_NORMALIZE` |
| `null_trim` | `SQLITE_ENABLE_NULL_TRIM` |
| `offset_sql_func` | `SQLITE_ENABLE_OFFSET_SQL_FUNC` |
| `ordered_set_aggregates` | `SQLITE_ENABLE_ORDERED_SET_AGGREGATES` |
| `ordered_set_funcs` | `SQLITE_ENABLE_ORDEREDSETFUNC` |
| `oversize_cell_check` | `SQLITE_ENABLE_OVERSIZE_CELL_CHECK` |
| `preupdate` | `SQLITE_ENABLE_PREUPDATE_HOOK` |
| `rbu` | `SQLITE_ENABLE_RBU` |
| `rtree` | `SQLITE_ENABLE_RTREE` |
| `rtree_int_only` | `SQLITE_RTREE_INT_ONLY` |
| `scanstatus` | `SQLITE_ENABLE_STMT_SCANSTATUS` |
| `session` | `SQLITE_ENABLE_SESSION` |
| `setlk_timeout` | `SQLITE_ENABLE_SETLK_TIMEOUT` |
| `snapshot` | `SQLITE_ENABLE_SNAPSHOT` |
| `sqllog` | `SQLITE_ENABLE_SQLLOG` |
| `stat4` | `SQLITE_ENABLE_STAT4` |
| `stmtvtab` | `SQLITE_ENABLE_STMTVTAB` |
| `unlock_notify` | `SQLITE_ENABLE_UNLOCK_NOTIFY` |
| `update_delete_limit` | `SQLITE_ENABLE_UPDATE_DELETE_LIMIT` |
| `uri_00_error` | `SQLITE_ENABLE_URI_00_ERROR` |

**Build, platform, pager and test-hook macros:**

| Option | Macro |
| --- | --- |
| `allow_rowid_in_view` | `SQLITE_ALLOW_ROWID_IN_VIEW` |
| `builtin_test` | `SQLITE_UNTESTABLE` |
| `casesensitivelike` | `SQLITE_CASE_SENSITIVE_LIKE` |
| `compileoption_diags` | `SQLITE_OMIT_COMPILEOPTION_DIAGS` |
| `configslower` | `CONFIG_SLOWDOWN_FACTOR` |
| `curdir` | `SQLITE_OS_WINCE` |
| `datetime` | `SQLITE_OMIT_DATETIME_FUNCS` (inverted sense) |
| `debug` | `SQLITE_DEBUG` |
| `default_autovacuum` | `SQLITE_DEFAULT_AUTOVACUUM` (numeric: 0 or 1) |
| `default_ckptfullfsync` | `SQLITE_DEFAULT_CKPTFULLFSYNC` |
| `direct_read` | `SQLITE_DIRECT_OVERFLOW_READ` |
| `dirsync` | `SQLITE_DISABLE_DIRSYNC` |
| `fast_secure_delete` | `SQLITE_FAST_SECURE_DELETE` |
| `has_codec` | SEE codec extension |
| `integrityck` | `SQLITE_OMIT_INTEGRITY_CHECK` |
| `legacyformat` | `SQLITE_DEFAULT_FILE_FORMAT` |
| `lfs` | `SQLITE_DISABLE_LFS` |
| `like_match_blobs` | `SQLITE_LIKE_DOESNT_MATCH_BLOBS` (inverted) |
| `lock_proxy_pragmas` | `SQLITE_ENABLE_LOCKING_STYLE` |
| `malloc_usable_size` | `HAVE_MALLOC_USABLE_SIZE` |
| `mem3` | `SQLITE_ENABLE_MEMSYS3` |
| `mem5` | `SQLITE_ENABLE_MEMSYS5` |
| `memdebug` | `SQLITE_MEMDEBUG` |
| `mergesort` | unconditional in `test_config.c` |
| `mmap` | `SQLITE_MAX_MMAP_SIZE` |
| `multiplex_ext_overwrite` | `SQLITE_MULTIPLEX_EXT_OVWR` |
| `mutex` | `SQLITE_MUTEX_OMIT` |
| `mutex_noop` | `SQLITE_MUTEX_NOOP` |
| `pagecache_overflow_stats` | `SQLITE_DISABLE_PAGECACHE_OVERFLOW_STATS` |
| `prefer_proxy_locking` | `SQLITE_PREFER_PROXY_LOCKING` |
| `rowid32` | `SQLITE_32BIT_ROWID` |
| `secure_delete` | `SQLITE_SECURE_DELETE` |
| `tempdb` | `SQLITE_OMIT_TEMPDB` (inverted sense) |
| `thread_misuse_warnings` | `SQLITE_THREAD_MISUSE_WARNINGS` |
| `threadsafe` | `SQLITE_THREADSAFE` (numeric, not boolean) |
| `threadsafe1` | `SQLITE_THREADSAFE==1` |
| `threadsafe2` | `SQLITE_THREADSAFE==2` |
| `win32malloc` | `SQLITE_WIN32_MALLOC` |
| `worker_threads` | `SQLITE_MAX_WORKER_THREADS` (numeric) |
| `yytrackmaxstackdepth` | `YYTRACKMAXSTACKDEPTH` |
| `conflict` | unconditional in `test_config.c` |
| `crashtest` | the crash VFS, testfixture build only |

Three entries are **not** plain booleans, and will surprise anything that treats
them as flags:

- `threadsafe`, `threadsafe1`, `threadsafe2` come from the numeric
  `SQLITE_THREADSAFE` setting. `threadsafe1` is set from
  `SQLITE_THREADSAFE==1 ? "1" : "0"`, so a build with
  `SQLITE_THREADSAFE=2` has `threadsafe2` 1 and `threadsafe1` 0. A single-threaded
  engine should set all three to 0.
- `worker_threads` is the literal `STRINGVALUE(SQLITE_MAX_WORKER_THREADS)` — a
  number, used as a count rather than a flag.
- `configslower` is the literal value of `CONFIG_SLOWDOWN_FACTOR`, defaulting to
  the string `1.0`, and is divided by in timing-sensitive tests.

---

## 4. First 10 test files, in dependency order

Alphabetical order is what the suite's own `alltests` list uses, and it is not a
dependency order — it starts with `8_3_names.test`, `affinity2.test` and
`aggerror.test`. The order below is chosen so each file's prerequisites are
already proven, and so a failure points at one thing.

| # | File | Why it is here |
| --- | --- | --- |
| 1 | `select1.test` | Baseline read path. Errors, literals, NULL handling, type affinity, joins, subqueries and compound SELECT, all against a single table. Gates on `subquery` and `compound` only. Nothing else should be attempted before this passes. |
| 2 | `select2.test` | Same shape as select1 but with a `WHERE` clause and more expression forms. Isolates expression evaluation from table access. Gates on `tclvar`. |
| 3 | `select3.test` | The shape of the result set itself: column naming, `sqlite3_column_*` type reporting, `typeof()`, and affinity conversion rules. The rest of the suite trusts this behaviour, so it is checked separately. |
| 4 | `select4.test` | ORDER BY, DISTINCT, LIMIT/OFFSET and the compound operators across multiple tables. Broader than select3 and needs everything in it. Gates on `compound`, `subquery`. |
| 5 | `insert.test` | The write path: rowid assignment, `last_insert_rowid()`, default values, `INSERT ... SELECT`, multi-row inserts, and constraint violations. Nothing after this can be trusted without it. Gates on `compound`, `conflict`, `subquery`, `tempdb`, `explain`, `reindex`. |
| 6 | `update.test` | Row replacement, `rowid` reassignment, and UPDATE with subqueries. Depends on insert for rowid behaviour. Gates on `altertable`, `subquery`. |
| 7 | `delete.test` | Row removal, `DELETE` without a WHERE, and the `sqlite_sequence` bookkeeping that insert and update set up. Last of the basic DML so all three are proven together. Gates on `explain` and `trigger` (the trigger part skips). |
| 8 | `index.test` | `CREATE INDEX`, `DROP INDEX`, UNIQUE enforcement, and — importantly — that the index and a full table scan return identical results. Depends on insert/update/delete to have data worth indexing. Gates on `conflict`, `subquery`, `view`, `explain`, `reindex`, `trigger` (the trigger part skips). |
| 9 | `trans.test` | `BEGIN`/`COMMIT`/`ROLLBACK`, nested and implicit transactions, and the `test.db` rollback path. Needs correct writes first, because it is asserting on what was written. Gates on `pager_pragmas`, `tempdb`. |
| 10 | `subselect.test` | `IN (SELECT ...)`, `EXISTS`, correlated subqueries and `NOT IN` NULL semantics — the NULL-handling corner that plain subquery tests miss. Small (210 lines) and depends on select1's subquery coverage. Gates on `compound` and `subquery` (in both the positive and negated form). |

Two files deliberately held back: `with1.test` (CTE, 1259 lines) and
`compound.test` do not exist under those names — compound SELECT is covered by
`select1`/`select4`/`subselect`, and CTEs by `with1.test` through `withM.test`.
`with1.test` gates on `windowfunc`, so it belongs after the window functions
land. `join.test` is next after this list, but it is 1413 lines and pulls in
`pragma` and `view`, so it is a better eleventh than a tenth.

---

## 5. What pass rate to actually expect

**There is no number to quote here, and the honest answer is that the number
does not exist yet.** Not "low" — *unmeasured*. The reasons are structural, not
pessimistic.

### No from-scratch engine has published a verified pass rate

There is no public, reproducible number from any independent SQLite
reimplementation reporting a pass rate against the **unmodified** TCL suite.
The engines that exist in this space — Turso, and the various other SQLite
reimplementations — validate through differential testing against real SQLite
plus suite work, and do not publish a suite pass rate.

This is worth stating plainly because "what percentage should we expect" has an
answer that is easy to get wrong by pattern-matching against projects that do
publish numbers (compilers, parsers, kernels). The TCL suite is not like those.
It is a 1190-file, roughly 490k-line suite written over 25 years by the person
who wrote the engine, testing the engine through an API that exposes its
internals. There is no reference point for "a good from-scratch engine scores
X%."

### Turso describes the TCL suite as ongoing work

The closest comparable project is Turso, a mature Rust reimplementation. Its
`COMPAT.md` says, verbatim:

> Compatibility is validated through differential testing against SQLite and
> ongoing work to pass the full SQLite TCL test suite.

That is the whole claim. Differential testing, and the TCL suite as work in
progress. It is not a pass rate, and it is not a claim that a pass rate is
currently being measured. Note also that Turso tracks SQLite **3.50.4** and
runs a vendored copy of the harness at `sqlite/conformance/upstream/`, i.e. a
copy it controls rather than the stock tree — which is itself a statement that
the stock harness does not drop in unmodified.

### What the difficulty actually is

Three things make the unmodified suite hard, and none of them are "the SQL is
hard":

1. **The expected values are SQLite's exact behaviour**, including its quirks.
   The suite is a characterisation test: it records what SQLite does, right or
   wrong. `select1.test` expects the literal string `no such table: test1`.
   Error text, float formatting, integer overflow behaviour and column affinity
   rules all have to match, not merely be reasonable.
2. **The test-only surface is large.** Even choosing strategy (a), a
   C-API-complete engine still needs `install_malloc_faultsim`, the devsim VFS
   family, and the test-control entry points before the suite runs at all. This
   is a real body of work with no payoff outside testing.
3. **The suite tests 25 years of accumulated edge cases**, many of which exist
   to pin down behaviour nobody would otherwise think to specify.

### What can be said

A defensible way to talk about progress, once there is an engine, is per-file
and per-gate, not as a single percentage:

- `tools/run_suite.sh` reports per-file pass/fail and exits non-zero if any
  file fails, so per-file results are already the natural unit.
- The capability gate means a file can pass by skipping. Counting skips
  separately from passes is the difference between a real number and a
  flattering one.
- The 10 files in section 4 are a reasonable first milestone, chosen so each one
  exercises a layer the previous ones proved.

What would be a genuinely useful number, once an engine exists: the pass rate
over the `veryquick` permutation with the capability array from
`tools/capabilities.tcl`, reported **alongside** the skip count and the count
of files that failed to even start. Those three numbers together say something.
A percentage alone does not.

---

## Appendix: running it

```sh
# fetch (idempotent, verified)
tools/fetch_sqlite_suite.sh
tools/fetch_sqlite_suite.sh --verify     # check without downloading
tools/fetch_sqlite_suite.sh --force      # discard and re-fetch

# run
tools/run_suite.sh 'select1.test'
tools/run_suite.sh 'select*'
tools/run_suite.sh --part 2/8 'alter*'   # shard via TCLTEST_PART
tools/run_suite.sh --permutation full 'where*'
```

Exit codes from `run_suite.sh`:

| Code | Meaning |
| --- | --- |
| 0 | every matched file passed |
| 1 | suite not fetched, harness missing, or no test files matched |
| 2 | bad arguments |
| 3 | no usable Tcl interpreter — see below |

Exit 3 is the current state, and is the expected one until an engine exposes
itself to Tcl. `run_suite.sh` preflights for the four commands `tester.tcl`
calls first — `sqlite3_initialize`, `sqlite3_test_control_pending_byte`,
`install_malloc_faultsim`, `autoinstall_test_functions` — and reports which are
missing instead of printing a Tcl backtrace.

One implementation note, because it is easy to reintroduce: those preflight
probes must be **script files**, not here-documents. The mingw and ucrt64
`tclsh` builds read a piped script but do not propagate its error to the exit
status:

```
$ tclsh < script.tcl     # "can't find package sqlite3"  ->  exit 0
$ tclsh script.tcl       # "can't find package sqlite3"  ->  exit 1
```

A here-doc preflight would therefore pass unconditionally and the script would
fall through to a raw Tcl error. `run_suite.sh` writes its probes to a temp
directory for this reason.

`tools/capabilities.tcl` is standalone and can be checked without an engine:

```sh
tclsh -c 'source tools/capabilities.tcl; puts [array size ::sqlite_options]'
# 128
```

### Environment

| Variable | Effect |
| --- | --- |
| `TCLSH` | Which `tclsh` to use. Defaults to `tclsh` on `PATH`, falling back to the ucrt64 build. |
| `TESTER` | Override the harness path. This is how strategy (b) swaps in a shim. |
| `TCLTEST_PART` | Shard selector, read by `permutations.test` as `A/B`. |
| `TESTDIR` | Points at the suite's `test/` directory. |
| `SQLITE_TEST_DIR` | Set to the same value; read by `testrunner.tcl`. |

Note that `TESTDIR` is not how `.test` files find the harness — they use
`[file dirname $argv0]`. It is set because the harness and several `.test` files
read it anyway.

### Current toolchain

Present: `tclsh` 8.6.18 and 8.6.17, `tcltest` 2.5.11 and 2.5.10, `sqlite3`
3.53.4, `git`, MSVC, Git Bash, MSYS2, `curl`, `tar`, `openssl`.

Not needed for this track, but worth noting for when an engine lands: Tcl dev
headers and `cmake` are both absent, and strategy (a) needs the headers to build
an extension.

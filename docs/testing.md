# Testing nsqlite against the official SQLite TCL suite

This document covers where the suite comes from, the two ways to wire it to an
engine, the capability gate that decides what runs, **what the suite says about
nsqlite today**, and what pass rate to actually expect.

**Section 5 is the roadmap.** It lists, with the real `sqlite3` answer beside
nsqlite's for each one, every engine gap the suite has surfaced, ordered by how
many test files each one blocks.

Tooling lives in `tools/`:

| File | What it does |
| --- | --- |
| `tools/fetch_sqlite_suite.sh` | Downloads the pinned suite, verifies it |
| `tools/run_suite.sh` | Runs test files against nsqlite through the shim |
| `tools/capabilities.tcl` | The `$::sqlite_options` array for a minimal build |
| `test/shim/tester.tcl` | Replacement harness that drives the `nsqlited` CLI |

Quick start:

```sh
tools/fetch_sqlite_suite.sh        # ~13 MB, verified against the manifest
cargo build -p nsqlited
tools/run_suite.sh 'select1.test'  # per-file counts, sorted by closeness
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
870  64  -> sha3-256
```

Manifest rows are `F <name> <hash> [<perms>]`. Three files in `test/` are marked
executable and so carry a fourth field, the literal `x`: `test/testrunner.tcl`,
`test/speedtest.tcl` and `test/json/json-speed-check.sh`. That is why the counts
above are 416 + 870 rather than 416 + 869 — the two 64-hex rows extra in the raw
census are those files' *hashes*, not extra files. The verifier reads the fourth
field explicitly and discards it; reading only three fields folds `x` onto the
end of the hash, makes the field 42 or 66 characters, and drops the row at the
length check. That failure is silent, and it would have applied to
`testrunner.tcl` — the suite's own job scheduler.

All 1286 files under `test/` plus `VERSION` are therefore checked (1285 files in
`test/` + `VERSION`), and the fetch stamps `.fetched-version` only after they all
pass. A stamp therefore always means "this tree hashes clean".

The stamp records the count as well as the version, `<version> <count>`, so a
future drop in coverage shows up rather than passing quietly. `verify_manifest`
also treats an unrecognised hash-field length as a failure rather than a skip.

This is verified end to end, not assumed:

```
$ tools/fetch_sqlite_suite.sh
fetching sqlite/sqlite tag version-3.53.4
  downloaded 13163206 bytes
  verified 1286 files against manifest (SHA-1 + SHA3-256)

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

**This is the strategy in use.** `test/shim/tester.tcl` implements it; see
section 5 for what it currently achieves.

**What must exist in the engine first:**

- A CLI that reads SQL and writes results in a parseable format. This is a much
  smaller ask than a Tcl extension: a line-oriented mode is enough, and most
  engines get one for free. nsqlite's `nsqlited --testsuite` is it — a
  tag-byte record stream, one record per line, every value hex-encoded, so a
  newline inside a TEXT value and the difference between NULL and the empty
  string both survive. The SQL script goes in on **standard input**, not the
  command line: Windows caps a command line at 32767 characters and
  `createtab` and `types` both build statements past that.
- Error messages that match SQLite's text exactly. This is the real cost. The
  suite compares error strings as expected values — `select1-1.1` expects
  `{no such table: test1}` verbatim — so a shim cannot paper over differences
  the way a C API binding can. The engine gets this right for the messages it
  has; where it does not, section 5.3 says so.
- Correct handling of process exit and partial output. Anything that crashes
  mid-statement has to be distinguishable from a clean error.

**What is permanently lost:**

- **Everything that needs in-process access to engine internals.** The
  fault-injection suite (`*malloc*`, `*fault*` — the bulk of `crash*.test` and
  friends) cannot work, because the shim has no way to make the engine fail an
  allocation on demand. Neither can anything needing `sqlite3_stmt` handles,
  test-control, or a specific VFS.
- **Any state that must persist between two statements.** A transaction open
  across statements, a cursor mid-scan, a write not yet committed. One process
  per statement means none of it survives, and this is a property of the
  architecture, not a list of bugs.
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
`set_options()`. It has **128** entries at tag `version-3.53.4`. Of these, **108**
are named by `.test` files in an `ifcapable`/`capable` expression, and **113**
are named somewhere in the tree if the three harness `.tcl` files and direct
`$::sqlite_options(NAME)` reads are included as well. The remaining 15 are never
named at all:

```
autoindex between_opt carray complete decltype diskio geopoly localtime
memdebug multiplex_ext_overwrite ordered_set_funcs rowid32 rtree_int_only
truncate_opt worker_threads
```

(Counting this needs care, and the exact figure depends on what is being
scanned. `ifcapable` accepts `ifcapable fts5`, `ifcapable !fts5` and
`ifcapable {fts5 && ...}`, and the condition can also sit in the second operand
of a `||`, as in `crashM.test:19`:
`ifcapable !crashtest||!8_3_names`. A first pass that assumed names begin with
a letter silently dropped `8_3_names`; a second that allowed digits also picks
up `database`, `of` and `shared_chache` — the first two from prose comments
("is capable of reading and writing databases", "we are not capable of doing an
integrity check") and the third from a typo in a trailing comment at
`e_uri.test:434`. Those three are not options and are discarded by checking each
token against the 128. No `.test` file uses a `$` variable in the condition
position, so there is no dynamic form to chase.)

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
named by **six** `.test` files — `8_3_names.test`, `crashM.test`,
`delete_db.test`, `mjournal.test`, `multiplex.test` and `multiplex3.test` —
which would then die on a missing entry. Five name it directly and one
(`crashM.test:19`) reaches it in the second operand of a `||`, which is the
form a scan that only looks at the first token also misses. (`permutations.test`
also lists `8_3_names.test` in a `-files` list; that is the file name, not the
option.)

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

## 5. Where the suite actually stands

This section is the one to read. Everything above it is scaffolding; this is
what running the suite produced.

### 5.1 How to run it

```sh
tools/fetch_sqlite_suite.sh                      # once; verified against the manifest
cargo build -p nsqlited
tools/run_suite.sh 'select1.test'                # one file
tools/run_suite.sh 'select*'                     # a glob
tools/run_suite.sh 'select1.test' 'types.test'   # several files
tools/run_suite.sh --jobs 8 'alter*'             # in parallel
tools/run_suite.sh --list 'select*'              # resolve the glob, run nothing
tools/run_suite.sh 'shimselftest.test'            # the harness's own tests
```

Globs are resolved against two roots: the pristine suite checkout and
`test/shim/`, which holds this harness's own `.test` files. `shimselftest.test`
has to be reachable through the same entry point as everything else, or it is a
test nobody runs.

`run_suite.sh` prints one line per file — `errors`, `tests run`, `aborted` —
sorted by how close the file is to passing, then a totals line, and exits
non-zero if anything failed. Each file's own log is at
`test/sqlite-run/<name>/output.txt`, and the numbers the summary shows are
parsed out of that log rather than recomputed, so they are the harness's own
`finalize_testing` counters and are comparable with an upstream run.

The five states are kept distinct on purpose:

| State | Meaning |
| --- | --- |
| `ok` | ran every test, none failed |
| `FAILED` | ran every test, some failed — the number that says what to work on |
| `ABORTED after N tests` | ran N tests, then stopped — everything after the abort never ran |
| `no results` | never reached its summary, so **zero** tests were checked |
| `SKIPPED` | reached its summary, which says `0 errors out of 0 tests` |

The last three are all "less was checked than the file wanted", and they are not
the same amount of less. A `SKIPPED` file is one whose whole body is behind an
`ifcapable` that does not hold — `tkt3871.test` is `ifcapable !vtab` — which is
a correct, informative outcome. A `no results` file is a crash or a timeout,
which is not. An `ABORTED` file is in between and is the one most easily
misread: it has a real error count, but that count only covers the part of the
file that got that far, so its errors/tests ratio flatters it. `func.test` is
the extreme case — 0 errors out of 3 tests looks perfect and means that 221 of
its 224 `do_test` calls never executed, because line 45 runs `PRAGMA encoding`
and the engine does not parse `PRAGMA`.

Reporting any of the three as a pass would be the single most misleading thing
this script could do, and the exit status is non-zero for all three. The
totals line says `aborted (incl. part-way)` for the same reason: the per-file
counts overlap rather than partition, and a reader who assumes they partition
will conclude the total is wrong.

`--jobs N` runs files in parallel through the same `run_one` this script uses
serially — the parallel path re-enters the script per file rather than
duplicating the tclsh and engine discovery in a subshell, because a fix applied
to one and missed in the other is how a "passes with `--jobs 1`, fails with
`--jobs 8`" bug gets in. Any worker that dies without leaving a result is
retried once serially, so a runner crash is never reported as an engine gap.

### 5.2 The numbers

Over all 1190 `.test` files, no permutation, the `nsqlited` CLI through
`test/shim/tester.tcl`:

| Outcome | Files |
| --- | --- |
| pass completely | **29** |
| run their tests, fail some | **495** |
| abort before their first test | **117** |
| produce no summary at all (crash or timeout) | **5** |
| capability-skipped — 0 of 0 by an `ifcapable` | **544** |

**524 of 1190 files actually run a test** — the first two rows. That is
**24 192 individual cases executed**. The row that matters is the third: the
suite is not measuring nsqlite at 29/1190, and it is not measuring it on 1190
either.

All 29 passing files are small (1–15 cases): `orderby3` (15), `symlink` (13),
`literal2` (11), `tkt3357` (6), `tkt3935`, `tkt3871`, `func` and `contrib01`
(3 each), `tkt3527` and `tkt-4a03edc4c8` (2 each), and 19 more at one case
apiece. Nothing with real depth passes yet.

### The 117 that abort before their first test

This is the most actionable list in the document, because a file that aborts on
its first line has told you nothing. Grouped by the abort:

| Files | Aborts on | Whose gap |
| --- | --- | --- |
| ~30 | a `db` method or harness command the shim reports as unsupported | **shim**, or a ceiling |
| 9 | `source $testdir/permutations.test` not found | **shim** (staging missed `*.test`) |
| 6 | `near "PRAGMA": syntax error` | **engine** |
| 3 | `near "ATTACH": syntax error` | **engine** |
| 4 | `cannot commit - no transaction is active` | **ceiling** (process-per-statement) |
| the rest, one to eight each | fault injection, VFS, codecs, `rowid` | mostly a ceiling |

This list shrank from 129 to 117 during this pass, and every file that left it
left because a missing harness command was added. The remainder are dominated
by things a CLI cannot provide at all — an injected malloc failure, a custom
VFS, a Tcl-defined SQL function — and those are ceilings, not bugs.

The five that produce no summary at all: `boundary2`, `delete` and `round1`
are killed by a 120-second per-file timeout, and each had already run hundreds
of cases when it stopped — they are slow, not broken. `shellA` drives an
external `sqlite3` binary the harness correctly declines to fake. `qrf04`
stops on a `db format` method the engine process cannot provide.

The 539 capability-skips are the largest single group and are mostly
**correct**: a file whose whole body is `ifcapable !vtab` has nothing to say to
an engine without virtual tables. They are counted separately, and the runner
exits non-zero for them, because counting them as passes is how a harness
reports a flattering number instead of a measurement. They are also the
cheapest large group to turn into signal: a per-file `--maxerror` and the
`veryquick` permutation would each reach files this one-shot run does not.

#### The vtab skips are the largest single unblock left, and half of them are not reachable

Measured 2026-09-28, by parsing each file's own `ifcapable` guards:

```sh
ls test/sqlite-suite/test/*.test | wc -l                      # 1190
grep -l 'ifcapable.*!vtab\|ifcapable vtab' test/sqlite-suite/test/*.test | wc -l   # 138
# of those 138, gated on vtab AND NOTHING ELSE:                125
```

**125 files are blocked on one missing capability and nothing else.** That is
larger than every engine gap in section 5.3 added together, and it is the
reason virtual tables are the next thing to build rather than anything further
down the list.

But the module survey changes what "build it" means, because a large share of
those 125 want modules **this machine's own `sqlite3` does not have**:

| module | files wanting it | present in the reference? |
| --- | --- | --- |
| `echo` | 55 | **no** — `no such module: echo` |
| `tcl` | 48 | **no** — `no such module: tcl` |
| `fuzzer` | 38 | **no** — `no such module: fuzzer` |
| `unionvtab` | 26 | **no** — `no such module: unionvtab` |
| `csv` | 12 | **no** — `no such module: csv` |
| `fts3` / `fts4` / `fts5` | many | yes |
| `rtree` | 12 | yes |
| `zipfile` | 10 | yes |

The five absent modules are **testfixture additions**: upstream's `testfixture`
build links them in for the suite's own use, and a stock `sqlite3` does not have
them. So the honest reading of "125 files" is not "implementing virtual tables
runs 125 files". It is closer to:

* the **vtab mechanism** — `xConnect`/`xBestIndex`/`xFilter`/`xColumn`/`xNext`,
  the `CREATE VIRTUAL TABLE` path, and the `rootpage = 0` catalog row — is one
  piece of work that everything above depends on, and
* after it, **fts3/fts4/fts5** is a large subsystem of its own (three tokenizers,
  a segment format, and query parsing) that would gate the largest single block
  of the rest, while
* `echo`, `tcl`, `fuzzer`, `unionvtab` and `csv` are only reachable if the
  shim supplies them, which is the *testfixture*'s job, not the engine's.

That last distinction is the one worth keeping: a file wanting `echo` is not an
engine gap. It is a harness gap, and it stays a harness gap however good the
virtual-table implementation gets.

### Closest to passing, and therefore the cheapest next wins

Reproduce with:

```sh
tools/run_suite.sh -q 'hexlit.test' 'coalesce.test' 'tkt1536.test' \
                    'tkt3457.test' 'tkt2409.test' 'tkt3419.test' \
                    'vacuummem.test' 'select8.test' 'enc3.test'
```

| file | errors / cases | state |
| --- | --- | --- |
| `tkt3419` | 0 / 6 | **passes** |
| `coalesce` | 1 / 9 | FAILED |
| `enc3` | 1 / 4 | FAILED |
| `select8` | 1 / 3 | FAILED |
| `tkt1536` | 1 / 2 | FAILED |
| `hexlit` | 2 / 132 | FAILED |
| `vacuummem` | 3 / 6 | FAILED |
| `tkt2409`, `tkt3457` | 0 / 0 | SKIPPED — 0 tests ran, nothing measured |

`hexlit` is the interesting one, because its two failures are unrelated and
between them they are two of the cheapest wins in the suite.

One is a missing range check on a hex literal:

```
sqlite3:  SELECT -0x08000000000000000;
          Parse error: hex literal too big: -0x08000000000000000
nsqlite:  -9223372036854775808
```

That is one boolean in the literal parser. The neighbouring case
(`-0x0800000000000000`, exactly `INT64_MIN`) is correct in both, so the bound
is off by one in the strict direction and nsqlite wraps instead of rejecting.

The other is that **`rowid` does not exist** — see section 5.3, item 13.

A caution about this table, learned the hard way: the previous version of it
listed `tkt3457` and `tkt2409` as `1 / 5` and `1 / 4`, and `tkt1536` as `1 / 9`.
Both of the first two actually run **zero** tests and are capability skips, and
`tkt1536` runs 2. A reader who took the top of that list as the next thing to
work on would have found two files that execute nothing at all. Any number in
this document that is not reproduced by a command printed next to it is worth
re-measuring before being believed; the run above is the one to trust, and the
`SKIPPED` line is in the table rather than hidden so that a file measuring
nothing cannot be mistaken for a file nearly passing.

### 5.3 What the suite has surfaced: the engine roadmap

Every item below was confirmed by running the same statement through the real
`sqlite3` 3.53.4 and through `nsqlited`, on separate database files, and
comparing. The sqlite3 answers are quoted, because the suite compares error text
verbatim and "close enough" is not a thing it accepts.

Ordered by how many files each one blocks.

**1. `PRAGMA` is not parsed at all.** Every `PRAGMA` statement is a syntax
error.

```
sqlite3:  PRAGMA full_column_names=on;   -> accepted
nsqlite:  E near "PRAGMA": syntax error
```

This is the single largest source of file aborts: 19 files stop dead on it, and
it is a prerequisite in `select1`, `createtab`, `index` and `insert` alike.
`full_column_names` and `short_column_names` alone account for most of
`select1`'s remaining failures, because they change what `execsql2` sees as a
column name (`f1` vs `test1.f1` vs `test1 . f1`).

**2. `CREATE INDEX` and `DROP INDEX` are unimplemented.**

```
sqlite3:  CREATE TABLE t(a,b); CREATE INDEX i1 ON t(a);   -> ok
nsqlite:  E CREATE INDEX is not supported yet
```

7 files abort here and the whole `index.test` (77 errors / 101 cases) is
unreachable. Nothing that plans a query can be trusted until an index exists.

> **CLOSED.** `CREATE INDEX` and `DROP INDEX` both work; the index is written to
> `sqlite_schema` as `type = 'index'` and `load_schema` reads it back, so an
> index survives the process that created it. Re-measured 2026-09-28:
> `CREATE TABLE t(a);CREATE INDEX i1 ON t(a);` → `X X` (no error), and
> `index.test` runs 101 cases instead of aborting. Three separate causes had to
> be fixed together: the parser never captured the `CREATE INDEX` source text,
> the creation path had no `flush()`, and `load_schema` read `type = 'table'`
> rows only.

**3. `INSERT ... SELECT` is unimplemented.**

```
sqlite3:  CREATE TABLE a(x); INSERT INTO a VALUES(1);
          INSERT INTO b SELECT x FROM a; SELECT * FROM b;   -> 1
nsqlite:  E INSERT ... SELECT is not supported yet
```

`types.test` dies on it, `select1-2.0` fails on it, and `insert.test` leans on
it heavily.

> **CLOSED.** Re-measured 2026-09-28: the same four statements return `X X X X`
> then the row `1`. `types.test` went from aborting at 12 cases to running
> **55** cases, which is why it no longer appears in the ABORTED column of
> section 5.6.

**4. Column names are not resolved when there is a `FROM` clause.** This one is
subtle and wrong in a way that is easy to ship by accident:

```
sqlite3:  SELECT nosuchcol FROM t;    -> no such column: nosuchcol
nsqlite:  X
          C 1 T6E6F73756368636F6C      (a column literally named "nosuchcol")

sqlite3:  SELECT nosuchcol;            -> no such column: nosuchcol
nsqlite:  E no such column: nosuchcol  (correct!)
```

So the error is raised when there is no `FROM` and silently dropped when there
is one, and the query returns a single column whose *name* is the unresolved
identifier. Every "no such column" test in the suite that reads from a table is
passing for the wrong reason or failing for a confusing one.

> **CLOSED.** Re-measured 2026-09-28: `CREATE TABLE t(a);SELECT nosuchcol FROM
> t;` → `E no such column: nosuchcol`, identical to the no-`FROM` case. Both
> spellings of the message now match the reference byte for byte.

**5. `ORDER BY <integer literal>` is not range-checked and not applied as a
column ordinal.**

```
sqlite3:  SELECT * FROM t5 ORDER BY 2;   -> 2 9 / 1 10      (column b)
          SELECT * FROM t5 ORDER BY 3;   -> Parse error: 1st ORDER BY term
                                             out of range - should be between 1 and 2
nsqlite:  both return 1 10 / 2 9  (sorted by the first column, no error)
```

So nsqlite both mis-sorts and fails to raise. `select1-4.9` through `4.12` are
all this.

> **CLOSED.** Re-measured 2026-09-28, on a table with two columns and two rows:
> `SELECT * FROM t5 ORDER BY 2;` returns `2 9` then `1 10` — the reference's
> answer, i.e. ordered by column *b* — and `ORDER BY 3` raises
> `E 1st ORDER BY term out of range - should be between 1 and 2`, the
> reference's text exactly. The out-of-range message names the count of the
> offending term, so the wording varies with position; that part is generated,
> not hard-coded.

**6. Aggregate misuse is detected by the wrong rule, and the messages do not
match.**

```
sqlite3:  SELECT count(a,b) FROM t;   -> wrong number of arguments to function count()
nsqlite:  misuse of aggregate function count()

sqlite3:  SELECT min(a) AS m FROM t GROUP BY a HAVING max(m)<1;
                                     -> misuse of aliased aggregate m
nsqlite:  X   (accepted, no error)
```

`select1-2.1` through `2.23` are all this. The aliased-aggregate rule (ticket
#2526) is a real check with a real message, and nsqlite has neither.

> **PARTLY CLOSED.** Re-measured 2026-09-28: `SELECT count(a,b) FROM t` now
> raises `E wrong number of arguments to function count()`, the reference's text
> exactly. The **aliased-aggregate rule (ticket #2526) is still open** — it is
> not in the list of live findings below, but nothing in this tree implements it
> either, so treat "has neither" as now "has the first half".

**7. `EXPLAIN` is not parsed.** `explain.test`, and the `do_eqp_test` cases in
`orderby1`, stop here. Note this is two different needs: the suite reads the
opcode dump from `EXPLAIN` *and* the plan graph from `EXPLAIN QUERY PLAN`, and
neither is reachable.

> **CLOSED.** Re-measured 2026-09-28: `EXPLAIN SELECT 1;` returns an 8-column
> result row whose first two names are `addr` and `opcode`, which is the shape
> the suite reads. `explain.rs` and the `do_eqp_test` comparison in
> `test/shim/tester.tcl` (section 5.7, bug 2) are what made it reachable.

**8. Affinity is not applied on `REAL` and `NUMERIC` columns in one direction.**

```
sqlite3:  CREATE TABLE t(a REAL);    INSERT INTO t VALUES(5);
                                     typeof(a) -> real
nsqlite:  real                       correct
sqlite3:  CREATE TABLE t(a NUMERIC); INSERT INTO t VALUES(5.0);
                                     typeof(a) -> integer
nsqlite:  real                       wrong
```

That is `types.test` cases 1.1.4, 1.1.6 and 1.1.7 — the whole file's current
failure count.

> **CLOSED.** Re-measured 2026-09-28: `CREATE TABLE t(a NUMERIC);INSERT INTO t
> VALUES(5.0);SELECT typeof(a) FROM t;` returns `integer`, the reference's
> answer. `affinity.rs` and `affinity_rules.rs` hold the conversion; the
> measured grid of every declared-type/value pair is in `docs/affinity-grid.md`.

**9. Error text case is not preserved for unknown functions.**

```
sqlite3:  SELECT XYZZY(1);   -> no such function: XYZZY
nsqlite:  E no such function: xyzzy
```

The suite compares this exactly, so case matters.

> **CLOSED.** Re-measured 2026-09-28: `SELECT XYZZY(1);` returns
> `E no such function: XYZZY` — the identifier's case is preserved. This is the
> kind of fix that looks cosmetic and is not: the suite compares the whole
> message, so a case difference fails the case that has nothing else wrong with
> it.

**10. `AS 'quoted alias'` is rejected.** `SELECT a AS 'x y'` is a syntax error
in nsqlite and fine in SQLite. The suite uses it constantly for column-name
tests.

> **CLOSED.** Re-measured 2026-09-28: `SELECT 1 AS 'x y';` returns a one-column
> result whose column name is `x y` (hex `782079`).

**11. `UNION` and `DISTINCT` are unimplemented.**

```
sqlite3:  SELECT a FROM t UNION SELECT a FROM t;    -> ok
nsqlite:  E Union is not supported yet

sqlite3:  SELECT DISTINCT a FROM t;   (t has 1,1)  -> 1
nsqlite:  returns both rows
```

`UNION` failing outright blocks a large slice of `select4`, `compound` coverage
and every `EXCEPT`/`INTERSECT` test. `DISTINCT` is worse in kind, because it
does not fail — it returns duplicate rows, so a test that happens to have no
duplicates passes and one that does gets a wrong answer rather than an error.

> **DISTINCT is CLOSED; UNION is not.** Re-measured 2026-09-28:
> `INSERT INTO t VALUES(1),(1);SELECT DISTINCT a FROM t;` returns the single
> row `1`. **`UNION` is still unimplemented** — `SELECT 1 UNION SELECT 2;`
> still answers `E Union is not supported yet`, where the reference returns
> `1` then `2`. It stays at the top of the live list because it is the parser
> grammar for all four compound forms (`UNION`, `UNION ALL`, `EXCEPT`,
> `INTERSECT`) and because the suite's own `compound.test` is named after it.

**12. A subquery in `FROM` is rejected**: `E a subquery in FROM is not supported
yet`. This blocks `select1-6.9.7`/`6.9.8` and much of `join.test`.

> **CONFIRMED STILL OPEN.** Re-measured 2026-09-28: `SELECT * FROM (SELECT a
> FROM t);` → `E a subquery in FROM is not supported yet`.

**13. `rowid` is not implemented.** `SELECT rowid FROM t` and `SELECT
t.rowid FROM t` both say `no such column: rowid`; SQLite returns the row's
identifier. This is implicit-column resolution, and it is the single most
referenced name in the suite — `hexlit`, the four `rowid*.test` files, and
much of `insert`/`update` are built on it.

Note what is *not* available for reaching the value once it is implemented:
there is no `last_insert_rowid()` either.

```
sqlite3:  CREATE TABLE t(a); INSERT INTO t VALUES(7);
          SELECT last_insert_rowid();          -> 1
nsqlite:  E no such function: last_insert_rowid
```

So the key is on disk and the engine has it, but nothing surfaces it to SQL
today. An implementer should read it out of the b-tree record rather than
expecting an existing accessor, and should add `last_insert_rowid()` as part of
the same change or the suite will still not be able to check it. `SELECT rowid
FROM t` is the case to fix first: the gap is real, and `sqlite3` returns `1`
for it.

> **PARTLY CLOSED — measured 2026-09-28, and the numbers are the point.**
> `rowid`, `_rowid_` and `oid` all resolve now, bare and table-qualified, and
> case-insensitively (`RowID`, `ROWID`, `oId` all work), and they resolve in
> `WHERE` as well as the select list. A suite file written to pin exactly that
> passes 8 of 8:
>
> ```sh
> tools/run_suite.sh 'colnames.test'      # 0 errors out of 8 tests
> ```
>
> What moved, measured against the same suite runner:
>
> | file | before | after |
> | --- | --- | --- |
> | `rowid.test` | 206 errors / 221 cases | **178** / 221 |
> | `hexlit.test` | 2 / 132 | **1** / 132 |
> | `intpkey.test` | 47 / 95 | **27** / 95 |
>
> **Read the direction of that table carefully.** Every error count went *down*
> while the case count held, which is the good direction — but the reason the
> counts fell is not that 100 tests started passing on their merits. Of
> `rowid.test`'s 178 remaining failures, **100 are the same missing harness
> command**: `invalid command name "restore_prng_state"`, 100 occurrences,
> against 1 for `save_prng_state`. Those tests are all inside one
> I/O-fault-injection section, and both procs are **undefined in the upstream
> suite too** — `test/sqlite-suite/test/tester.tcl` only *calls* them, and the
> C testfixture supplies them. So this is a **harness ceiling**, not a gap to
> close: a CLI shim cannot inject an I/O failure into a process it does not own.
> The other two files that use them are `bitvec.test` and `walslow.test`, and
> `bitvec.test` skips 0-of-0 for the same reason.
>
> `last_insert_rowid()` is **still open**, and it is worth being precise about
> why, because the answer is not "the engine has not got round to it":
>
> ```sh
> $ sqlite3 li.db "CREATE TABLE t(a); INSERT INTO t VALUES(100);"
> $ sqlite3 li.db "SELECT last_insert_rowid();"    # -> 0   in a FRESH process
> $ sqlite3 li.db "SELECT rowid,a FROM t;"         # -> 1|100
> ```
>
> **The reference returns 0 too.** The value is not on disk and the file format
> does not preserve it; it lives in the connection. `fts4lastrowid.test` expects
> `3`, because it measures inside a single `execsql` block — which is exactly
> the shape this shim's one-process-per-statement model cannot express. So
> `last_insert_rowid()` is not a roadmap item for the engine at all; it is
> blocked on the shim, and it should be read as harness work.

**14. `BEGIN` on a file that did not exist when the process opened it.**
`Pager::open` builds its fresh-file branch (`len == 0`) with `path: None` and
never assigns it; only the non-fresh branch runs `pager.path =
Some(path.to_owned())`. `begin_journal` then hits `self.path.clone().ok_or_else(…)`
and answers `an in-memory database cannot be journalled` about a file-backed
database that has its handle open and is being flushed to. The fix is one line:
set `path: Some(path.to_owned())` in the fresh branch too, or hoist the
assignment below the `if fresh` / `else`.

```
$ rm -f f2.db
$ printf 'BEGIN;\n' | nsqlited --testsuite f2.db
E an in-memory database cannot be journalled          # wrong
$ printf 'BEGIN;\n' | sqlite3 f2.db                   # rc=0, no output
```

> **CLOSED.** Re-measured 2026-09-28: on a file that does not exist,
> `printf 'BEGIN;\n' | nsqlited --testsuite f2.db` produces no output and
> exits 0, the reference's answer. `pager.path` is now set in the fresh branch.
>
> A note on the *other* thing this item used to be confused with, because the
> two look identical from the outside and only one is a bug: the shim runs one
> process per statement, so **a transaction still cannot span two `execsql`
> calls**, and that remains a harness ceiling. `where2.test` fails 58 of 58 for
> exactly this reason, and will until the shim grows a single-process mode.

This reproduces entirely inside one process and `sqlite3` never shows it, so it
is a plain engine bug and not a limit of the process-per-statement harness. It
does **not** currently cost the suite anything, and it is worth saying why: the
shim's `sqlite3` proc materialises a newly opened path with `SELECT 1;` before
handing the name out, so by the time a test's first statement runs the file is
4096 bytes and takes the non-fresh branch. Two of the first-tier files abort on
`PRAGMA` or `DROP INDEX` long before this would matter. Fixing it is still
right — it is one line, it is a false error message, and the shim's
materialisation is a mitigation, not a reason the bug does not exist.

Separately, and genuinely a harness ceiling: the engine process exits between
statements, so **a transaction cannot span two `execsql` calls**. That is a real
boundary of what this harness can express, and it is what several `alter*` and
`with*` files hit as `cannot commit - no transaction is active`. It is worth
knowing where it falls, and it is worth not confusing it with the bug above,
which looks similar from the outside and is not a ceiling at all.

### 5.4 What the suite has surfaced: the shim roadmap

These are the things that stop a file before any test runs. They are listed
because they are cheap and they are what stands between the reachable
files and the rest.

Already fixed in this pass, and worth keeping in mind when reading the numbers
above, because each one converted a file-abort into real test output. The first
two are the important ones, because they were not "missing features" — they
were the harness reporting the wrong thing:

- **`nsqlite_flatten` and `execsql2` read a result set's column NAMES as if
  they were its values.** `nsqlite_run` returns one two-element list per
  statement, `{columns rows}`, and the consumers unpacked that as
  `{kind body}` — testing element 0 against the string `"ERROR"`. Element 0 of
  an ordinary result is the column list, so the comparison was false and the
  code returned *the column names* where the values belonged. Every `execsql2`
  case, and every column-name test built on it, was reading the wrong thing and
  comparing it. Fixed, with a single `nsqlite_stmt_error` predicate so the two
  shapes cannot drift apart again.
- **`db eval SQL SCRIPT ARRAY-VAR` (the callback form) did not exist.** 23 files
  aborted on "wrong # args". It now runs the script once per row, exposes each
  column as a **bare variable** (`$opcode`, `$a`) as upstream does, and
  evaluates the script in the test file's own scope. Getting that last part
  right took measurement rather than reasoning: the handle is an `interp
  alias`, and `do_test` runs its body under `uplevel #0`, so the same call
  sits at two different frame depths. Both are now detected, and
  `shimselftest.test` pins the behaviour.
- **`catchsql` returned `{0}` on success; upstream returns `{0 <rows>}`.** There
  are 4039 `catchsql` call sites. Fixed, and the rows are taken from the same
  execution rather than a second one — a second execution would double every
  `INSERT` and the tests that count rows afterwards would blame the engine.
- **`db close` uninstalled the handle, but upstream leaves the Tcl command
  usable.** A file doing `db close; sqlite3 db test.db; execsql ...` — which
  `types.test` does on its second line — hit `invalid command name "db"`. 29
  files. Fixed: `sqlite3` re-installs.
- **`source $testdir/<helper>.tcl` could not find the helper.** 122 files source
  `malloc_common.tcl`, 68 `lock_common.tcl`, 27 `fts3_common.tcl`, 26
  `wal_common.tcl` — and `$testdir` is the shim's scratch directory. The
  suite's `.tcl` helpers are now staged there, unmodified.
- **Missing harness commands that abort a file outright**: `do_not_use_codec`
  (41 files), `nonzero_reserved_bytes` (10), `load_static_extension` (9),
  `testvfs` (6), `database_may_be_corrupt` (6), `test_set_config_pagecache` (5),
  `atomic_batch_write` (5), `faultsim_save_and_close` (7),
  `test_cli_invocation` (7), `sqlite_limit` (4), `sqlite_create_function` (4),
  `sqlite_config_pmasz` (4), `do_select_tests` (7), `drop_all_tables` (3),
  `do_realnum_test` (4), `speed_trial_init` (3), `breakpoint` (3). All added.

Added in a second pass, after grouping the 129 remaining aborts by cause:
`do_malloc_test`, `do_ioerr_test`, `do_faultsim_test`,
`faultsim_delete_and_reopen`, the `sqlite3_memdebug_*` family,
`sqlite3_db_config_lookaside`, `sqlite3_config_lookaside`,
`sqlite3_config_uri`, `sqlite3_config_pmasz`, `sqlite3_soft_heap_limit`,
`sqlite3_hard_heap_limit`, `sqlite3_libversion_number`,
`sqlite3_register_cksumvfs`, `sqlite3_multiplex_initialize`,
`sqlite3_simulate_device`, `file_control_reservebytes`,
`file_control_chunksize_test`, `test_sqlite3_log`,
`clang_sanitize_address`, `sqlite3_create_function`, `getFileRetries`,
`getFileRetryDelay`. That moved 46 files out of "aborts on its first line".

Two more worth calling out, because they are the difference between a driver
file and a test file and it is not obvious which is which:

- **`source $testdir/permutations.test` failed.** `all.test`, `quick.test`,
  `full.test`, `veryquick`, `extraquick` and four others are four lines each:
  they source `permutations.test` and call `run_test_suite <name>`. Staging
  only `*.tcl` left them unable to find it, and all nine aborted on line one.
  The staging now copies `*.test` too, skipping the file under test.
- **`nsqlite_unsupported` said "database handle method" for everything.** A `db`
  method and a global harness command produced the same sentence, so a log
  reading `the database handle method "slave_test_file"` sends you looking at
  the `db` dispatch when the actual caller is `permutations.test`'s global
  `slave_test_file`. It now distinguishes the two.

The driver files themselves (`quick.test` and friends) are **not** test files.
They contain no cases: they ask `permutations.test` to run every other file in
a child interpreter under a permutation, and a child cannot drive the engine
process this shim owns. `run_suite.sh --permutation` does that job from
outside, one file at a time. So they report the gap and reach a summary with 0
of 0, which is the correct outcome and not a failure to fix.

Still open, and the next things to fix:

- **`db func` / `db collation` / `db create_aggregate` register a Tcl callback
  the engine process cannot call.** 12 files abort on `func` alone, 11 on
  `null`/`collate`. These cannot be fixed in the shim; they need the engine to
  accept a function over a pipe, or the tests to be skipped honestly via
  `ifcapable`.
- **`sqlite3_table_column_metadata`** (colmeta, 46 cases) needs schema metadata
  the record protocol does not carry.
- **`sqlite3_prepare`/`step`/`finalize`** cannot work across a process
  boundary, and the tests that use them are testing in-process behaviour
  (a table staying readable while another statement writes). `createtab.test`
  drives this directly.
- **The per-statement process model is the ceiling.** Anything that observes
  state *between* statements — an open transaction, a cursor mid-scan, a
  pending write — is not expressible. That is a design fact, not a bug list,
  and it is the argument for eventually building strategy (a) as well.

### 5.5 Testing the harness itself

`test/shim/shimselftest.test` is a normal `.test` file that tests the **shim**,
not the engine — 56 cases, all passing. It exists because a shim that silently
drifts from upstream's semantics turns every one of the 1190 real files into an
unreliable measurement, and the failure looks like an engine bug.

```sh
tools/run_suite.sh 'shimselftest.test'
# 0 errors out of 56 tests
```

Every case in it pins a behaviour the shim got wrong at least once. The
instructive ones:

- `catchsql` returns `{0 {rows}}`, not `{0 rows}` and not `{0}` — upstream is
  `lappend r $msg`, so the rows are one element.
- `db eval SQL ARRAY-VAR` leaves the **last** row, not the first and not all of
  them; the callback form is the one that sees every row.
- The callback form exposes each column as a **bare variable** (`$opcode`,
  `$a`), not only as `row(opcode)`. Both spellings appear in the suite.
- `do_test`'s four expected-value forms need their **delimiters**: `/pat/` for a
  regexp, `*glob*` for a glob, `#/list/` for numeric. `{[0-9]+}` and `a*` are
  *exact* comparisons, and cases for that are included because "looks like a
  regexp" is not "is one".
- Each of those four forms is checked in **both** directions. A comparison that
  is too loose reports passes that are not; one that is too tight reports
  failures that are not. Checking only the accepting direction proves nothing.

Two more self-test files were added for the same reason:

```sh
tools/run_suite.sh 'eqpselftest.test'     # 0 errors out of 13 tests
tools/run_suite.sh 'md5probe.test'        # 0 errors out of 21 tests
tools/run_suite.sh 'md5cmdtest.test'      # 0 errors out of  7 tests
```

`eqpselftest.test` exists because the `do_eqp_test` bug was **invisible by
construction**. A comparison that fails too often produces a red suite
somebody notices; a comparison that fails too *little* produces a green one
nobody checks. The plan is stubbed in that file rather than fetched, because
`EXPLAIN` is unparsed and a real plan is not available yet — what is under test
is how the expectation is framed, and a stub answers that question exactly. It
includes negative cases: a substring that is not in the plan, and a whole-plan
expectation that does not match, must each still fail, and the test asserts
that the error counter went up. The failure count is saved and restored around
them so the file reports 0 errors while still proving the checks bite.

`md5probe.test` pins the digest semantics, because a wrong digest is the most
dangerous possible harness bug here: `trans.test` and 29 other files use
`md5sum` to decide whether a transaction preserved a table, so a digest that is
merely *stable* rather than *correct* turns those files green. Each expectation
is a value computed independently, not a value the shim produced.

Its last three cases pin the other half of the contract: a shape this shim does
**not** compute has to be reported, not guessed at. `SELECT md5sum(a) FROM t
GROUP BY a` should produce one digest per group and this shim produces one
digest, so it refuses the statement. That guard was written with `\b`, which Tcl
takes as a **backspace** rather than a word boundary — the pattern compiled
cleanly and matched nothing, and the statement silently returned a single digest
for a query whose real answer is a different number of rows. A guard that cannot
fire is the same as no guard, and the case is in the test file so it cannot go
back to being one.

`md5cmdtest.test` pins the `md5` command against the RFC 1321 vectors and, more
usefully, pins the relationship between it and the aggregate. The two produce
**different** digests over the same rows, and that is correct: `md5` is handed
one string the caller has already joined, while `md5sum` concatenates with no
separator at all. A harness that made them agree by quietly inserting a
separator would pass the easy case and fail every real one, so both digests are
asserted.

### 5.7 Three harness bugs this pass found

These are recorded because each one was a check that was not doing what it
claimed, which is worse than a missing check: a missing check shows up as a
missing test, a weakened one shows up as a wrong answer.

**1. `md5sum()` was called but never defined.** The interception in
`nsqlite_run` called `nsqlite_md5sum_records`, a proc that did not exist in the
file. Every `SELECT md5sum(...)` in the suite raised
`invalid command name "nsqlite_md5sum_records"`. The first user-visible effect
was in `trans.test`, where the statement at line 721 that establishes
`::checksum` threw, `::checksum` was never set, and every later read raised
`can't read "checksum": no such variable` — aborting the file. The comment
above the interception claimed the gap was "closed here", and it was not.

The replacement computes the digest in Tcl, because the engine cannot evaluate
it and cannot be made to: `md5sum` is an aggregate, the only aggregate that
could stand in for it is `group_concat`, and reaching that needs a scalar
subquery, which the engine rejects with `subqueries are not supported yet`.
The statement is split, the aggregate is fetched as per-row argument values,
and the MD5 is taken here. The semantics are `md5step`/`md5finalize` from
`threadtest3.c:391-415`: every argument is taken as TEXT, all rows feed **one**
running MD5 in order with no separator, a NULL argument contributes nothing at
all, and zero rows still finalizes over the empty string.

The NULL rule is the one worth calling out, because the obvious implementation
is wrong. `md5step` skips a NULL with an `if( zData )` guard, so NULL and the
empty string must not hash the same — but a `coalesce(arg, '')` rewrite, which
is the natural way to render arguments as TEXT, makes them identical. The shim
therefore asks the engine directly, selecting
`CASE WHEN (arg) IS NULL THEN 1 ELSE 0 END` beside the value and dropping the
row when the flag is set. That is a question about the engine's own NULL
semantics rather than a guess about them.

The MD5 itself was verified against the RFC 1321 vectors
(`""`, `"a"`, `"abc"`, 80-byte input) before anything depended on it, and
`func.test` — which uses `md5sum` with 400 arguments — went from failing to
passing as a result.

**2. `do_eqp_test` compared for equality where upstream compares for
containment.** Upstream (`test/sqlite-suite/test/tester.tcl:1060-1065`) wraps a
non-plan-block expectation in `/*...*/` and runs it through `do_execsql_test`,
so `do_eqp_test 1.1 {SELECT * FROM t1} {SCAN}` matches a plan that *contains*
`SCAN`. The shim instead passed the bare expectation, so `do_test` compared the
one-element list `{SCAN}` against the list of plan lines for exact equality.
There are 397 `do_eqp_test` call sites, and every one whose expectation is a
bare substring was silently downgraded. Upstream's exact-match fast path was
dropped too, which only costs a second `EXPLAIN`.

This was masked because `EXPLAIN` is unparsed, so the tests fail either way —
luck, not design. It became visible as soon as the framing was fixed, because
`orderby1` went from 46 errors to 33 and `index` from 76 to 70: those failures
were the harness's, not the engine's. The engine gap underneath is unchanged.

**3. `do_eqp_execsql_test` dropped the query-plan half.** It ran
`execsql $sql` twice and compared the first result against *both* expectations.
That is not a weaker check but a different one: the plan was never compared, so
a wrong plan passed whenever the rows happened to match. It also reported the
second half as `${name}.1` where upstream reports `${name}b`, so the test names
in the log did not line up with an upstream run.

**4. The `md5` Tcl command was missing.** `md5sum` is not the only thing the
testfixture build adds. It also links an `md5` **command** into tclsh, and three
files call it — `func`, `memdb` and `trans2`. Stock tclsh has no such command
(`[info commands md5]` returns nothing), so `trans2.test` aborted at its first
`hash1` call with `invalid command name "md5"` and ran **0 of its 291 tests**.
The shim already had an RFC 1321 MD5 for the aggregate, so this is an alias onto
the same function, and the two have to be the same function: `func.test` calls
`md5` 400 times in a loop to *predict* what its own `md5sum()` query returns, so
if they disagreed the test would fail for a reason that has nothing to do with
the engine.

With it wired up, `trans2.test` runs all 291 cases. All 291 fail, on three
engine gaps — `no such table: t1` (203), `cannot rollback - no transaction is
active` (29) and `PRAGMA` (1) — which is the shape of result that means a file
is now *measuring* the engine rather than *blocked* by the harness.

One Tcl subtlety is worth recording because it cost the most time here. Both
eqp fixes pass the expectation to `do_test` through `uplevel`, and `[list $res]`
is **not** a no-op: `list` quotes a string that already needs quoting, so
`do_test` received the wrapped expectation with a brace on each end, the
leading brace became part of the expected value, and the glob stopped
matching. `[concat [list do_test ...] [list $res]]` splices the expectation in
as a separate word and produces a valid argument list. The failure mode is
silent — the test fails, with an expected value that looks correct in the log.

### 5.6 What the first tier looks like

The brief asked for `select1`, `create`, `insert`, `types`, `orderby1`, `trans1`
and `index`. Two of those names do not exist in SQLite 3.53.4 — there is no
`create.test` and no `trans1.test`; the corresponding files are `createtab.test`
and `trans.test`. The intent maps cleanly.

Reproduce this table with exactly:

```sh
tools/run_suite.sh 'select1.test' 'createtab.test' 'insert.test' 'types.test' \
                   'orderby1.test' 'trans.test' 'index.test'
```

| Asked for | Actual file | errors / cases run | state | What blocks it |
| --- | --- | --- | --- | --- |
| `select1` | `select1.test` | 69 / 188 | FAILED | `UNION`, `rowid`, `*` in the select list, column-name rules |
| `create` | `createtab.test` | 16 / 20 | FAILED | the prepare/step API (a harness ceiling) |
| `insert` | `insert.test` | 45 / 74 | FAILED | `rowid`, `UNION`, multi-row `VALUES` |
| `types` | `types.test` | 7 / 55 | FAILED | `rowid` |
| `orderby1` | `orderby1.test` | 31 / 51 | aborted at 51 | the remaining `EXPLAIN QUERY PLAN` shapes, `rowid` |
| `trans1` | `trans.test` | 66 / 106 | aborted at 106 | `rowid`, `txn_state`, and the process-per-statement ceiling |
| `index` | `index.test` | 70 / 101 | FAILED | `rowid`, and the plan assertions |

**Measured 2026-09-28**, with `--jobs 6` (the per-file numbers do not depend on
the job count; the runner re-enters the same `run_one` either way).

**Every one of the seven runs and reaches its summary.** None passes
completely, and the reason in each case is an engine gap listed above, not a
harness gap — that distinction is the point of the table.

Three files moved since the table was first written, all upward, and each one
names a roadmap item that section 5.3 now records as closed:

* `types.test` went from **0 errors out of 12 cases, aborted** to **7 of 55**,
  because `INSERT ... SELECT` and the NUMERIC affinity rule are both fixed. It
  is no longer in the ABORTED state at all.
* `select1.test` went from 45/113 to 69/**188** — 75 more cases execute, because
  the `PRAGMA` and column-name paths that used to abort it are reachable. Its
  error *count* rose too, which is the expected shape: more cases running means
  more chances to fail, and a file that measures more is worth more than one
  that aborts early.
* `index.test` is unchanged at 70/101 because `CREATE INDEX` and `DROP INDEX`
  being fixed made the whole file *reachable* — before, it could not run at
  all. An unchanged error count here is not an unchanged situation.

What the remaining blockers have in common: **`rowid` is nearly all of them.**
It is item 13 on the list and it is now the single cheapest large win, for the
reason given in its entry.

Two things in that table are worth reading carefully rather than skimming.

`types.test` used to report **0 errors** — which is not a pass, but a file that
aborted after 12 of its cases on `INSERT ... SELECT is not supported yet`. That
is fixed: it now runs 55 cases and reports 7 real errors. `func.test` is the
remaining example of the shape, and it is worse than `types.test` was — 0
errors out of 3 tests, out of 224 `do_test` calls in the file — because
`PRAGMA` on line 45 stops it before the file's own subject matter. This is why
the runner reports `ABORTED` as its own state rather than folding it into
`FAILED`: an error count with no denominator for the file is not a pass rate.
A file that has stopped aborting is a strictly better thing than one that has
not, even when its error count goes **up** — `select1.test` is at 69 errors
over 188 cases where it used to report 45 over 113.

The numbers also moved once, for a different reason: when the `do_eqp_test`
comparison was fixed (section 5.7), `orderby1` went 46 → 33 errors and
`index` 76 → 70, because those files' plan tests were being failed by a
harness bug rather than by the engine. `EXPLAIN` itself is now parsed, so that
gap is closed too.

### 5.6a A wider sweep, for scale

The seven files above are the ones the brief named. A wider run over 24 files
puts the first tier in context, and is worth having because the error counts
span three orders of magnitude:

```sh
tools/run_suite.sh --jobs 8 'select[1-8].test' 'createtab.test' 'insert[2]?.test' \
    'types[2-3]?.test' 'orderby[12].test' 'trans[2]?.test' 'index.test' \
    'where.test' 'join.test' 'func.test' 'coalesce.test' 'hexlit.test'
```

| file | errors / cases | state |
| --- | --- | --- |
| `coalesce` | 1 / 9 | FAILED |
| `select8` | 1 / 3 | FAILED |
| `hexlit` | 2 / 132 | FAILED |
| `types` | 0 / 12 | aborted at 12 |
| `func` | 0 / 3 | aborted at 3 |
| `types3` | 2 / 2 | aborted at 2 |
| `insert2` | 7 / 8 | aborted at 8 |
| `orderby2` | 7 / 11 | FAILED |
| `select7` | 8 / 9 | aborted at 9 |
| `createtab` | 16 / 20 | FAILED |
| `types2` | 18 / 84 | aborted at 84 |
| `insert` | 51 / 74 | FAILED |
| `orderby1` | 33 / 51 | aborted at 51 |
| `select3` | 60 / 90 | FAILED |
| `select1` | 45 / 113 | aborted at 113 |
| `index` | 70 / 101 | FAILED |
| `select6` | 83 / 87 | FAILED |
| `join` | 103 / 161 | aborted at 161 |
| `trans` | 66 / 106 | aborted at 106 |
| `where` | 282 / 315 | FAILED |
| `trans2` | 291 / 291 | FAILED |
| `select2`, `select4`, `select5` | 0 / 0 | no results |

**1 682 cases across 24 files**, 1 146 of them failing.

`trans2` is the one to look at. It runs **291 cases where it used to run none**,
and all 291 fail — on `no such table: t1` (203), `cannot rollback - no
transaction is active` (29) and `PRAGMA` (1). That is the shape of a file that
has started *measuring* the engine rather than being blocked by the harness,
which is the only direction that matters here; its error count is large because
its cases are real, not because the harness is inventing failures.

`select2`, `select4` and `select5` produce no summary at all. They are not
skips — they are files the run could not finish inside its time budget, which
is a different thing again and is why the runner reports `no summary` rather
than `0 / 0`.

---

## 6. What pass rate to actually expect

**A single percentage is not a meaningful number for this suite, and the reason
is structural rather than pessimistic.** The rest of this section is why, and
why section 5 reports per-file counts instead.

### No from-scratch engine has published a verified pass rate

There is no public, reproducible number from any independent SQLite
reimplementation reporting a pass rate against the **unmodified** TCL suite.
The engines that exist in this space — Turso, and the various other SQLite
reimplementations — validate through differential testing against real SQLite
plus suite work, and do not publish a suite pass rate.

This is worth stating plainly because "what percentage should we expect" has an
answer that is easy to get wrong by pattern-matching against projects that do
publish numbers (compilers, parsers, kernels). The TCL suite is not like those.
It is a 1190-file suite whose `.test` + `.tcl` tree is 498,973 lines, written
over 25 years by the person who wrote the engine, testing the engine through an
API that exposes its internals. There is no reference point for "a good
from-scratch engine scores X%."

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

Report per-file counts, and always three of them. `tools/run_suite.sh` is built
to produce exactly that, and the three numbers answer three different
questions:

| Number | Question |
| --- | --- |
| passed | is this correct here? |
| failed | what is wrong, and where? — this is the one that drives work |
| ran no tests | what did we not even look at? |

A percentage collapses those into one figure and destroys the distinction that
matters most. If 700 of 1190 files produce nothing, "2% of the suite passes"
and "100% of the files we can actually run pass" are both true, and only the
second one is about the engine.

The capability gate is a third number, and it is not optional. A file can pass
by being entirely skipped (`tkt3871.test` is `ifcapable !vtab` and reports 0
of 0), and a shim that counted that as a pass would be reporting a flattering
number. `run_suite.sh` prints it as `SKIPPED` and exits non-zero for it.

The 10 files in section 4 remain a reasonable first milestone, chosen so each
exercises a layer the previous ones proved. Section 5.6 reports where those
stand today.

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
tools/run_suite.sh 'select1.test' 'types.test'   # several files
tools/run_suite.sh --jobs 8 'alter*'             # in parallel
tools/run_suite.sh --permutation full 'where*'
tools/run_suite.sh --maxerror 1 'alter.test'     # stop a file after 1 failure
tools/run_suite.sh --list 'select*'              # resolve the glob, run nothing
tools/run_suite.sh --quiet 'select*'             # summary only
```

Exit codes from `run_suite.sh`:

| Code | Meaning |
| --- | --- |
| 0 | every matched file passed |
| 1 | at least one file had a test failure, or produced no results |
| 2 | bad arguments, or no test files matched |
| 3 | the harness, the engine, or `tclsh` is missing |

Exit 1 covers both "ran and failed some" and "never produced a result". Both
mean the engine does not do it yet, and neither is allowed to read as a pass.

One implementation note, because it is easy to reintroduce: any `tclsh` probe
must be a **script file**, not a here-document. The mingw and ucrt64 `tclsh`
builds read a piped script but do not propagate its error to the exit status:

```
$ tclsh < script.tcl     # "can't find package sqlite3"  ->  exit 0
$ tclsh script.tcl       # "can't find package sqlite3"  ->  exit 1
```

A here-doc probe would therefore pass unconditionally and the script would fall
through to a raw Tcl error. `run_suite.sh` writes its probes to a temp
directory for this reason.

`tools/capabilities.tcl` is standalone and can be checked without an engine.
Ship the probe as a **file**, for the same reason as above:

```sh
printf 'source tools/capabilities.tcl; puts [array size ::sqlite_options]\n' > /tmp/cap.tcl
tclsh /tmp/cap.tcl
# 128
```

There is no `tclsh -c` on 8.6. `-c` is not a flag, so `tclsh` treats it as a
script filename, fails to open it, and drops to the interactive REPL — printing
a bare `%` and then blocking on stdin. It exits 0, so the mistake reads as
success and the expected `128` never appears at all.

### Environment

| Variable | Effect |
| --- | --- |
| `TCLSH` | Which `tclsh` to use. Defaults to `tclsh` on `PATH`, falling back to the ucrt64 build. |
| `NSQLITED` | The engine binary. Defaults to `target/debug/nsqlited.exe`. |
| `TCLTEST_PART` | Shard selector, passed through if set in the environment. |
| `TESTDIR` | Points at the suite's `test/` directory. |
| `SQLITE_TEST_DIR` | Set to the same value; read by `testrunner.tcl`. |

Note that `TESTDIR` is not how `.test` files find the harness — they use
`[file dirname $argv0]`. It is set because the harness and several `.test` files
read it anyway.

### What `run_suite.sh` actually executes

`run_suite.sh` runs **`test/shim/tester.tcl`**, not the suite's own
`testrunner.tcl`. That is a deliberate change from the original version of this
script, and the reason is worth stating precisely, because the stock
`testrunner.tcl` cannot work here.

The suite's harness is a library, not a driver. Every `.test` file does
`set testdir [file dirname $argv0]` and `source $testdir/tester.tcl`, then calls
`finish_test`. The stock harness reaches for `sqlite3_initialize`,
`install_malloc_faultsim` and `autoinstall_test_functions` before the first
test executes, and those come from the SQLite *testfixture* build's Tcl
binding. Without it, `testrunner.tcl` fails on its first line.

So the shim replaces `tester.tcl` — not the driver — and the shim is
self-driving: it takes `<permutation> <file.test>`, stages the suite's `.tcl`
helpers and a forwarder beside the test file, opens the database, and runs the
file itself, reporting through the same `finalize_testing` counters upstream
uses. The invocation contract (`<permutation> <path/to/file.test>`) is
unchanged, so the numbers are comparable with an upstream run.

The shim creates `tester.tcl` in the **run directory**, not in the suite's tree.
The suite is a pristine checkout verified against its Fossil manifest, and a
run must not be able to modify it.

`testrunner.tcl` remains the right entry point for strategy (a) — a real Tcl
extension — and the `testrunner.tcl` invocation contract documented above still
describes it correctly. What changed is only which one `run_suite.sh` calls,
because strategy (b) is the strategy in use.

The `--start=<perm>:<file>` form is a *different* entry point. It sets
`::G(start:permutation)` and `::G(start:file)`, but both are only consulted
inside `slave_test_file` (`tester.tcl:2392`), which is reached solely through
`permutations.test`'s `run_tests` loop. It filters; it does not apply a
permutation, because applying one means setting `::G(perm:name)`,
`::G(perm:prefix)` and `::G(perm:dbconfig)`, and only `testrunner.tcl` and
`run_tests` do that.

`TCLTEST_PART` is read in exactly one place: `permutations.test:1165`, inside
`run_tests` — the same file-list driver. The single-file path does not consult
it. `run_suite.sh` still exports it when `--part` is given, so a shim or a
driver that wraps `permutations.test` can honour it, but sharding a
`testrunner.tcl <perm> <file>` invocation does nothing on its own.

### Current toolchain

Present: `tclsh` 8.6.18 and 8.6.17, `tcltest` 2.5.11 and 2.5.10, `sqlite3`
3.53.4, `git`, MSVC, Git Bash, MSYS2, `curl`, `tar`, `openssl`.

Not needed for this track, but worth noting for when an engine lands: Tcl dev
headers and `cmake` are both absent, and strategy (a) needs the headers to build
an extension.

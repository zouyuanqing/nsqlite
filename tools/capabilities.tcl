# tools/capabilities.tcl
#
# The $::sqlite_options array for a deliberately minimal nsqlite build.
#
# WHY THIS FILE EXISTS
#
# Every .test file in the official suite decides what to run by asking the
# interpreter which features the engine has. It does that through the global
# $::sqlite_options array, which the engine's test fixture populates at startup.
# tester.tcl reads it two ways:
#
#   if {$::sqlite_options(memdebug)} { ... }            direct read
#   set AUTOVACUUM $sqlite_options(default_autovacuum)
#   ifcapable  fts5            { ... }                  rewritten by ifcapable
#   ifcapable !default_autovacuum { ... }
#   ifcapable  fts3 || json1   { ... }
#
# The array is defined by SQLite's own test fixture, in src/test_config.c, in
# the proc set_options(). It has exactly 128 entries at tag version-3.53.4.
# The .test files reference 89 of them directly through ifcapable/capable;
# the rest are read as plain array variables.
#
# Nothing in the suite ever sets a missing entry. So an engine that leaves one
# undefined does not get a clean skip, it gets "can't read
# "::sqlite_options(fts5)": no such variable" and the test file dies. That is
# why this file defines ALL 127, including the ones nsqlite does not implement.
# A test guarded by `ifcapable rbu` should skip because rbu is 0, not because
# the array lookup blew up.
#
# HOW TO USE
#
#   source tools/capabilities.tcl
#   puts $::sqlite_options(fts5)
#   array size ::sqlite_options      ;# 128
#
# A Tcl extension built against nsqlite should source this at init, then
# override any entry that disagrees with what the engine actually does.
#
# These values describe INTENT, not verified behaviour. Nothing in the engine
# sets them yet, and writing 1 does not implement anything. Flipping a 1 on
# without the matching feature makes the suite run tests that fail, which is
# the intended way to find out what is still missing.

package require Tcl 8.6

# ---------------------------------------------------------------------------
# What this build intends to pass. Everything else defaults to 0 below.
#
# NOTE ON NAMES: the suite has no `btree`, `tables`, `indexes` or
# `transactions` option. Those are the intended capabilities, expressed in the
# real names the suite actually understands:
#
#   btree          always on. There is no flag for it; a pager that can walk
#                  pages is assumed. Nothing to set.
#   tables         always on. CREATE/INSERT/UPDATE/DELETE are unconditional.
#   indexes        autoindex (UNIQUE enforcement). A plain CREATE INDEX is also
#                  unconditional -- there is no index capability gate.
#   transactions   always on. BEGIN/COMMIT/ROLLBACK are unconditional.
#   views          view
#   subqueries     subquery
#   compound       compound
#   cte            cte
#
# So the only ones in that list that need an entry set are the last four, and
# they are all 1 below. The rest of the 128 are gates the suite uses to skip
# work; 0 there means "skip", which is the honest answer for a build that has
# not implemented them yet.
#
# The 0 values are load-bearing rather than lazy -- each one makes a whole
# family of .test files skip instead of fail, so a test file that passes is
# doing real work rather than quietly opting out. How much each one hides:
#
#   vtab     133 test files gate on it
#   fts3     112
#   trigger   65
#   wal       53
#   fts5      15
#   rtree     12
#   json1      9
#   8_3_names  5
#   rbu       2
#   session    1
#   geopoly   0
#
# Deliberately absent, because they are large and self-contained and are not
# prerequisites for a correct core:
#
#   fts3, fts3_unicode, fts4_deferred, fts5   full-text search
#   rtree, rtree_int_only                      R*Tree index
#   geopoly                                   geospatial R*Tree layer
#   session                                   the sessions extension
#   json1                                     the JSON functions
#   rbu                                       the RBU extension
#   vtab                                      the virtual table mechanism
#   trigger                                   triggers
#
# The two costs of setting vtab and trigger to 0 are worth stating plainly:
# 133 and 65 test files respectively will skip, and both are things a real
# SQLite has. Leaving them out is a deliberate scoping decision for the first
# milestone, not a claim that they are unnecessary.
# ---------------------------------------------------------------------------

set ::sqlite_options(8_3_names)             0  ;# SQLITE_ENABLE_8_3_NAMES
set ::sqlite_options(altertable)            0  ;# SQLITE_OMIT_ALTERTABLE
set ::sqlite_options(analyze)               0  ;# SQLITE_OMIT_ANALYZE
set ::sqlite_options(api_armor)             0  ;# SQLITE_ENABLE_API_ARMOR
set ::sqlite_options(atomicwrite)           0  ;# SQLITE_ENABLE_ATOMIC_WRITE
set ::sqlite_options(attach)                0  ;# SQLITE_OMIT_ATTACH
set ::sqlite_options(auth)                  0  ;# SQLITE_OMIT_AUTHORIZATION
set ::sqlite_options(autoinc)               1  ;# present, not SQLITE_OMIT_AUTOINCREMENT
set ::sqlite_options(autoindex)             1  ;# present, not SQLITE_OMIT_AUTOMATIC_INDEX
set ::sqlite_options(autoreset)             0  ;# SQLITE_OMIT_AUTORESET
set ::sqlite_options(autovacuum)            0  ;# SQLITE_OMIT_AUTOVACUUM
set ::sqlite_options(between_opt)           1  ;# present, not SQLITE_OMIT_BETWEEN_OPTIMIZATION
set ::sqlite_options(bloblit)               1  ;# present, not SQLITE_OMIT_BLOB_LITERAL
set ::sqlite_options(builtin_test)          0  ;# SQLITE_UNTESTABLE
set ::sqlite_options(carray)                0  ;# SQLITE_ENABLE_CARRAY
set ::sqlite_options(casesensitivelike)     0  ;# SQLITE_CASE_SENSITIVE_LIKE
set ::sqlite_options(cast)                  1  ;# present, not SQLITE_OMIT_CAST
set ::sqlite_options(check)                 1  ;# present, not SQLITE_OMIT_CHECK
set ::sqlite_options(columnmetadata)        0  ;# SQLITE_ENABLE_COLUMN_METADATA
set ::sqlite_options(compileoption_diags)   0  ;# SQLITE_OMIT_COMPILEOPTION_DIAGS
set ::sqlite_options(complete)              1  ;# present, not SQLITE_OMIT_COMPLETE
set ::sqlite_options(compound)              1  ;# present, not SQLITE_OMIT_COMPOUND_SELECT
set ::sqlite_options(configslower)          1  ;# CONFIG_SLOWDOWN_FACTOR, the default
set ::sqlite_options(conflict)              1  ;# unconditional in test_config.c
set ::sqlite_options(crashtest)             0  ;# the crash VFS, testfixture build only
set ::sqlite_options(cte)                   1  ;# present, not SQLITE_OMIT_CTE
set ::sqlite_options(curdir)                0  ;# SQLITE_OS_WINCE, not set here
set ::sqlite_options(cursorhints)           0  ;# SQLITE_ENABLE_CURSOR_HINTS
set ::sqlite_options(datetime)              0  ;# not SQLITE_OMIT_DATETIME_FUNCS
set ::sqlite_options(debug)                 0  ;# SQLITE_DEBUG
set ::sqlite_options(decltype)              1  ;# present, not SQLITE_OMIT_DECLTYPE
set ::sqlite_options(default_autovacuum)    0  ;# SQLITE_DEFAULT_AUTOVACUUM, numeric 0/1
set ::sqlite_options(default_ckptfullfsync) 0  ;# SQLITE_DEFAULT_CKPTFULLFSYNC
set ::sqlite_options(deprecated)            0  ;# SQLITE_OMIT_DEPRECATED
set ::sqlite_options(deserialize)           0  ;# SQLITE_OMIT_DESERIALIZE
set ::sqlite_options(direct_read)           0  ;# SQLITE_DIRECT_OVERFLOW_READ
set ::sqlite_options(dirsync)               0  ;# SQLITE_DISABLE_DIRSYNC
set ::sqlite_options(diskio)                0  ;# SQLITE_OMIT_DISKIO
set ::sqlite_options(explain)               1  ;# present, not SQLITE_OMIT_EXPLAIN
set ::sqlite_options(fast_secure_delete)    0  ;# SQLITE_FAST_SECURE_DELETE
set ::sqlite_options(floatingpoint)         1  ;# present, not SQLITE_OMIT_FLOATING_POINT
set ::sqlite_options(foreignkey)            0  ;# SQLITE_OMIT_FOREIGN_KEY
set ::sqlite_options(fts3)                  0  ;# SQLITE_ENABLE_FTS3
set ::sqlite_options(fts3_unicode)          0  ;# SQLITE_ENABLE_FTS3, unicode61 tokenizer
set ::sqlite_options(fts4_deferred)         0  ;# not SQLITE_DISABLE_FTS4_DEFERRED
set ::sqlite_options(fts5)                  0  ;# SQLITE_ENABLE_FTS5
set ::sqlite_options(geopoly)               0  ;# SQLITE_ENABLE_GEOPOLY
set ::sqlite_options(gettable)              0  ;# SQLITE_OMIT_GET_TABLE
set ::sqlite_options(has_codec)             0  ;# the SEE codec extension
set ::sqlite_options(hiddencolumns)         1  ;# SQLITE_ENABLE_HIDDEN_COLUMNS
set ::sqlite_options(icu)                   0  ;# SQLITE_ENABLE_ICU
set ::sqlite_options(icu_collations)        0  ;# SQLITE_ENABLE_ICU_COLLATIONS
set ::sqlite_options(incrblob)              0  ;# SQLITE_OMIT_INCRBLOB
set ::sqlite_options(integrityck)           0  ;# SQLITE_OMIT_INTEGRITY_CHECK
set ::sqlite_options(json1)                 0  ;# not SQLITE_OMIT_JSON
set ::sqlite_options(legacyformat)          0  ;# SQLITE_DEFAULT_FILE_FORMAT
set ::sqlite_options(lfs)                   0  ;# SQLITE_DISABLE_LFS
set ::sqlite_options(like_match_blobs)     1  ;# not SQLITE_LIKE_DOESNT_MATCH_BLOBS
set ::sqlite_options(like_opt)              1  ;# present, not SQLITE_OMIT_LIKE_OPTIMIZATION
set ::sqlite_options(load_ext)              0  ;# SQLITE_OMIT_LOAD_EXTENSION
set ::sqlite_options(localtime)             0  ;# SQLITE_OMIT_LOCALTIME
set ::sqlite_options(lock_proxy_pragmas)    0  ;# SQLITE_ENABLE_LOCKING_STYLE
set ::sqlite_options(lookaside)             0  ;# SQLITE_OMIT_LOOKASIDE
set ::sqlite_options(malloc_usable_size)    0  ;# HAVE_MALLOC_USABLE_SIZE
set ::sqlite_options(mathlib)               0  ;# SQLITE_ENABLE_MATH_FUNCTIONS
set ::sqlite_options(mem3)                  0  ;# SQLITE_ENABLE_MEMSYS3
set ::sqlite_options(mem5)                  0  ;# SQLITE_ENABLE_MEMSYS5
set ::sqlite_options(memdebug)              0  ;# SQLITE_MEMDEBUG
set ::sqlite_options(memorydb)              0  ;# SQLITE_OMIT_MEMORYDB
set ::sqlite_options(memorymanage)          0  ;# SQLITE_ENABLE_MEMORY_MANAGEMENT
set ::sqlite_options(mergesort)             1  ;# unconditional in test_config.c
set ::sqlite_options(mmap)                  0  ;# SQLITE_MAX_MMAP_SIZE, zero for now
set ::sqlite_options(multiplex_ext_overwrite) 0 ;# SQLITE_MULTIPLEX_EXT_OVWR
set ::sqlite_options(mutex)                 0  ;# SQLITE_MUTEX_OMIT
set ::sqlite_options(mutex_noop)            0  ;# SQLITE_MUTEX_NOOP
set ::sqlite_options(normalize)             0  ;# SQLITE_ENABLE_NORMALIZE
set ::sqlite_options(null_trim)             0  ;# SQLITE_ENABLE_NULL_TRIM
set ::sqlite_options(offset_sql_func)       0  ;# SQLITE_ENABLE_OFFSET_SQL_FUNC
set ::sqlite_options(or_opt)                1  ;# present, not SQLITE_OMIT_OR_OPTIMIZATION
set ::sqlite_options(ordered_set_aggregates) 0 ;# SQLITE_ENABLE_ORDERED_SET_AGGREGATES
set ::sqlite_options(ordered_set_funcs)     0  ;# SQLITE_ENABLE_ORDEREDSETFUNC
set ::sqlite_options(oversize_cell_check)   0  ;# SQLITE_ENABLE_OVERSIZE_CELL_CHECK
set ::sqlite_options(pagecache_overflow_stats) 0 ;# SQLITE_DISABLE_PAGECACHE_OVERFLOW_STATS
set ::sqlite_options(pager_pragmas)         0  ;# SQLITE_OMIT_PAGER_PRAGMAS
set ::sqlite_options(pragma)                1  ;# present, not SQLITE_OMIT_PRAGMA
set ::sqlite_options(prefer_proxy_locking)  0  ;# SQLITE_PREFER_PROXY_LOCKING
set ::sqlite_options(preupdate)             0  ;# SQLITE_ENABLE_PREUPDATE_HOOK
set ::sqlite_options(progress)              0  ;# SQLITE_OMIT_PROGRESS_CALLBACK
set ::sqlite_options(rbu)                   0  ;# SQLITE_ENABLE_RBU
set ::sqlite_options(reindex)               0  ;# SQLITE_OMIT_REINDEX
set ::sqlite_options(rowid32)               0  ;# SQLITE_32BIT_ROWID
set ::sqlite_options(rtree)                 0  ;# SQLITE_ENABLE_RTREE
set ::sqlite_options(rtree_int_only)        0  ;# SQLITE_RTREE_INT_ONLY
set ::sqlite_options(scanstatus)            0  ;# SQLITE_ENABLE_STMT_SCANSTATUS
set ::sqlite_options(schema_pragmas)        0  ;# SQLITE_OMIT_SCHEMA_PRAGMAS
set ::sqlite_options(schema_version)        0  ;# SQLITE_OMIT_SCHEMA_VERSION_PRAGMAS
set ::sqlite_options(secure_delete)         0  ;# SQLITE_SECURE_DELETE
set ::sqlite_options(session)               0  ;# SQLITE_ENABLE_SESSION
set ::sqlite_options(setlk_timeout)         0  ;# SQLITE_ENABLE_SETLK_TIMEOUT
set ::sqlite_options(shared_cache)          0  ;# SQLITE_OMIT_SHARED_CACHE
set ::sqlite_options(snapshot)              0  ;# SQLITE_ENABLE_SNAPSHOT
set ::sqlite_options(sqllog)                0  ;# SQLITE_ENABLE_SQLLOG
set ::sqlite_options(stat4)                 0  ;# SQLITE_ENABLE_STAT4
set ::sqlite_options(stmtvtab)              0  ;# SQLITE_ENABLE_STMTVTAB
set ::sqlite_options(subquery)              1  ;# present, not SQLITE_OMIT_SUBQUERY
set ::sqlite_options(tclvar)                0  ;# SQLITE_OMIT_TCL_VARIABLE
set ::sqlite_options(tempdb)                1  ;# not SQLITE_OMIT_TEMPDB
set ::sqlite_options(thread_misuse_warnings) 0 ;# SQLITE_THREAD_MISUSE_WARNINGS
set ::sqlite_options(threadsafe)            0  ;# SQLITE_THREADSAFE
set ::sqlite_options(threadsafe1)           0  ;# SQLITE_THREADSAFE==1
set ::sqlite_options(threadsafe2)           0  ;# SQLITE_THREADSAFE==2
set ::sqlite_options(trace)                 0  ;# SQLITE_OMIT_TRACE
set ::sqlite_options(trigger)               0  ;# SQLITE_OMIT_TRIGGER
set ::sqlite_options(truncate_opt)          1  ;# present, not SQLITE_OMIT_TRUNCATE_OPTIMIZATION
set ::sqlite_options(unlock_notify)         0  ;# SQLITE_ENABLE_UNLOCK_NOTIFY
set ::sqlite_options(update_delete_limit)   0  ;# SQLITE_ENABLE_UPDATE_DELETE_LIMIT
set ::sqlite_options(uri_00_error)          0  ;# SQLITE_ENABLE_URI_00_ERROR
set ::sqlite_options(utf16)                 0  ;# SQLITE_OMIT_UTF16
set ::sqlite_options(vacuum)                0  ;# SQLITE_OMIT_VACUUM
set ::sqlite_options(view)                  1  ;# present, not SQLITE_OMIT_VIEW
set ::sqlite_options(vtab)                  0  ;# SQLITE_OMIT_VIRTUALTABLE
set ::sqlite_options(wal)                   0  ;# SQLITE_OMIT_WAL
set ::sqlite_options(win32malloc)           0  ;# SQLITE_WIN32_MALLOC
set ::sqlite_options(windowfunc)            0  ;# SQLITE_OMIT_WINDOWFUNC
set ::sqlite_options(worker_threads)        0  ;# SQLITE_MAX_WORKER_THREADS
set ::sqlite_options(wsd)                   0  ;# SQLITE_OMIT_WSD
set ::sqlite_options(yytrackmaxstackdepth)  0  ;# YYTRACKMAXSTACKDEPTH

# Two more that the harness reads directly by name.
set ::sqlite_options(allow_rowid_in_view)   0  ;# SQLITE_ALLOW_ROWID_IN_VIEW

# ---------------------------------------------------------------------------
# Sanity check: the suite does not tolerate a missing entry, so the count is
# part of the contract. Fail loudly at source time rather than 200 files deep.
# ---------------------------------------------------------------------------
namespace eval ::nsqlite {
    variable capability_count [array size ::sqlite_options]
    if {$capability_count != 128} {
        # Names sorted, so the diff against a reference copy is readable.
        variable capability_names [lsort [array names ::sqlite_options]]
        return -code error \
            "sqlite_options has $capability_count entries, expected 128.\n\
             Check tools/capabilities.tcl against src/test_config.c at tag\n\
             version-3.53.4. Names: $capability_names"
    }
    variable capability_names [lsort [array names ::sqlite_options]]
    variable capabilities $capability_names
}

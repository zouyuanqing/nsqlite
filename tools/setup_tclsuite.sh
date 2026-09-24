#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# setup_tclsuite.sh — make the official SQLite TCL suite RUNNABLE on this
# machine, against a stock tclsh 8.6 + the stock sqlite3 Tcl extension that
# MSYS2 already ships. No Tcl compiler, no C compiler, no tcltest build needed.
#
# What it does:
#   1. Fetches the suite from the sqlite/sqlite GitHub mirror into ./testsuite.
#   2. Writes tcl/options.tcl  — the 128-entry ::sqlite_options array that the
#      testfixture binary normally gets from src/test_config.c, rewritten for
#      the feature set nsqlite will have.
#   3. Writes tcl/testfixture_ext.tcl — pure-Tcl stand-ins for the C commands
#      that tester.tcl calls at SOURCE time.
#   4. Writes run_one.tcl — the driver: package require sqlite3, source the
#      shims, source tester.tcl, run one .test file, report the pass count.
#
# Usage:
#   ./tools/setup_tclsuite.sh                 # fetch + write
#   ./testsuite/run_one.tcl <name.test>       # run one file
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITE="$ROOT/testsuite"
MIRROR="https://codeload.github.com/sqlite/sqlite/tar.gz/refs/heads/master"

# --- 1. fetch ----------------------------------------------------------------
if [ ! -d "$SUITE/test" ]; then
  echo "fetching suite from $MIRROR"
  mkdir -p "$SUITE"
  curl -sSL --max-time 600 -o "$SUITE/_s.tar.gz" "$MIRROR"
  tar xzf "$SUITE/_s.tar.gz" -C "$SUITE"
  # Normalise sqlite-<hash>/ to testsuite/
  inner="$(find "$SUITE" -maxdepth 1 -type d -name 'sqlite-*' | head -1)"
  if [ -n "$inner" ]; then
    tmp="$SUITE/._x"; mkdir -p "$tmp"
    mv "$inner"/* "$tmp"/ 2>/dev/null || true
    rm -rf "$inner"
    mv "$tmp"/* "$SUITE"/ 2>/dev/null || true
    rmdir "$tmp" 2>/dev/null || true
  fi
  rm -f "$SUITE/_s.tar.gz"
fi
echo "suite at: $SUITE  ($(ls "$SUITE"/test/*.test | wc -l) .test files)"

# --- 2/3/4. shim files -------------------------------------------------------
mkdir -p "$SUITE/tcl"

cat > "$SUITE/tcl/options.tcl" <<'TCL'
# The ::sqlite_options array, exactly as src/test_config.c builds it, but
# describing nsqlite instead of a C build.
#
# `ifcapable FOO ...` rewrites the word FOO into `$::sqlite_options(FOO)`
# (tester.tcl:1690-1734), so a 0 makes the guarded block self-skip. Only
# capabilities nsqlite actually has are 1. The array is COMPLETE (all 128
# names) so a test consulting an unlisted capability evaluates to 0 rather
# than raising "no such element in array".
array set ::sqlite_options {
  malloc_usable_size               0
  rowid32                          0
  allow_rowid_in_view              0
  carray                           0
  casesensitivelike                0
  configslower                     1.0
  curdir                           1
  win32malloc                      0
  debug                            0
  default_ckptfullfsync            0
  direct_read                      0
  dirsync                          0
  lfs                              1
  pagecache_overflow_stats         1
  mmap                             0
  worker_threads                   0
  memdebug                         0
  {8_3_names}                      0
  cursorhints                      0
  hiddencolumns                    0
  deserialize                      0
  mathlib                          0
  mem3                             0
  mem5                             0
  offset_sql_func                  0
  ordered_set_aggregates           0
  preupdate                        0
  snapshot                         0
  mutex                            1
  mutex_noop                       0
  altertable                       0
  analyze                          0
  api_armor                        0
  atomicwrite                      0
  geopoly                          0
  json1                            0
  has_codec                        0
  like_match_blobs                 1
  attach                           0
  auth                             0
  autoinc                          1
  autoindex                        1
  autoreset                        0
  autovacuum                       1
  default_autovacuum               0
  between_opt                      1
  builtin_test                     0
  bloblit                          1
  cast                             1
  check                            0
  cte                              0
  columnmetadata                   0
  ordered_set_funcs                0
  oversize_cell_check              0
  compileoption_diags              0
  complete                         1
  compound                         1
  conflict                         1
  crashtest                        1
  datetime                         0
  decltype                         0
  deprecated                       1
  diskio                           0
  explain                          0
  floatingpoint                    1
  foreignkey                       0
  fts3                             0
  fts5                             0
  fts3_unicode                     0
  fts4_deferred                    0
  gettable                         0
  icu                              0
  icu_collations                   0
  incrblob                         0
  integrityck                      1
  legacyformat                     0
  like_opt                         1
  load_ext                         0
  localtime                        0
  lookaside                        1
  memorydb                         1
  memorymanage                     0
  mergesort                        1
  null_trim                        0
  or_opt                           1
  rbu                              0
  pager_pragmas                    1
  pragma                           1
  progress                         0
  reindex                          0
  rtree                            0
  rtree_int_only                   0
  schema_pragmas                   1
  schema_version                   1
  session                          0
  stat4                            0
  stmtvtab                         0
  scanstatus                       0
  lock_proxy_pragmas               0
  prefer_proxy_locking             0
  shared_cache                     0
  subquery                         1
  tclvar                           0
  threadsafe                       0
  threadsafe1                      0
  threadsafe2                      0
  tempdb                           1
  trace                            0
  thread_misuse_warnings           0
  trigger                          0
  truncate_opt                     1
  utf16                            0
  vacuum                           0
  view                             0
  vtab                             0
  wal                              0
  wsd                              0
  update_delete_limit              0
  unlock_notify                    0
  fast_secure_delete               0
  secure_delete                    0
  multiplex_ext_overwrite          0
  yytrackmaxstackdepth             0
  sqllog                           0
  uri_00_error                     0
  normalize                        0
  windowfunc                       0
  setlk_timeout                    0
}
TCL

cat > "$SUITE/tcl/testfixture_ext.tcl" <<'TCL'
# Pure-Tcl stand-ins for the C commands that the `testfixture` binary provides
# and a stock sqlite3 Tcl extension does not. Only the commands tester.tcl
# invokes at SOURCE time are required to get the harness to load; everything
# else a test file touches will raise "invalid command name", which is the
# honest signal that a capability is missing.

# -- source-time bootstrap. tester.tcl:102 calls the first of these at the top
#    level, and a missing command ABORTS the whole `source`, silently leaving
#    every later proc (do_test, execsql, capable, ifcapable) undefined.
proc sqlite3_test_control_pending_byte {args} { return }
proc load_testfixture_extensions        {args} { return }
proc install_malloc_faultsim           {args} { return }
proc autoinstall_test_functions         {args} { return }

# -- init / memory / status
proc sqlite3_shutdown                 {args} { return }
proc sqlite3_initialize               {args} { return }
proc sqlite3_config_memstatus         {args} { return }
proc sqlite3_soft_heap_limit64        {args} { return }
proc sqlite3_hard_heap_limit64        {args} { return }
proc sqlite3_memdebug_settitle        {args} { return }
proc sqlite3_memdebug_backtrace       {args} { return }
proc sqlite3_memdebug_dump            {args} { return }
proc sqlite3_memdebug_log             {args} { return }
proc sqlite3_memdebug_malloc_count    {args} { return 0 }
proc sqlite3_memory_used              {args} { return 0 }
proc sqlite3_memory_highwater         {args} { return 0 }
proc sqlite3_status                   {args} { return {0 0 0} }
proc sqlite3_enable_shared_cache      {args} { return 0 }
proc sqlite3_reset_auto_extension     {args} { return }

# -- handle introspection. These return inert values; tests that genuinely need
#    the raw C API through ::DB must be omitted rather than silently passed.
proc sqlite3_connection_pointer {args} { return 0 }
proc sqlite3_db_config            {args} { return 0 }
proc sqlite3_test_control         {args} { return 0 }
proc sqlite3_limit                {args} { return 0 }
proc sqlite3_config               {args} { return 0 }

# -- called at source time by tester.tcl:2620-2621 (the last source-time block,
#    immediately before it sources thread_common.tcl / malloc_common.tcl) ------
proc database_never_corrupt      {args} { return }
proc database_can_be_corrupt     {args} { return }
proc extra_schema_checks         {args} { return }

# -- called by the per-file runner path (tester.tcl slave_test_file, and the
#    finish_test path). Not all of these are strictly "source time", but every
#    one of them aborts a whole .test file if missing, so they are cheaper to
#    stub than to chase.
proc reset_prng_state            {args} { return }
proc save_prng_state             {args} { return }
proc restore_prng_state          {args} { return }
proc vfs_unlink_test             {args} { return }
proc db_enter                    {args} { return }
proc db_leave                    {args} { return }
proc sqlite3_clear_tsd_memdebug  {args} { return }

# -- fpnum_compare is a testfixture C command (src/test1.c) used by do_test to
#    compare floating-point results with a tolerance. tester.tcl:784 falls back
#    to it whenever a plain string compare fails, so EVERY numeric test hits it.
#    A faithful-enough pure-Tcl version: compare as doubles, allowing the same
#    relative tolerance the C one uses.
proc fpnum_compare {a b} {
    if {$a eq $b} { return 1 }
    if {[catch {expr {double($a) - double($b)}} d]} { return 0 }
    set m [expr {abs(double($a))}]
    if {$m < 1.0} { set m 1.0 }
    return [expr {abs($d) <= 1.0e-5*$m}]
}
proc tcl_variable_type {args} { return "" }
proc sqlite3_test_errstr {args} { return "" }
proc number_of_cores {args} { return 1 }
proc working_64bit_int {args} { return 1 }
proc uses_stmt_journal {args} { return 1 }
proc doublearray_addr {args} { return 0 }
proc intarray_addr {args} { return 0 }
proc int64array_addr {args} { return 0 }
proc textarray_addr {args} { return 0 }
proc decode_hexdb {args} { return "" }
proc test_write_db {args} { return }
proc database_may_be_corrupt {args} { return }

# -- The read-only integer limits that src/test_config.c publishes with
#    Tcl_LinkVar (its LINKVAR macro, lines 805-837). tester.tcl and
#    permutations.test read several of them, so a missing one aborts the whole
#    file. These are the stock SQLite defaults from src/sqliteLimit.h; they
#    describe the ENGINE UNDER TEST, so they must eventually track whatever
#    nsqlite actually implements rather than being left at SQLite's values.
set ::SQLITE_MAX_LENGTH                1000000000
set ::SQLITE_MAX_COLUMN                2000
set ::SQLITE_MAX_SQL_LENGTH            1000000000
set ::SQLITE_MAX_EXPR_DEPTH            1000
set ::SQLITE_MAX_COMPOUND_SELECT       500
set ::SQLITE_MAX_VDBE_OP               250000000
set ::SQLITE_MAX_FUNCTION_ARG          127
set ::SQLITE_MAX_VARIABLE_NUMBER       32766
set ::SQLITE_MAX_PAGE_SIZE             65536
set ::SQLITE_MAX_PAGE_COUNT            4294967294
set ::SQLITE_MAX_LIKE_PATTERN_LENGTH   50000
set ::SQLITE_MAX_TRIGGER_DEPTH         1000
set ::SQLITE_DEFAULT_CACHE_SIZE        -2000
set ::SQLITE_DEFAULT_PAGE_SIZE         4096
set ::SQLITE_DEFAULT_FILE_FORMAT       4
set ::SQLITE_DEFAULT_SYNCHRONOUS       2
set ::SQLITE_DEFAULT_WAL_SYNCHRONOUS   2
set ::SQLITE_MAX_ATTACHED              10
set ::SQLITE_MAX_DEFAULT_PAGE_SIZE     65536
set ::SQLITE_MAX_WORKER_THREADS        0
set ::SQLITE_MAX_SCHEMA                0
set ::SQLITE_MAX_TRIGGER_STEPS         1000000
set ::TEMP_STORE                       0

# -- load_testfixture_extensions INTERP
#    tester.tcl:2379 calls this in EVERY slave interpreter, passing the handle
#    of the new interpreter, immediately before sourcing the .test file into
#    it. A slave has a clean global namespace: nothing from the parent leaks
#    in. The sqlite3 Tcl extension, ::sqlite_options and every stub above must
#    therefore be re-established INSIDE the slave, and this hook is the only
#    place in the harness that ever gets the slave interpreter's handle.
#
#    ::nsqlite_tcl_suite_dir is set by run_one.tcl before this file is sourced;
#    when this file is sourced again INSIDE a slave it must not overwrite it.
if {![info exists ::nsqlite_tcl_suite_dir]} {
    set ::nsqlite_tcl_suite_dir [file dirname [file dirname [info script]]]
}
proc load_testfixture_extensions {interp} {
    global nsqlite_tcl_suite_dir
    # NB: inside `interp eval` the script runs in the SLAVE, so the parent's
    # value must be substituted here, in the parent, not referenced from inside.
    interp eval $interp [subst -nocommands {
        set ::nsqlite_tcl_suite_dir {$::nsqlite_tcl_suite_dir}
        lappend auto_path [file join $::nsqlite_tcl_suite_dir tcl]
        if {[catch {package require sqlite3}]} {
            error "slave interpreter: no sqlite3 Tcl extension"
        }
    }]
    # Re-seed the capability array in the slave.
    interp eval $interp [list array set ::sqlite_options [array get ::sqlite_options]]
    # Re-define the stubs in the slave.
    interp eval $interp [list source \
        [file join $::nsqlite_tcl_suite_dir tcl testfixture_ext.tcl]]
    return
}
TCL

cat > "$SUITE/run_one.tcl" <<'TCL'
#!/usr/bin/env tclsh
# Drive ONE or more official .test files under a stock tclsh.
#   usage: tclsh run_one.tcl <name.test> [<name.test> ...]
set suite [file normalize [file dirname $argv0]]

lappend auto_path [file join $suite tcl]

# The real sqlite3 Tcl extension. On this machine MSYS2 ucrt64 already ships
# one (3.53.4), which is what makes this whole approach possible.
if {[catch {package require sqlite3}]} {
  puts stderr "FATAL: no sqlite3 Tcl extension. Need `package require sqlite3`."
  exit 1
}

# Fill ::sqlite_options BEFORE tester.tcl reads it. Under a real testfixture
# the C code sets this; under plain tclsh it is empty and every `ifcapable`
# block errors out.
source [file join $suite tcl options.tcl]
set ::nsqlite_tcl_suite_dir $suite
source [file join $suite tcl testfixture_ext.tcl]

set testdir [file join $suite test]

# Capture the requested files BEFORE tester.tcl resets $argv (it does so at
# line 505, once it has consumed its own command-line switches).
set ::NSQL_SUITE_FILES $argv

# tester.tcl derives the scratch dir from $argv0's directory, so point argv0
# at the test dir and cd into a scratch area for the .db files.
set scratch [file join $suite scratch]
file mkdir $scratch
cd $scratch
set argv {}
set argv0 [file join $testdir [lindex $::NSQL_SUITE_FILES 0]]

# testrunner.tcl sources permutations.test BEFORE tester.tcl (testrunner.tcl:20),
# and permutations.test itself sources tester.tcl at its line 15. Reproduce
# that order, otherwise permutations.test's own `db close` (line 18) kills the
# handle that tester.tcl's reset_db just created.
namespace eval ::trd {}
set ::trd::tcltest 1
if {[file readable [file join $testdir permutations.test]]} {
  source [file join $testdir permutations.test]
}
unset ::trd::tcltest
source [file join $testdir tester.tcl]

# tester.tcl installs a stdout filter that owns the terminal. Turn it off so
# the summary below is the only thing printed, and so output is captured.
set ::G(verbose) 0

foreach f $::NSQL_SUITE_FILES {
  set file [file join $testdir $f]
  puts stderr "--- $f ---"
  set ::TC(count) 0
  set ::TC(errors) 0
  set ::TC(fail_list) [list]
  set ::TC(omit_list) [list]
  if {[catch {uplevel #0 [list source $file]} err]} {
    puts stderr "  SOURCE ERROR: $err"
  }
  set n $::TC(count)
  set e [llength $::TC(fail_list)]
  set o [llength $::TC(omit_list)]
  puts stderr "  RESULT $f tests=$n errors=$e omitted=$o"
  if {$e} { foreach t $::TC(fail_list) { puts stderr "    ! $t" } }
}
TCL

echo "wrote:"
echo "  $SUITE/tcl/options.tcl"
echo "  $SUITE/tcl/testfixture_ext.tcl"
echo "  $SUITE/run_one.tcl"
echo
echo "Run one file:"
echo "  tclsh $SUITE/run_one.tcl main.test"

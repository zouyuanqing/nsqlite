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

# -- Remaining testfixture C commands (src/test1.c's command table). These
#    are inert: they exist so a test file does not abort on the first line.
#    Any test that actually asserts on one of them will fail, which is the
#    honest outcome. The raw sqlite3_* C-API commands (sqlite3_step,
#    sqlite3_column_*, sqlite3_bind_*, ...) are deliberately NOT stubbed:
#    they need a real sqlite3* handle, and stubbing them would let a test
#    report success while the engine did nothing.
proc load_static_extension {args} { return }
proc add_test_collate {args} { return }
proc add_test_collate_needed {args} { return }
proc add_test_function {args} { return }
proc add_test_utf16bin_collate {args} { return }
proc add_alignment_test_collations {args} { return }
proc atomic_batch_write {args} { return }
proc create_null_module {args} { return }
proc register_dbstat_vtab {args} { return }
proc optimization_control {args} { return }
proc print_explain_query_plan {args} { return }
proc pcache_stats {args} { return }
proc sqlite3_vfs_list {args} { return }
proc sqlite3_create_function_v2 {args} { return }
proc sqlite3_create_function {args} { return }
proc sqlite3_create_collation {args} { return }
proc sqlite3_rekey {args} { return }
proc sqlite3_db_readonly_ {args} { return }
proc sqlite3_wal_checkpoint {args} { return }
proc sqlite3_wal_checkpoint_v2 {args} { return }
proc sqlite3_wal_autocheckpoint {args} { return }
proc sqlite3_unlock_notify {args} { return }
proc sqlite3_mmap_warm {args} { return }
proc sqlite3_pager_refcounts {args} { return }
proc sqlite3_expanded_sql {args} { return }
proc sqlite3_normalize {args} { return }
proc sqlite3_normalized_sql {args} { return }
proc sqlite3_stmt_explain {args} { return }
proc sqlite3_stmt_isexplain {args} { return }
proc sqlite3_stmt_readonly {args} { return }
proc sqlite3_stmt_status {args} { return }
proc sqlite3_global_recover {args} { return }
proc sqlite3_delete_database {args} { return }
proc sqlite3_db_filename {args} { return }
proc sqlite3_db_readonly {args} { return }
proc sqlite3_db_cacheflush {args} { return }
proc sqlite3_db_release_memory {args} { return }
proc sqlite3_sleep {args} { return }
proc sqlite3_system_errno {args} { return }
proc sqlite3_error_offset {args} { return }
proc sqlite3_expired {args} { return }
proc sqlite3_set_errmsg {args} { return }
proc sqlite3_txn_state {args} { return }
proc sqlite3_transfer_bindings {args} { return }
proc sqlite3_thread_cleanup {args} { return }
proc sqlite3_libversion_number {args} { return }
proc sqlite3_table_column_metadata {args} { return }
proc vfs_initfail_test {args} { return }
proc vfs_unregister_all {args} { return }
proc vfs_reregister_all {args} { return }
proc vfs_current_time_int64 {args} { return }
proc file_control_test {args} { return }
proc file_control_chunksize_test {args} { return }
proc file_control_sizehint_test {args} { return }
proc file_control_data_version {args} { return }
proc file_control_persist_wal {args} { return }
proc file_control_powersafe_overwrite {args} { return }
proc file_control_vfsname {args} { return }
proc file_control_reservebytes {args} { return }
proc file_control_tempfilename {args} { return }
proc file_control_external_reader {args} { return }
proc file_control_lockproxy_test {args} { return }
proc file_control_lasterrno_test {args} { return }
proc strftime {args} { return }
proc tcl_objproc {args} { return }
proc sorter_test_fakeheap {args} { return }
proc sorter_test_sort4_helper {args} { return }
proc test_sqlite3_log {args} { return }
proc prng_seed {args} { return }
proc filc_build {args} { return }
proc .treetrace {args} { return }
proc bind_carray_intptr {args} { return }
proc dbconfig_maindbname_icecube {args} { return }
proc file_control_win32_av_retry {args} { return }
proc file_control_win32_get_handle {args} { return }
proc file_control_win32_set_handle {args} { return }
proc getrusage {args} { return {0 0 0} }
proc lock_win32_file {args} { return }

# -- Secondary testfixture modules (src/test_vfs.c, test_hexio.c,
#    test_demovfs.c, test_jt_vfs.c, test_bestindex.c, test_sqllog.c,
#    test_async.c, test_ctrl.c, test_malloc.c). Like the set above these are
#    inert placeholders so a .test file does not die on line 1.
# `testvfs NAME ...` registers a scriptable VFS and creates a Tcl command
# called NAME. Tests then call `NAME script`, `NAME filter`, `NAME delete`.
# Creating that command is what the following lines depend on, so the stub has
# to actually do it; the VFS itself is never installed, so any test that
# relies on its behaviour will fail rather than pass falsely.
namespace eval ::nsqlite {}
proc testvfs {name args} {
    if {$name eq "" || [string index $name 0] eq "-"} { return }
    interp alias {} $name {} ::nsqlite::tvfs_stub
    return
}
proc ::nsqlite::tvfs_stub {args} {
    # `delete` must actually remove the command, like the real one.
    if {[lindex $args 0] eq "delete"} {
        catch {interp alias {} [lindex $args 0] {}}
        return
    }
    return
}
proc vfslog {args} { return }
proc vdbe_coverage {args} { return }
proc md5 {args} { return }
proc md5file {args} { return }
proc btree_insert {args} { return }
proc btree_from_db {args} { return }
proc btree_pager_stats {args} { return }
proc btree_set_cache_size {args} { return }
proc sqlthread {args} { return }
proc clock_seconds {args} { return }
proc register_demovfs {args} { return }
proc unregister_demovfs {args} { return }
proc register_jt_vfs {args} { return }
proc unregister_jt_vfs {args} { return }
proc register_cube_geom {args} { return }
proc register_circle_geom {args} { return }
proc sqlite3_crash_enable {args} { return }
proc sqlite3_crash_now {args} { return }
proc sqlite3_crash_on_write {args} { return }
proc sqlite3_crashparams {args} { return }
proc sqlite3_simulate_device {args} { return }
proc vfs_set_readmark {args} { return }
proc vfs_shmlock {args} { return }
proc sqlite3_auto_extension_sqr {args} { return }
proc sqlite3_auto_extension_cube {args} { return }
proc sqlite3_auto_extension_broken {args} { return }
proc sqlite3_cancel_auto_extension_sqr {args} { return }
proc sqlite3_cancel_auto_extension_cube {args} { return }
proc sqlite3_cancel_auto_extension_broken {args} { return }
proc sqlite3demo_superlock {args} { return }
proc sqlite3_blocking_step {args} { return }
proc sqlite3_backup {args} { return }
proc sqlite3_install_memsys3 {args} { return }
proc sqlite3_config_alt_pcache {args} { return }
proc sqlite3_config_heap {args} { return }
proc sqlite3_config_lookaside {args} { return }
proc sqlite3_config_pagecache {args} { return }
proc sqlite3_config_sorterref {args} { return }
proc sqlite3_db_config_lookaside {args} { return }
proc sqlite3_exec_nr {args} { return }
proc sqlite3_memdebug_fail {args} { return }
proc sqlite3_win_test_unc_locking {args} { return }
proc sqlite3_vfs {args} { return }
proc sqlite3_autovacuum_pages {args} { return }

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
  # $f is resolved to an absolute path in the PARENT: the child interpreter
  # changes directory (tester.tcl cds into a scratch dir), so a bare file name
  # would not resolve there.
  set f [file join $testdir $f]
  puts stderr "--- [file tail $f] ---"
  set ::TC(count) 0
  set ::TC(errors) 0
  set ::TC(fail_list) [list]
  set ::TC(omit_list) [list]
  # A .test file is a plain Tcl script, and tester.tcl can call [abort] on
  # some conditions, so running every file in this one interpreter lets one
  # file tear down the whole loop. Run each file in a child interpreter, whose
  # death cannot affect the parent.
  set ::TC(errors) 0
  if {[catch {
      interp create nfile
      # The child needs the real sqlite3 extension: tester.tcl:115 renames
      # the [sqlite3] command, which aborts the whole source if it is absent.
      interp eval nfile [list lappend auto_path [file join $suite tcl]]
      interp eval nfile {package require sqlite3}
      interp eval nfile [list set ::argv0 $::argv0]
      interp eval nfile [list set ::argv  [list $f]]
      # tester.tcl:496 reads $testdir before deriving it, and every .test file
      # opens with `set testdir [file dirname $argv0]`, so seed it here.
      interp eval nfile [list set ::testdir $testdir]
      interp eval nfile [list set ::nsqlite_name [file tail $f]]
      # Tests that drive the raw C API set ::STMT / ::DB from a real
      # sqlite3_connection_pointer. Provide inert placeholders so such a file
      # runs and FAILS on its assertions instead of aborting on a missing var.
      if {![info exists ::nsqlite_seeded]} { set ::nsqlite_seeded 1 }
      interp eval nfile {if {![info exists ::STMT]} {set ::STMT 0}}
      interp eval nfile {if {![info exists ::DB]}  {set ::DB  0}}
      interp eval nfile [list array set ::G [array get ::G]]
      interp eval nfile [list source [file join $suite tcl options.tcl]]
      interp eval nfile [list set ::nsqlite_tcl_suite_dir $suite]
      interp eval nfile [list source [file join $suite tcl testfixture_ext.tcl]]
      interp eval nfile [list source [file join $testdir tester.tcl]]
      # The file's tests run inside SLAVE interpreters (tester.tcl's
      # slave_test_file), and the counters are kept in the PARENT of those
      # slaves, i.e. in `nfile`. So the totals have to be sampled after the
      # file finishes, out of the child rather than out of this process.
      # A .test file ends by calling finish_test, which (in the plain sqlite3
      # extension) prints the summary and returns; it only calls [exit] under
      # testrunner. Either way the child may end before the parent can read its
      # counters, so the child itself writes the result line to stderr and the
      # parent just relays it.
      interp eval nfile [list proc ::nsqlite_report {} {
        puts stderr "  RESULT $::nsqlite_name tests=$::TC(count) errors=[llength $::TC(fail_list)] omitted=[llength $::TC(omit_list)]"
        foreach t $::TC(fail_list) { puts stderr "    ! $t" }
      }]
      # Replace finish_test (defined by tester.tcl) with a reporter, so the
      # child's own summary is emitted and the child does not exit.
      interp eval nfile {rename finish_test {}}
      interp eval nfile {proc finish_test {args} { ::nsqlite_report }}
      interp eval nfile [list source $f]
      interp delete nfile
  } err opts]} {
    catch {interp delete nfile}
    puts stderr "  SOURCE ERROR: $err"
    if {[dict exists $opts -errorinfo]} {
        puts stderr [string range [dict get $opts -errorinfo] 0 500]
    }
    set n 0; set e 1; set o 0
  }
}
TCL

echo "wrote:"
echo "  $SUITE/tcl/options.tcl"
echo "  $SUITE/tcl/testfixture_ext.tcl"
echo "  $SUITE/run_one.tcl"
echo
echo "Run one file:"
echo "  tclsh $SUITE/run_one.tcl main.test"

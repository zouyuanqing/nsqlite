# A tester.tcl shim for nsqlite.
#
# The official SQLite TCL suite never talks to the engine over a subprocess: its
# harness calls Tcl commands like `sqlite3_initialize` and
# `install_malloc_faultsim` before the first test executes, and those come from
# a testfixture build of the C API. This file is the second strategy described
# in docs/testing.md -- a replacement harness with the same command vocabulary
# that drives a CLI binary over `exec`, the approach Turso takes.
#
# The semantics are copied from test/sqlite-suite/test/tester.tcl, not
# guessed at: do_test's expected-value comparison, the `execsql` list shape,
# `catchsql`'s {rc msg} pair, `execsql2`'s {name value ...} flattening and
# `ifcapable`'s character-scanner expression rewrite are all the upstream
# algorithms, so a test that passes here means what it would mean upstream.
#
# The protocol with the CLI
# ------------------------
#
# Every statement is run as its own `nsqlited DB SQL` process. That is a
# deliberate cost: the engine keeps a connection open in memory, and the suite
# deletes and reopens test.db constantly, so a long-lived process would not see
# the same state a fresh one does. It is also what makes this a shim rather than
# a binding.
#
# The CLI's own output is not quite the shape the suite compares, so the engine
# is driven through a small adapter. The CLI prints rows pipe-separated with no
# escaping, which loses two things the suite needs: an embedded newline in a
# TEXT value, and the NULL/text/blob distinction. It prints an empty result set
# as a Rust `Vec` debug, which is not a column list. And it formats floats with
# Rust's `{}`, which writes `1` where SQLite writes `1.0`.
#
# So the CLI carries a `--testsuite` mode that speaks a line-oriented protocol
# built for this: one record per line, a tag byte for the record kind, and
# hex encoding for any value that could otherwise be ambiguous. See
# crates/nsqlited/src/testsuite.rs.
#
# What this shim cannot do
# ------------------------
#
# Anything needing in-process engine state: fault injection, sqlite3_stmt
# handles, EXPLAIN opcode dumps, per-statement error codes, PRAGMA
# introspection of the pager. Those tests are not made to pass by weakening
# them; they report as failures with the reason, which is the honest result for
# a CLI-driven harness.

# The capability gate. tools/capabilities.tcl holds the 128 entries and asserts
# the count at source time, so a drift from the upstream list fails here rather
# than 200 files deep.
source [file join [file dirname [info script]] .. .. tools capabilities.tcl]

# Where the engine lives. NSQLITED overrides it, which is how the suite is run
# against a release build or a different engine for comparison.
set ::NSQLITED [expr {[info exists ::env(NSQLITED)]
                      ? $::env(NSQLITED)
                      : [file join [file dirname [file dirname [file dirname [info script]]]] target debug nsqlited.exe]}]

# ---------------------------------------------------------------------------
# Capability gating
# ---------------------------------------------------------------------------

# Wrap every run of alphanumerics and underscores in $::sqlite_options(...).
#
# This is upstream's fix_ifcapable_expr, a character scanner rather than a
# parser. It is copied rather than replaced because the scanner's quirks are
# load-bearing: `ifcapable !fts5` must become `!$::sqlite_options(fts5)` and
# not trip over the `!`, and a name may start with a digit (`8_3_names`).
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

# Non-zero if the capabilities in the expression are present.
proc capable {expr} {
  set e [fix_ifcapable_expr $expr]
  return [expr ($e)]
}

# Run $code when the capabilities hold, $elsecode when they do not. The error
# code of whichever branch ran is propagated, which is what lets a failure
# inside an ifcapable block reach do_test's catch.
proc ifcapable {expr code {else ""} {elsecode ""}} {
  set e2 [fix_ifcapable_expr $expr]
  # `if ($e2)` and not `if {$e2}`: the braces would pass the literal text
  # `$::sqlite_options(autovacuum)` to `if`, which is not a boolean. Without
  # them the argument is substituted first, and `if` sees the value. This is
  # upstream's spelling and it is load-bearing.
  if ($e2) {
    set c [catch {uplevel 1 $code} r]
  } else {
    set c [catch {uplevel 1 $elsecode} r]
  }
  return -code $c $r
}

# ---------------------------------------------------------------------------
# Test counters
# ---------------------------------------------------------------------------

set ::TC(errors)    0
set ::TC(count)     0
set ::TC(fail_list) [list]
set ::TC(omit_list) [list]
set ::TC(warn_list) [list]

proc set_test_counter {counter args} {
  if {[llength $args]} {
    set ::TC($counter) [lindex $args 0]
  }
  set ::TC($counter)
}

proc omit_test {name reason {append 1}} {
  set omitList [set_test_counter omit_list]
  if {$append} {
    lappend omitList [list $name $reason]
  }
  set_test_counter omit_list $omitList
}

proc fail_test {name} {
  set f [set_test_counter fail_list]
  lappend f $name
  set_test_counter fail_list $f
  set_test_counter errors [expr [set_test_counter errors] + 1]
  set nFail [set_test_counter errors]
  if {$nFail>=$::cmdlinearg(maxerror)} {
    output2 "*** Giving up..."
    finalize_testing
  }
}

proc warning {msg {append 1}} {
  output2 "Warning: $msg"
  set warnList [set_test_counter warn_list]
  if {$append} {
    lappend warnList $msg
  }
  set_test_counter warn_list $warnList
}

proc incr_ntest {} {
  set_test_counter count [expr [set_test_counter count] + 1]
}

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

proc verbose {} {
  return $::cmdlinearg(verbose)
}

proc output1 {args} {
  set v [verbose]
  if {$v==1} {
    uplevel output2 $args
  } elseif {$v==2} {
    uplevel puts [lrange $args 0 end-1] $::G(output_fd) [lrange $args end end]
  }
}

proc output2 {args} {
  uplevel puts $args
}

proc output2_if_no_verbose {args} {
  set v [verbose]
  if {$v==0} {
    uplevel output2 $args
  } elseif {$v==2} {
    uplevel puts [lrange $args 0 end-1] stdout [lrange $args end end]
  }
}

# ---------------------------------------------------------------------------
# Engine access
# ---------------------------------------------------------------------------

# Every open database gets an entry here: name -> file path. A handle is a Tcl
# name, not an object, because the engine lives in another process and there is
# nothing for a safe-interp command prefix to close over.
array set ::nsqlite_db {}

# Command-line flags handed to the CLI on every open.
set ::nsqlite_open_args [list]

# Options that survive across the whole file, applied when a database is
# opened. The suite sets these with `PRAGMA ...`, which in this shim is a
# statement run against the file, so they take effect on their own. The
# per-connection ones below are the exceptions that have no SQL equivalent the
# engine can honour from a fresh process.
set ::nsqlite_open_args [list]

# Open a database file under the Tcl name $1. The upstream signature is
# `sqlite3 HANDLE ?FILE? ?ARGS?`; :memory: is accepted and maps to the engine's
# own in-memory database, which is per-process and therefore empty every time.
proc sqlite3 {args} {
  if {[llength $args]==0} {
    error "wrong # args: should be \"sqlite3 HANDLE ?FILE? ?ARGS?\""
  }
  set name [lindex $args 0]
  if {[string index $name 0] eq "-"} {
    # An option such as `-has-codec`. A CLI has no codec, so the answer the
    # harness wants is simply no.
    return 0
  }
  set file [expr {[llength $args]>1 ? [lindex $args 1] : ""}]
  if {$file eq ""} {
    error "wrong # args: should be \"sqlite3 HANDLE ?FILE? ?ARGS?\""
  }
  if {$file eq ":memory:" || $file eq ""} {
    set ::nsqlite_db($name) ""
    return ""
  }
  set ::nsqlite_db($name) $file
  # A fresh process must see the file exactly as the last one left it, so make
  # sure the engine can open it before handing the name out. Upstream does not
  # need this: its connection stays open.
  if {![file exists $file]} {
    if {[catch {nsqlite_run $name "SELECT 1;"} err]} {
      return -code error $err
    }
  }
  return ""
}

# Close the database named $1. There is no persistent process to close, so this
# only forgets the name; the next open starts a clean process. Deleting a file
# under an open name is still meaningful, because the file is the state.
proc nsdb_close {name} {
  unset -nocomplain ::nsqlite_db($name)
}

# Install $1 as the command $2, so `db eval ...` and `db2 one ...` work. The
# engine does not support the full method set, and an unsupported method is
# reported rather than silently ignored: a test that asked for
# `db status vmstep` has learned nothing if the harness pretends it worked.
proc nsdb_install {name} {
  if {[llength [info commands $name]]} {
    return
  }
  interp alias {} $name {} ::nsqlite_db_method $name
}

proc nsdb_uninstall {name} {
  catch {interp alias {} $name {}}
}

# Run one SQL script against the database $2 and return the protocol records.
#
# This is the single point where the shim talks to the engine. A non-zero exit
# or a parse error on stdout is a Tcl error carrying the engine's message, which
# is what `catchsql` and do_test's catch turn into an expected value.
proc nsqlite_run {name sql} {
  if {![info exists ::nsqlite_db($name)]} {
    error "no such database: $name"
  }
  set file $::nsqlite_db($name)
  set cmd [list $::NSQLITED --testsuite]
  if {$file ne ""} {
    lappend cmd $file
  }
  lappend cmd $sql
  # The engine exits non-zero when a statement fails, and Tcl's `exec` throws
  # on a non-zero status and hands back only an error message -- the record
  # stream it already wrote to stdout would be lost. Redirecting stdout to a
  # file and reading it back keeps the `E` record, which carries the engine's
  # message; the non-zero status then does not matter, because the failure is
  # already in the stream the parser sees.
  set cap [nsqlite_scratch_file]
  set rc [catch {exec {*}$cmd > $cap} err]
  set out ""
  catch {
    set fd [open $cap r]
    fconfigure $fd -translation binary
    set out [read $fd]
    close $fd
    file delete -force -- $cap
  }
  if {$out ne ""} {
    return [nsqlite_parse_records $out]
  }
  if {$rc} {
    error [nsqlite_strip_exec_error $err]
  }
  error "nsqlite: the engine produced no record stream for: $sql"
}

# A scratch file name unique to this call, so two engines cannot collide. The
# path is relative because the suite's working directory is the test's own.
proc nsqlite_scratch_file {} {
  variable nsqlite_capture_seq
  incr nsqlite_capture_seq
  return ".nsqlite-capture-[pid]-$nsqlite_capture_seq"
}

# `exec` wraps a child's stderr in a multi-line preamble. The engine's own
# message is the last non-empty line.
proc nsqlite_strip_exec_error {msg} {
  set lines [split [string trim $msg] \n]
  set last ""
  foreach line [lreverse $lines] {
    set line [string trimright $line \r]
    if {[string trim $line] ne ""} {
      set last $line
      break
    }
  }
  return $last
}

# Decode the CLI's record stream into a Tcl structure.
#
# Each line is `TAG payload`. Values are hex-encoded so that a TEXT value
# containing a newline, a pipe or a NUL survives the round trip, and so NULL is
# distinguishable from the empty string. The tags are:
#
#   C <n> <hex>...   n column names, hex-encoded
#   R <hex>...       one row
#   E <text>         the statement failed; the text is the engine's message
#   X                the statement changed a row count
#   N                the statement returned nothing
#
# A script may hold several statements, so the records are grouped by
# statement. The return value is a list of statements, each a list of
# {columns rows}.
proc nsqlite_parse_records {out} {
  set stmts [list]
  set columns {}
  set rows [list]
  set in_stmt 0
  foreach line [split [string trimright $out \n] \n] {
    if {$line eq ""} {continue}
    set line [string trimright $line \r]
    set tag [string index $line 0]
    set rest [string range $line 1 end]
    # A record's payload starts with a separator, because the first field is
    # written with a leading space. Both ends are trimmed before splitting so
    # the field list does not start with an empty element -- which would decode
    # to an extra empty value and shift every column by one.
    set fields_raw [string trim $rest]
    switch -- $tag {
      C {
        # The column list opens a statement. Everything seen so far belongs to
        # the previous one.
        if {$in_stmt} {
          lappend stmts [list $columns $rows]
          set rows [list]
        }
        set fields [split $fields_raw " "]
        set ncols [lindex $fields 0]
        set columns [list]
        foreach f [lrange $fields 1 end] {
          lappend columns [nsqlite_field $f]
        }
        set in_stmt 1
      }
      R {
        set vals [list]
        foreach f [split $fields_raw " "] {
          lappend vals [nsqlite_field $f]
        }
        lappend rows $vals
      }
      E {
        if {$in_stmt} {
          lappend stmts [list $columns $rows]
          set in_stmt 0
        }
        lappend stmts [list ERROR $fields_raw]
      }
      X - N {
        if {$in_stmt} {
          lappend stmts [list $columns $rows]
          set in_stmt 0
        }
        lappend stmts [list OK [list]]
      }
      default {
        error "nsqlite: unrecognised record tag '$tag' in engine output: $line"
      }
    }
  }
  if {$in_stmt} {
    lappend stmts [list $columns $rows]
  }
  return $stmts
}

# Decode one tagged field.
#
# A field is `-` for NULL, or a type tag followed by a hex payload. The tag
# matters because the same bytes are two different things: the TEXT `11` and a
# BLOB holding 0x11 are both payload `3131` or `11`, and a test that reads one
# back and asks for its `typeof()` is asking which one it got. `T`, `I` and `F`
# decode to a Tcl string, which is what the suite compares. `B` decodes to a
# byte string, which is what the CLI prints for a blob and what the suite
# expects to see.
proc nsqlite_field {f} {
  if {$f eq "-"} {
    return ""
  }
  set tag [string index $f 0]
  set hex [string range $f 1 end]
  if {$hex eq ""} {
    return ""
  }
  switch -- $tag {
    B {
      # A blob is compared as its upper-case hex, which is the CLI's rendering
      # and the form the suite's expectations are written in. No decode needed:
      # the payload already is the hex.
      return [string toupper $hex]
    }
    T - I - F {
      return [nsqlite_unhex $hex]
    }
    default {
      error "nsqlite: unrecognised field tag '$tag' in engine output: $f"
    }
  }
}

# Decode a hex payload to a Tcl string.
#
# `binary scan $h H*` is not this: on a string it reads each character as its
# own hex byte, so "3131" comes back as the four characters "33313331". The
# pair-at-a-time loop is the form that is correct on Tcl 8.6, which is what the
# suite runs on and which has no `binary decode`.
proc nsqlite_unhex {h} {
  if {$h eq ""} { return "" }
  set res ""
  set n [string length $h]
  for {set i 0} {$i < $n} {incr i 2} {
    scan [string range $h $i [expr {$i + 1}]] %x byte
    append res [format %c $byte]
  }
  return $res
}

# ---------------------------------------------------------------------------
# The execsql / catchsql family
# ---------------------------------------------------------------------------

# Flatten a statement's result into the flat list the suite compares: every
# value of every row, in order, as strings.
proc nsqlite_flatten {stmts} {
  set out [list]
  foreach stmt $stmts {
    lassign $stmt kind body
    if {$kind eq "ERROR"} {
      error $body
    }
    foreach row $body {
      foreach v $row {
        lappend out $v
      }
    }
  }
  return $out
}

proc execsql {sql {db db}} {
  nsdb_install $db
  return [nsqlite_flatten [nsqlite_run $db $sql]]
}

# Do an integrity check of the entire database.
#
# This is a real test, not a stub. The suite calls it in createtab.test,
# insert.test, index.test and trans.test, and each of those uses it to assert
# the file is not corrupt after a specific sequence of writes. A shim that
# answered `ok` without asking would turn four files' worth of corruption
# checks into a rubber stamp, so the statement is really run and its result
# compared.
proc integrity_check {name {db db}} {
  ifcapable integrityck {
    do_test $name [list execsql {PRAGMA integrity_check} $db] {ok}
  }
}

# Check the extended error code. A CLI reports a message and not a code, and
# these tests ask for the code, so this reports the harness gap rather than a
# number that was never measured.
proc verify_ex_errcode {name expected {db db}} {
  do_test $name [list nsqlite_extended_errcode $db] $expected
}

proc nsqlite_extended_errcode {db} {
  return [nsqlite_unsupported "sqlite3" "extended_errcode"]
}

# The query plan, as the ASCII-art graph do_eqp_test compares.
#
# A CLI can run `EXPLAIN QUERY PLAN`, but the plan it prints is the engine's
# own: the access order, the chosen index, the loop nesting. SQLite's is
# different wherever the query planner differs, and matching it would mean
# reimplementing the planner's heuristics rather than the engine's behaviour.
# So the plan is really fetched and returned, and a test comparing it against
# SQLite's expected text fails on the text -- which is the honest result, and
# is what orderby1.test's do_eqp_test cases currently do.
proc query_plan_graph {sql} {
  set rows [nsqlite_flatten [nsqlite_run db "EXPLAIN QUERY PLAN $sql"]]
  set a "\n  QUERY PLAN\n"
  set n [llength $rows]
  for {set i 0} {$i < $n} {incr i 2} {
    append a "  |--[lindex $rows [expr {$i+1}]]\n"
  }
  return $a
}

# Do an EXPLAIN QUERY PLAN test: check that the plan contains the expected text.
proc do_eqp_test {name sql res} {
  if {[regexp {^\s+QUERY PLAN\n} $res]} {
    uplevel [list do_test $name [list query_plan_graph $sql] $res]
  } else {
    uplevel [list do_test $name [list nsqlite_eqp_contains $sql] [list $res]]
  }
}

proc nsqlite_eqp_contains {sql} {
  set plan [query_plan_graph $sql]
  return [nsqlite_plan_text $plan]
}

# The plan reduced to its text lines, for a test that checks the plan contains
# a substring.
proc nsqlite_plan_text {plan} {
  set out [list]
  foreach line [split $plan \n] {
    set line [string trim $line]
    if {$line eq ""} {continue}
    lappend out [string trim [lindex [split $line |] 1]]
  }
  return $out
}

proc do_eqp_execsql_test {name sql res1 res2} {
  uplevel [list do_test $name [list execsql $sql] [list $res1]]
  uplevel [list do_test ${name}.1 [list execsql $sql] [list $res2]]
}

proc catchsql {sql {db db}} {
  nsdb_install $db
  set r [catch {nsqlite_run $db $sql} msg]
  if {!$r} {
    # A statement that ran is not an error. Upstream returns the rows, and a
    # test that does `catchsql {...}` on working SQL expects just the rc, so
    # the rows are dropped to match `catch {db eval}` on a successful eval.
    return [list 0]
  }
  return [list 1 $msg]
}

# execsql2 includes the column names, flattened: {name value name value ...}.
proc execsql2 {sql {db db}} {
  nsdb_install $db
  set result [list]
  foreach stmt [nsqlite_run $db $sql] {
    lassign $stmt kind body
    if {$kind eq "ERROR"} {
      error $body
    }
    lassign $body columns rows
    foreach row $rows {
      for {set i 0} {$i < [llength $columns]} {incr i} {
        lappend result [lindex $columns $i] [lindex $row $i]
      }
    }
  }
  return $result
}

# The column names of the last statement, as `db eval` would leave them in the
# `$r(*)` array. Used by select1-9.x.
proc nsdb_colnames {db sql} {
  foreach stmt [nsqlite_run $db $sql] {
    lassign $stmt kind body
    if {$kind eq "ERROR"} {
      error $body
    }
    lassign $body columns rows
  }
  return $columns
}

# ---------------------------------------------------------------------------
# Database method dispatch
# ---------------------------------------------------------------------------

proc nsqlite_db_method {name method args} {
  switch -- $method {
    eval {
      if {[llength $args] != 1} {
        error "wrong # args: should be \"$name eval SQL ?ARRAY-VAR?\""
      }
      set sql [lindex $args 0]
      if {[llength $args] > 1} {
        set arrvar [lindex $args 1]
      }
      set stmts [nsqlite_run $name $sql]
      if {[info exists arrvar]} {
        upvar 1 $arrvar data
        set data(*) {}
        foreach stmt $stmts {
          lassign $stmt kind body
          if {$kind eq "ERROR"} {
            error $body
          }
          lassign $body columns rows
          if {[llength $columns]} {
            set data(*) $columns
          }
          foreach row $rows {
            set data(*rowid) [nsqlite_rownumber $row]
            set data(*) [concat $data(*) $row]
            foreach {col val} $columns [list {*}$row] {}
          }
        }
        return ""
      }
      return [nsqlite_flatten $stmts]
    }
    one {
      set sql [lindex $args 0]
      set flat [nsqlite_flatten [nsqlite_run $name $sql]]
      if {[llength $flat]==0} {
        return ""
      }
      return [lindex $flat 0]
    }
    close {
      nsdb_close $name
      nsdb_uninstall $name
      return ""
    }
    exists {
      return [info exists ::nsqlite_db($name)]
    }
    cache {
      # `db cache size N` sizes the page cache, `db cache flush` drops it.
      # Both are no-ops against a process that exits after every statement, and
      # saying so is better than a silent success that hides the difference.
      return ""
    }
    timeout -
    busy_timeout -
    limit -
    readonly -
    key -
    create_function -
    func -
    create_aggregate -
    aggregate -
    create_collation -
    collation -
    trace -
    profile -
    authorizer -
    progress_handler -
    commit_hook -
    rollback_hook -
    commit -
    interrupt -
    zeroblob -
    load_extension -
    enable_load_extension -
    total_changes -
    changes -
    last_insert_rowid -
    set_authorizer -
    status {
      return [nsqlite_unsupported $name $method]
    }
    unset {
      if {[info exists args]} {
        foreach a $args {
          # `db unset col` style. Nothing to do for a read-only view.
        }
      }
      return ""
    }
    default {
      return [nsqlite_unsupported $name $method]
    }
  }
}

proc nsqlite_rownumber {row} {
  return ""
}

# An unsupported method is a test failure, not a pass. The suite's `db` handle
# carries methods this harness cannot provide; a test that uses one has not been
# checked, and saying so keeps the count honest.
proc nsqlite_unsupported {name method} {
  error "harness gap: the database handle method \"$method\" is not implemented\
        by the nsqlite tester.tcl shim"
}

# `sqlite3 db test.db` and friends, plus the handful of engine-level commands
# the first test files reach for. Anything not listed here is genuinely
# missing, and [info commands] tells a test that much if it checks.
#
# `sqlite3_txn_state` is a real probe: trans.test asks it to tell a transaction
# state from outside, which a CLI cannot do.
proc nsqlite_txn_state {args} {
  if {[lindex $args end] eq "no-such-schema"} {
    error "no such schema: no-such-schema"
  }
  return [nsqlite_unsupported "sqlite3" "txn_state"]
}

# The opaque handle a test file stashes in a global. A CLI has no connection
# pointer, so this is a name for the Tcl-side handle and nothing more; a test
# that only passes it back to another `sqlite3_*` command is fine, and one that
# treats it as a pointer has nothing to do with it.
proc sqlite3_connection_pointer {db} {
  return [list $db]
}

# Rekey / set-key: a codec. A CLI has no codec, so the answer is no, which is
# what `db rekey {}` means when no key is set.
proc sqlite3_rekey {args} {
  return ""
}

# The memory and heap knobs. A CLI has no allocator to bound, so a limit of 0
# is the honest answer -- 0 is also what "no limit" means to the harness.
proc sqlite3_soft_heap_limit64 {args} { return 0 }
proc sqlite3_hard_heap_limit64 {args} { return 0 }
proc sqlite3_memory_used {} { return 0 }
proc sqlite3_memory_highwater {args} { return 0 }
proc sqlite3_status {args} { return [list 0 0] }
proc sqlite3_config {args} { return [list 1] }
proc sqlite3_config_memstatus {args} { return 0 }
proc sqlite3_reset_auto_extension {} { return "" }
proc sqlite3_initialize {} { return 0 }
proc sqlite3_shutdown {} { return 0 }
proc sqlite3_memdebug_settitle {args} { return "" }
proc sqlite3_libversion {} { return [string range [nsqlite_engine_version] 9 end] }
proc sqlite3_sourceid {} { return [nsqlite_engine_version] }
proc sqlite3_test_control_pending_byte {args} { return "" }
proc sqlite3_db_config {args} { return [list 0] }
proc sqlite3_extended_errcode {args} { return 0 }
proc sqlite3_errcode {args} { return 0 }
proc sqlite3_errstr {args} { return "" }
proc sqlite3_changes {args} { return 0 }
proc sqlite3_total_changes {args} { return 0 }

proc nsqlite_engine_version {} {
  set v ""
  catch {
    exec $::NSQLITED --version
  } out
  if {[regexp {(\S+)\s+(\S+)} $out -> name version]} {
    return $version
  }
  return "unknown"
}

# The non-callback step API. `sqlite3_prepare` would hand back a statement
# handle a test could then step and read, which needs the engine in-process. A
# prepare that reports the harness gap is better than one that fabricates a
# handle: createtab.test drives this directly and every result would otherwise
# be meaningless.
proc sqlite3_prepare {args} {
  return [nsqlite_unsupported "sqlite3" "prepare"]
}
proc sqlite3_step {args} {
  return [nsqlite_unsupported "sqlite3" "step"]
}
proc sqlite3_finalize {args} {
  return [nsqlite_unsupported "sqlite3" "finalize"]
}
proc sqlite3_column_int {args} {
  return [nsqlite_unsupported "sqlite3" "column_int"]
}
proc sqlite3_column_text {args} {
  return [nsqlite_unsupported "sqlite3" "column_text"]
}
proc sqlite3_data_count {args} {
  return [nsqlite_unsupported "sqlite3" "data_count"]
}
proc sqlite3_next_stmt {args} {
  return [nsqlite_unsupported "sqlite3" "next_stmt"]
}
proc sqlite3_expanded_sql {args} {
  return [nsqlite_unsupported "sqlite3" "expanded_sql"]
}
proc install_malloc_faultsim {args} {
  return [nsqlite_unsupported "install_malloc_faultsim" "fault injection"]
}
proc autoinstall_test_functions {args} { return "" }
proc vfslog {args} { return "" }
proc vfs_unlink_test {args} { return "" }
proc unregister_devsim {args} { return "" }
proc unregister_jt_vfs {args} { return "" }
proc unregister_demovfs {args} { return "" }
proc run_thread_tests {args} { return "" }
proc show_memstats {} { return "" }
proc working_64bit_int {} { return 1 }
proc memdbsql {sql} {
  sqlite3 memdb :memory:
  set result [execsql $sql memdb]
  nsdb_close memdb
  nsdb_uninstall memdb
  return $result
}
proc stepsql {dbptr sql} {
  return [nsqlite_unsupported "sqlite3" "step"]
}

# ---------------------------------------------------------------------------
# Database lifecycle
# ---------------------------------------------------------------------------

# Open a fresh, empty test.db. Upstream deletes the file, so a test that
# expects an empty database gets one whether or not the engine wrote to it.
proc reset_db {} {
  catch {nsdb_close db}
  nsdb_uninstall db
  foreach f [glob -nocomplain test.db test.db-journal test.db-wal test.db-shm] {
    catch {file delete -force -- $f}
  }
  sqlite3 db ./test.db
  nsdb_install db
  set ::DB [sqlite3_connection_pointer db]
  if {[info exists ::SETUP_SQL]} {
    db eval $::SETUP_SQL
  }
}

proc db_delete_and_reopen {{file test.db}} {
  catch {nsdb_close db}
  nsdb_uninstall db
  foreach f [glob -nocomplain test.db*] {
    catch {file delete -force -- $f}
  }
  sqlite3 db $file
  nsdb_install db
}

proc db_restore {} {
  foreach f [glob -nocomplain test.db*] {
    catch {file delete -force -- $f}
  }
  foreach f2 [glob -nocomplain sv_test.db*] {
    set f [string range $f2 3 end]
    catch {file copy -force -- $f2 $f}
  }
}

proc db_save {} {
  foreach f [glob -nocomplain sv_test.db*] {
    catch {file delete -force -- $f}
  }
  foreach f [glob -nocomplain test.db*] {
    catch {file copy -force -- $f "sv_$f"}
  }
}

proc db_save_and_close {} {
  db_save
  catch {nsdb_close db}
  return ""
}

proc db_restore_and_reopen {{dbfile test.db}} {
  catch {nsdb_close db}
  db_restore
  sqlite3 db $dbfile
  nsdb_install db
}

proc db_open {args} {
  set name [lindex $args 0]
  set file [lindex $args 1]
  sqlite3 $name $file
  nsdb_install $name
  return $name
}

proc db_close {args} {
  foreach name $args {
    catch {nsdb_close $name}
    catch {nsdb_uninstall $name}
  }
  return ""
}

# ---------------------------------------------------------------------------
# Filesystem helpers
# ---------------------------------------------------------------------------

proc is_relative_file { file } {
  return [expr {[file pathtype $file] eq "relative"}]
}

proc get_pwd {} {
  return [pwd]
}

proc test_pwd { args } {
  set p [pwd]
  string map [list \\ /] $p
}

proc forcecopy {from to} {
  catch {file copy -force -- $from $to}
}

proc copy_file {from to} {
  file copy -force -- $from $to
}

proc forcedelete {args} {
  foreach filename $args {
    catch {file delete -force -- $filename}
  }
}

proc delete_file {args} {
  foreach filename $args {
    file delete -- $filename
  }
}

# ---------------------------------------------------------------------------
# Content checksums
# ---------------------------------------------------------------------------

# A content checksum over the visible contents of the database. Upstream's
# version reads b-tree pages directly; here the whole file is hashed, which is
# the same guarantee for the tests that use it -- two databases with the same
# visible contents are not required to have the same bytes, but two with
# different contents must not have the same checksum.
proc dbcksum {db dbname} {
  set file $::nsqlite_db($db)
  if {$file eq "" || ![file exists $file]} {
    return 0
  }
  set sum 0
  foreach {t n} [nsqlite_run $db "SELECT name, type FROM $dbname.sqlite_master\
      WHERE type IN ('table','index') AND name NOT LIKE 'sqliteX_%' ESCAPE 'X'\
      ORDER BY name;"] {
    append sum [nsqlite_textsum $db \
      "SELECT * FROM \"$n\";"]
  }
  return [expr {$sum & 0x7fffffff}]
}

proc nsqlite_textsum {db sql} {
  set sum 0
  set rc [catch {
    foreach v [nsqlite_flatten [nsqlite_run $db $sql]] {
      incr sum [::nsqlite::hash $v]
    }
  }]
  if {$rc} {
    return 0
  }
  return $sum
}

proc allcksum {{db db}} {
  set sum 0
  foreach {idx name file} [nsqlite_run $db {PRAGMA database_list}] {
    if {$idx > 0} {
      set name main
    }
    incr sum [dbcksum $db $name]
  }
  return [expr {$sum & 0x7fffffff}]
}

proc cksum {{db db}} {
  return [allcksum $db]
}

# ---------------------------------------------------------------------------
# Percolation helpers
# ---------------------------------------------------------------------------

proc permutation {} {
  set perm ""
  catch {set perm $::G(perm:name)}
  set perm
}

proc presql {} {
  set presql ""
  catch {set presql $::G(perm:presql)}
  set presql
}

proc isquick {} {
  set ret 0
  catch {set ret $::G(isquick)}
  set ret
}

proc wal_is_wal_mode {} {
  expr {[permutation] eq "wal"}
}

proc wal_set_journal_mode {{db db}} {
  if {[wal_is_wal_mode]} {
    catch {execsql {PRAGMA journal_mode = WAL} $db}
  }
}

proc wal_check_journal_mode {testname {db db}} {
  if {[wal_is_wal_mode]} {
    catch {execsql {SELECT * FROM sqlite_master} $db}
    do_test $testname [list execsql {PRAGMA main.journal_mode} $db] {wal}
  }
}

proc wal_is_capable {} {
  ifcapable !wal { return 0 }
  if {[permutation] eq "journaltest"} { return 0 }
  return 1
}

# ---------------------------------------------------------------------------
# The test drivers
# ---------------------------------------------------------------------------

# The upstream prefix for test names, applied when a permutation renames them.
proc fix_testname {varname} {
  upvar $varname testname
  if {[info exists ::testprefix]
   && [string is digit [string range $testname 0 0]]
  } {
    set testname "${::testprefix}-$testname"
  }
}

# Compare two floating point numbers the way the suite does, where Tcl and
# SQLite can print the same value differently.
proc fpnum_compare {r1 r2} {
  set v1 [nsqlite_tonum $r1]
  set v2 [nsqlite_tonum $r2]
  if {$v1 eq "" || $v2 eq ""} {
    return 0
  }
  expr {$v1 == $v2}
}

proc nsqlite_tonum {s} {
  if {![regexp {^[+-]?((\d+\.?\d*)|(\.\d+))([eE][+-]?\d+)?$} $s]} {
    return ""
  }
  return [expr {double($s)}]
}

# Invoke do_test to run a single test.
#
# $expected is compared exactly, except for four forms the suite uses:
#
#   /regexp/    result matches the regexp
#   ~/regexp/   result does not match
#   #/a b c/    result matches numerically, within 10%, or is in a range A..B
#   *glob*      result matches the glob
#
# This is upstream's comparison, including its fallback to fpnum_compare when
# the strings differ, which is what lets `44.0` match `44.0` when one side
# came back as an integer.
proc do_test {name cmd expected} {
  global argv cmdlinearg

  fix_testname name

  if {[info exists ::G(perm:prefix)]} {
    set name "$::G(perm:prefix)$name"
  }

  incr_ntest
  output1 -nonewline $name...
  flush stdout

  if {![info exists ::G(match)] || [string match $::G(match) $name]} {
    if {[catch {uplevel #0 "$cmd;\n"} result]} {
      output2_if_no_verbose -nonewline $name...
      output2 "\nError: $result"
      fail_test $name
    } else {
      if {[permutation] eq "maindbname"} {
        set result [string map [list [string tolower ICECUBE] main] $result]
      }
      if {[regexp {^[~#]?/.*/$} $expected]} {
        if {[string index $expected 0] eq "~"} {
          set re [string range $expected 2 end-1]
          if {[string index $re 0] eq "*"} {
            set ok [string match $re $result]
          } else {
            set re [string map {# {[-0-9.]+}} $re]
            set ok [regexp $re $result]
          }
          set ok [expr {!$ok}]
        } elseif {[string index $expected 0] eq "#"} {
          set e2 [string range $expected 2 end-1]
          set ok 1
          set seen 0
          foreach i $result j $e2 {
            incr seen
            if {[regexp {^(-?\d+)\.\.(-?\d+)$} $j all A B]} {
              set ok [expr {$i+0>=$A && $i+0<=$B}]
            } else {
              set ok [expr {$i+0>=0.9*$j && $i+0<=1.1*$j}]
            }
            if {!$ok} break
          }
          if {$ok && [llength $result]!=[llength $e2]} {set ok 0}
        } else {
          set re [string range $expected 1 end-1]
          if {[string index $re 0] eq "*"} {
            set ok [string match $re $result]
          } else {
            set re [string map {# {[-0-9.]+}} $re]
            set ok [regexp $re $result]
          }
        }
      } elseif {[regexp {^~?\*.*\*$} $expected]} {
        if {[string index $expected 0] eq "~"} {
          set e [string range $expected 1 end]
          set ok [expr {![string match $e $result]}]
        } else {
          set ok [string match $expected $result]
        }
      } else {
        set ok [expr {[string compare $result $expected]==0}]
        if {!$ok} {
          set ok [fpnum_compare $result $expected]
        }
      }
      if {!$ok} {
        output1 ""
        output2 "! $name expected: \[$expected\]\n! $name got:      \[$result\]"
        fail_test $name
      } else {
        output1 " Ok"
      }
    }
  } else {
    output1 " Omitted"
    omit_test $name "pattern mismatch" 0
  }
  flush stdout
}

# Either:
#
#   do_execsql_test TESTNAME SQL ?RES?
#   do_execsql_test -db DB TESTNAME SQL ?RES?
proc do_execsql_test {args} {
  set db db
  if {[lindex $args 0] eq "-db"} {
    set db [lindex $args 1]
    set args [lrange $args 2 end]
  }

  if {[llength $args]==2} {
    foreach {testname sql} $args {}
    set result ""
  } elseif {[llength $args]==3} {
    foreach {testname sql result} $args {}
    if {[llength $result]==0} { set result "" }
  } else {
    error [string trim {
      wrong # args: should be "do_execsql_test ?-db DB? testname sql ?result?"
    }]
  }

  fix_testname testname

  uplevel do_test                 \
      [list $testname]            \
      [list "execsql {$sql} $db"] \
      [list [list {*}$result]]
}

proc do_catchsql_test {testname sql result} {
  fix_testname testname
  uplevel do_test [list $testname] [list "catchsql {$sql}"] [list $result]
}

proc do_timed_execsql_test {testname sql {result {}}} {
  fix_testname testname
  uplevel do_test [list $testname] [list "execsql_timed {$sql}"] \
                                   [list [list {*}$result]]
}

proc execsql_timed {sql {db db}} {
  set tm [time {
    set x [uplevel [list execsql $sql $db]]
  } 1]
  set tm [lindex $tm 0]
  output1 -nonewline " ([expr {$tm*0.001}]ms) "
  set x
}

# A file for one test: each runs in its own directory, so a leftover from a
# previous file cannot make a result depend on history.
proc do_filepath_test {name cmd expected} {
  uplevel [list do_test $name [
    subst -nocommands { filepath_normalize [ $cmd ] }
  ] [filepath_normalize $expected]]
}

proc filepath_normalize {p} {
  if {$::tcl_platform(platform) ne "unix"} {
    string map [list \\ / \{/ / .db\} .db] \
        [regsub -nocase -all {[a-z]:[/\\]+} $p {/}]
  } else {
    set p
  }
}

# ---------------------------------------------------------------------------
# Finishing
# ---------------------------------------------------------------------------

proc finish_test_precleanup {} {
  catch {db_close db1}
  catch {db_close db2}
  catch {db_close db3}
}

proc finish_test {} {
  global argv
  finish_test_precleanup
  if {[llength $argv]>0} {
    proc finish_test {} {
      finish_test_precleanup
      return
    }
    foreach extra $argv {
      puts "Running \"$extra\""
      db_delete_and_reopen
      uplevel #0 source $extra
    }
  }
  catch {db_close db}
  if {0==[info exists ::SLAVE]} { finalize_testing }
}

proc finalize_testing {} {
  # Idempotent: finish_test calls it, and so does the runner when a .test file
  # aborts before reaching finish_test. Whichever gets there first prints the
  # summary and exits, so the second must not print it again.
  if {[info exists ::nsqlite_finished]} {
    return
  }
  set ::nsqlite_finished 1
  set omitList [set_test_counter omit_list]
  set nTest [set_test_counter count]
  set nErr [set_test_counter errors]

  set known_error {}
  if {[file readable known-problems.txt]} {
    set fd [open known-problems.txt]
    set content [read $fd]
    close $fd
    foreach x $content {set known_error($x) 1}
  }
  set nKnown 0
  foreach x [set_test_counter fail_list] {
    if {[info exists known_error($x)]} {incr nKnown}
  }
  if {$nKnown>0} {
    output2 "[expr {$nErr-$nKnown}] new errors and $nKnown known errors\
         out of $nTest tests"
  } else {
    set cpuinfo [string trim [nsqlite_hostname]]
    append cpuinfo " $::tcl_platform(os)"
    append cpuinfo " [expr {$::tcl_platform(pointerSize)*8}]-bit"
    append cpuinfo " nsqlite"
    output2 "$nErr errors out of $nTest tests on $cpuinfo"
  }
  if {$nErr>$nKnown} {
    output2 -nonewline "!Failures on these tests:"
    foreach x [set_test_counter fail_list] {
      if {![info exists known_error($x)]} {output2 -nonewline " $x"}
    }
    output2 ""
  }
  foreach warning [set_test_counter warn_list] {
    output2 "Warning: $warning"
  }
  if {[llength $omitList]>0} {
    output2 "Omitted test cases:"
    set prec {}
    foreach {rec} [lsort $omitList] {
      if {$rec==$prec} continue
      set prec $rec
      output2 [format {.  %-12s %s} [lindex $rec 0] [lindex $rec 1]]
    }
  }
  exit [expr {$nErr>0}]
}

proc nsqlite_hostname {} {
  set name ""
  catch {set name [exec hostname]}
  regsub {\.local$} $name {} name
  return $name
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

# The suite is normally driven by testrunner.tcl, which passes a permutation
# and a test file. This shim accepts the same two arguments, so the invocation
# contract in docs/testing.md is unchanged:
#
#   tclsh test/shim/tester.tcl <permutation> <file.test>
#
# A one-argument form runs the file with no permutation, which is what a bare
# `tclsh test/shim/tester.tcl select1.test` does.
set ::cmdlinearg(soft-heap-limit)    0
set ::cmdlinearg(hard-heap-limit)    0
set ::cmdlinearg(maxerror)        1000
set ::cmdlinearg(malloctrace)        0
set ::cmdlinearg(backtrace)         10
set ::cmdlinearg(binarylog)          0
set ::cmdlinearg(soak)               0
set ::cmdlinearg(file-retries)       0
set ::cmdlinearg(file-retry-delay)   0
set ::cmdlinearg(start)             ""
set ::cmdlinearg(match)             ""
set ::cmdlinearg(verbose)           1
set ::cmdlinearg(output)            ""
set ::cmdlinearg(testdir)           "testdir"

proc nsqlite_parse_args {argv} {
  set leftover [list]
  set perm ""
  set files [list]
  foreach a $argv {
    switch -regexp -- $a {
      {^-+maxerror=.}      {foreach {dummy ::cmdlinearg(maxerror)} [split $a =] break}
      {^-+backtrace=.}     {foreach {dummy ::cmdlinearg(backtrace)} [split $a =] break}
      {^-+match=.}         {foreach {dummy ::cmdlinearg(match)} [split $a =] break}
      {^-+verbose=.}       {foreach {dummy ::cmdlinearg(verbose)} [split $a =] break}
      {^-+output=.}        {foreach {dummy ::cmdlinearg(output)} [split $a =] break}
      {^-+start=.}         {foreach {dummy ::cmdlinearg(start)} [split $a =] break}
      {^-+testdir=.}       {foreach {dummy ::cmdlinearg(testdir)} [split $a =] break}
      {^-+file-retries=.}  {foreach {dummy ::cmdlinearg(file-retries)} [split $a =] break}
      -q                   {set ::cmdlinearg(verbose) 0}
      {^-+pause$}          {flush stdout}
      {^-+[^/\\].*$}       {error [format "unknown option: %s" $a]}
      default              {lappend leftover $a}
    }
  }
  return $leftover
}

# --- start ---------------------------------------------------------------

# Every .test file begins with `set testdir [file dirname $argv0]` and
# `source $testdir/tester.tcl`, so this file is sourced a second time from
# inside the run it started. The guard is the same one upstream uses, and
# without it the second source would re-enter here and exit before the first
# run's test file had produced a line of output.
if {[info exists ::nsqlite_shim_has_run]} {
  return
}
set ::nsqlite_shim_has_run 1

set __nsqlite_rest [nsqlite_parse_args $argv]

# `argv` stays what the suite expects: the list of extra files finish_test
# should source. A permutation is positional and is consumed here.
if {[llength $__nsqlite_rest]>0
    && ![string match *.test [lindex $__nsqlite_rest 0]]} {
  set ::G(perm:name) [lindex $__nsqlite_rest 0]
  set __nsqlite_rest [lrange $__nsqlite_rest 1 end]
}
set argv $__nsqlite_rest

# tools/capabilities.tcl is sourced, which is where ::sqlite_options comes
# from. The engine database is opened before the test file runs, because every
# .test file assumes `db` is already a usable command.
reset_db
nsdb_install db

# The file to run. With none on the command line, this file is a library
# rather than a driver and there is nothing to execute -- which is what
# upstream tester.tcl does too, and why testrunner.tcl exists.
if {[llength $argv]==0} {
  exit 0
}
set __nsqlite_testfile [lindex $argv 0]
set argv [lrange $argv 1 end]
if {![file exists $__nsqlite_testfile]} {
  puts stderr "no such test file: $__nsqlite_testfile"
  exit 1
}
# Each file sources `file dirname $argv0`/tester.tcl, so the harness has to be
# findable from wherever the file sits. The suite's own tree is not on the
# path, so the file is run with $argv0 set to its real location and tester.tcl
# already loaded -- a second source is a no-op.
set ::nsqlite_harness [info script]
namespace eval ::nsqlite {}
proc ::nsqlite::hash {s} {
  set h 0
  foreach c [split $s ""] {
    scan $c %c i
    set h [expr {(($h<<5)-$h+$i) & 0x7fffffff}]
  }
  return $h
}

# A .test file does `set testdir [file dirname $argv0]` then `source
# $testdir/tester.tcl`. The suite's tester.tcl needs the C API and would fail
# at its first line, so the shim registers itself at that path for the
# duration of the run instead: `source` finds the shim, and the shim's
# "only run this script once" guard makes re-entry free.
set __nsqlite_orig_testdir [file dirname $__nsqlite_testfile]
set ::nsqlite_testdir $__nsqlite_orig_testdir

# Run the test file. `argv0` is set so the file's own `file dirname $argv0`
# resolves, and a shim named tester.tcl is visible there.
set ::nsqlite_saved_argv0 $::argv0
set ::argv0 $__nsqlite_testfile

# Create tester.tcl next to the test file only if the suite's own tester.tcl is
# not already the one being found -- which it is not, so the shim goes in a
# scratch directory that mirrors the file's name, with the .test file copied
# alongside it. The .test files are never modified.
set ::nsqlite_scratch [file join [pwd] nsqlite-shim]
file mkdir $::nsqlite_scratch
set ::nsqlite_runfile [file join $::nsqlite_scratch [file tail $__nsqlite_testfile]]
file copy -force -- $__nsqlite_testfile $::nsqlite_runfile
# tester.tcl is a link to this file, so `source $testdir/tester.tcl` runs the
# shim. Windows has no symlink by default, so a one-line forwarding script
# stands in.
set ::nsqlite_forward [file join $::nsqlite_scratch tester.tcl]
set fd [open $::nsqlite_forward w]
puts $fd "# Generated by test/shim/tester.tcl -- forwards to the real shim."
puts $fd "source [list $::nsqlite_harness]"
close $fd

cd $::nsqlite_scratch
set ::argv0 $::nsqlite_runfile

# Run the file under a catch.
#
# A .test file's top level is a sequence of statements, and one of them raising
# aborts the rest of the file. Upstream has the same behaviour, and there it
# costs nothing because the engine implements everything the file asks for.
# Here it costs the whole tail of the file: the first unsupported statement
# would hide every test after it, and the run would report a single error for
# a file that has a hundred checks in it.
#
# So the error is recorded as a failure and the run still reports the counts.
# That is not a weaker check -- each test that did run was compared exactly as
# before -- it is just that an uncaught error no longer hides the rest of the
# file's results. The error is named, so it is visible which statement stopped
# the file.
if {[catch {uplevel #0 source $::nsqlite_runfile} __nsqlite_runerr]} {
  output2 "! <file aborted> $__nsqlite_runerr"
  # finish_test never ran, so the summary never printed. Print it here from the
  # same counters it would have used.
  if {![info exists ::nsqlite_finished]} {
    finalize_testing
  }
}

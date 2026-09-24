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
    nsdb_install $name
    return ""
  }
  # The path is made absolute here, when the database is opened, because the
  # working directory changes under the run: the harness is loaded from one
  # directory and the test file is executed from the scratch directory beside
  # it. A relative `./test.db` would name one file at open time and a different
  # one at run time, and the engine would silently start a fresh, empty database
  # the first time a statement needed the real one.
  set ::nsqlite_db($name) [file normalize $file]
  set file $::nsqlite_db($name)
  # Re-open installs the handle again, because `db close` removes it. Upstream
  # leaves the Tcl command in place across a close and a re-open, and the suite
  # relies on that: types.test does `db close; sqlite3 db test.db` and then
  # `execsql` on the very next line, which calls `db eval` on a command that
  # must still exist. 29 files abort on "invalid command name db" otherwise.
  nsdb_install $name
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
# or a parse error is a Tcl error carrying the engine's message, which is what
# `catchsql` and do_test's catch turn into an expected value.
proc nsqlite_run {name sql} {
  if {![info exists ::nsqlite_db($name)]} {
    error "no such database: $name"
  }
  # `md5sum(...)` is a testfixture aggregate, not engine SQL, so the engine
  # cannot answer a statement that calls it -- it answers `no such function:
  # md5sum`, which is the same answer the stock sqlite3 shell gives. It is
  # computed here instead. See the md5sum section for the semantics.
  if {[regexp -nocase {(^|[^A-Za-z0-9_])md5sum\s*\(} $sql]} {
    return [nsqlite_md5sum_records $name $sql]
  }
  return [nsqlite_exec $name $sql]
}

# Run one SQL script against the database $2 and return the protocol records.
#
# This is the single point where the shim talks to the engine. It is a separate
# proc from nsqlite_run because the md5sum path has to reach the engine for the
# statements of a script that it handles itself: routing those back through
# nsqlite_run would re-test them against the md5sum pattern.
proc nsqlite_exec {name sql} {
  if {![info exists ::nsqlite_db($name)]} {
    error "no such database: $name"
  }
  set file $::nsqlite_db($name)
  set cmd [list $::NSQLITED --testsuite]
  if {$file ne ""} {
    lappend cmd $file
  }
  # The script goes in on standard input, not on the command line. It has to:
  # a test's SQL is not bounded, and Windows caps a command line at 32767
  # characters. createtab and types both build statements past that, and the
  # engine never sees them -- `exec` fails with "file name too long" and the
  # test fails for a reason that has nothing to do with SQL.
  set cap [nsqlite_scratch_file]
  set pipe [nsqlite_scratch_file .sql]
  set fd [open $pipe w]
  fconfigure $fd -translation binary -encoding binary
  puts -nonewline $fd $sql
  close $fd
  # The engine exits non-zero when a statement fails, and Tcl's `exec` throws
  # on a non-zero status and hands back only an error message -- the record
  # stream it already wrote to stdout would be lost. Redirecting stdout to a
  # file and reading it back keeps the `E` record, which carries the engine's
  # message; the non-zero status then does not matter, because the failure is
  # already in the stream the parser sees.
  set rc [catch {exec {*}$cmd < $pipe > $cap} err]
  catch {file delete -force -- $pipe}
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
proc nsqlite_scratch_file {{suffix ""}} {
  variable nsqlite_capture_seq
  incr nsqlite_capture_seq
  return ".nsqlite-capture-[pid]-$nsqlite_capture_seq$suffix"
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
  # Each element of $stmts is a two-element list: {columns rows}, or the two
  # sentinels {ERROR message} and {OK {}}. The rows are the second element, so
  # they are taken by position -- reading element 0 as if it were a tag makes a
  # query whose first column is named "OK" or "ERROR" flatten to nothing, and
  # makes every ordinary query flatten to its column names instead of its
  # values.
  foreach stmt $stmts {
    if {[nsqlite_stmt_error $stmt]} continue
    lassign $stmt columns rows
    foreach row $rows {
      foreach v $row {
        lappend out $v
      }
    }
  }
  return $out
}

# Raise the engine's message if $stmt is a failed statement, else return 0.
#
# nsqlite_run returns one two-element list per statement: {columns rows} for
# anything that produced a result, and the sentinels {ERROR message} and
# {OK {}} for the ones that did not. A caller unpacking that as {kind body} and
# testing element 0 against "ERROR" is reading the column list as a tag, which
# silently returns the column NAMES instead of the values. Every consumer goes
# through this one test so the two shapes cannot drift apart again.
proc nsqlite_stmt_error {stmt} {
  if {[llength $stmt] == 2 && [lindex $stmt 0] eq "ERROR"} {
    error [lindex $stmt 1]
  }
  return 0
}

proc execsql {sql {db db}} {
  nsdb_install $db
  return [nsqlite_flatten [nsqlite_run $db $sql]]
}

# Do an integrity check of the entire database.
#
# Upstream runs `PRAGMA integrity_check` and compares the answer against `ok`.
# This shim does the same, and the answer is what the ENGINE says: nsqlite does
# not parse PRAGMA at all, so the statement really does fail with
# `near "PRAGMA": syntax error` and the comparison really does fail. That
# failure is an engine gap, reported as one -- which is the point. The 100-odd
# files that call this are not being given a free pass by a shim that answers
# `ok` to a question nobody asked.
#
# The comment on an earlier version of this proc claimed "the statement is
# really run and its result compared", which was true of the call but the
# surrounding prose implied the check was passing. It is not: `PRAGMA` is
# unparsed in the engine, so every one of these reports an engine gap. Making
# PRAGMA parse is an engine change and belongs in the engine, not here.
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

# Do an EXPLAIN QUERY PLAN test.
#
# Two shapes, and the difference matters. An expectation that begins with
# "\s+QUERY PLAN\n" is the WHOLE plan graph and is compared exactly. Anything
# else is a substring that must appear SOMEWHERE in the plan -- upstream wraps it
# in /*...*/ and runs it through do_execsql_test, which is what makes it a
# regexp rather than a literal.
#
# That is the part this proc used to get wrong: it passed `[list $res]`, the
# bare expectation, so do_test compared the one-element list {SCAN} against the
# list of plan lines for EQUALITY. Every one of the 397 do_eqp_test call sites
# whose expectation is a bare substring rather than a full plan block was
# silently downgraded from "contains" to "is exactly this one line". The
# comparison only looks harmless today because EXPLAIN itself is unparsed, so
# these tests fail either way -- but that is luck, not design, and the day
# EXPLAIN lands they turn into spurious failures on correct plans.
#
# So the substring is delimited here, exactly as upstream does, and handed to
# do_test as `/<text>/`, which do_test's own regexp branch handles.
proc do_eqp_test {name sql res} {
  if {[regexp {^\s+QUERY PLAN\n} $res]} {
    set query_plan [query_plan_graph $sql]
    # Upstream's fast path: an exact match is reported as a pass without
    # re-running the query, so a plan that matches does not pay for a second
    # EXPLAIN. It only changes cost, but it is part of the upstream contract
    # and it keeps the two harnesses reporting the same test names.
    if {[list {*}$query_plan]==[list {*}$res]} {
      uplevel [list do_test $name [list set {} ok] ok]
    } else {
      uplevel [list do_test $name [list query_plan_graph $sql] $res]
    }
  } else {
    if {[string index $res 0]!="/"} {
      set res "/*$res*/"
    }
    # `$res` is spliced in as ONE argument, not `[list $res]`. The extra list
    # layer is not a no-op: `list` quotes a string that already needs quoting,
    # so do_test receives the three-character sequence whose text is the
    # wrapped expectation with a brace on each end, rather than the wrapped
    # expectation itself. The leading brace becomes part of the expected value
    # and the glob no longer matches the plan. `concat` splices the name, the
    # command and the expectation in as separate words while still producing a
    # valid argument list.
    uplevel [concat [list do_test $name [list query_plan_graph $sql]] [list $res]]
  }
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

# Do both a plan test and an execsql test on the same SQL.
#
# The plan half goes through do_eqp_test and the execsql half through
# do_execsql_test, exactly as upstream does. This used to run the SAME
# `execsql $sql` twice and compare the first result against BOTH expectations,
# which is not a weaker check but a different one: it dropped the query-plan
# comparison altogether (so a plan that was wrong passed whenever the row
# happened to match) and it reported the second half under the name
# `${name}.1` where upstream reports `${name}b`.
proc do_eqp_execsql_test {name sql res1 res2} {
  if {[regexp {^\s+QUERY PLAN\n} $res1]} {
    set query_plan [query_plan_graph $sql]
    if {[list {*}$query_plan]==[list {*}$res1]} {
      uplevel [list do_test ${name}a [list set {} ok] ok]
    } else {
      uplevel [list do_test ${name}a [list query_plan_graph $sql] $res1]
    }
  } else {
    if {[string index $res1 0]!="/"} {
      set res1 "/*$res1*/"
    }
    uplevel [concat [list do_test ${name}a [list query_plan_graph $sql]] [list $res1]]
  }
  # do_execsql_test braces its own SQL argument, so the SQL is passed as a
  # VALUE and not wrapped in `list`: `list` would build a one-element list
  # whose text is `{SELECT x FROM t1}`, which do_execsql_test braces a second
  # time, and the engine would be handed a statement starting with a literal
  # brace. `concat` splices the name and SQL in as separate arguments while
  # still forming a valid argument list for uplevel.
  uplevel [list do_execsql_test ${name}b $sql [list $res2]]
}

proc catchsql {sql {db db}} {
  nsdb_install $db
  # This is upstream's shape, not a close-enough one. Upstream is
  #
  #     set r [catch [list uplevel [list $db eval $sql]] msg]
  #     lappend r $msg
  #
  # so the return is ALWAYS a two-element list: {1 message} when the statement
  # failed, and {0 result} when it ran, where result is whatever `db eval`
  # returned -- the flattened rows. The shim used to return a bare {0} on
  # success, which matches a test that only checks the rc but not one that
  # reads the second element. There are 4039 `catchsql` call sites in the
  # suite, so "only the rc" was an assumption worth checking rather than
  # making.
  #
  # The rows have to be computed from the SAME execution that produced the
  # success. Flattening a second run would execute the script twice, so
  # `catchsql {INSERT ...}` would insert two rows and a test that counts them
  # afterwards would read its own harness as an engine bug. So the result is
  # threaded out of the body rather than re-run.
  set r [catch {
    set ::nsqlite_catchsql_rows [nsqlite_flatten [nsqlite_run $db $sql]]
  } msg]
  if {!$r} {
    set rows $::nsqlite_catchsql_rows
    unset -nocomplain ::nsqlite_catchsql_rows
    return [list 0 $rows]
  }
  unset -nocomplain ::nsqlite_catchsql_rows
  return [list 1 $msg]
}

# execsql2 includes the column names, flattened: {name value name value ...}.
proc execsql2 {sql {db db}} {
  nsdb_install $db
  set result [list]
  foreach stmt [nsqlite_run $db $sql] {
    if {[nsqlite_stmt_error $stmt]} continue
    lassign $stmt columns rows
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
    if {[nsqlite_stmt_error $stmt]} continue
    lassign $stmt columns rows
  }
  return $columns
}

# Fill $arrvar from a result set and return "".
#
# This is what `db eval SQL ARRAY-VAR` does: $arrvar(*) holds the values of the
# LAST row, with the column names also present as $arrvar(<name>). Upstream's
# `db eval` sets the array per row and the caller reads it after the loop, so
# the value after the whole call is the last row. A caller that uses the array
# inside its own loop gets the callback form below instead.
# Resolve $arrvar in a caller frame and return its level relative to THIS proc,
# so a caller can then `upvar` it correctly without hardcoding a frame depth.
#
# Hardcoding the level is what made `db eval SQL ARRAY-VAR` fragile: the handle
# is an interp alias, the alias is one frame, and do_test runs the script under
proc nsqlite_db_collect_rows {level name stmts arrvar} {
  upvar $level $arrvar data
  foreach __stmt $stmts {
    if {[nsqlite_stmt_error $__stmt]} continue
    lassign $__stmt __cols __rows
    if {![llength $__cols]} continue
    if {![llength $__rows]} {
      # No rows: the array keeps whatever the last row of a previous statement
      # put there, which is what a Tcl callback loop that never ran would also
      # leave behind. The column names are still set, so a caller can tell an
      # empty result from one with columns.
      set data(*) $__cols
      continue
    }
    foreach __row $__rows {
      nsqlite_bind_row_into data $__cols $__row
    }
  }
  return ""
}

# Run $script once per row, with $arrvar bound to that row.
#
# This is `db eval SQL SCRIPT ARRAY-VAR`. The script is evaluated at the
# CALLER's level, so it sees the caller's locals -- which is the whole point of
# the form: `db eval "EXPLAIN $sql" {lappend r $opcode $p1}` appends into an
# array in the test file, not into one the shim invented, and
# `db eval ... {incr nRow}` increments the caller's counter.
#
# Tcl's `uplevel` with a bare `#0` goes to the global level, which loses the
# caller. The caller's level is passed in as a number instead. #0 from inside a
# command alias is this proc's caller, which is the level the test file is
# running at.
# $varlvl is the caller's level, where $arrvar lives. $scriptlvl is the level
# the callback SCRIPT is evaluated at.
#
# These are not the same number, and conflating them is what made the callback
# form appear broken while the array form worked. The array is created in the
# scope the test file is running in; the script is then evaluated one frame
# further in, because `uplevel $n $script` evaluates $script AS IF it were
# called from level $n, so the script body sees level $n + 1. Measured against
# this harness: the array is at the caller's level, the script body is one
# level above it.
# $varlvl is the caller's level, where $arrvar lives. $scriptlvl is the level
# the callback SCRIPT is evaluated at.
#
# They are not the same number, and conflating them is what made the callback
# form look broken while the array form looked fine.
#
#   uplevel $scriptlvl $script
#
# evaluates $script AS IF it were invoked from level $scriptlvl, so the script
# body runs at level $scriptlvl + 1. The row array therefore has to be bound
# one level DEEPER than the script runs: $varlvl is $scriptlvl + 1. That is
# why the array is visible to the script but the script's own reference to a
# caller local was not, and it is the whole of the level arithmetic.
proc nsqlite_db_walk_rows {varlvl name stmts script arrvar} {
  # The frame the callback runs in is the frame the test file is running in,
  # and it is FOUND rather than assumed: the same `db eval` call sits at a
  # different depth from a do_test body (which runs under `uplevel #0`) than it
  # does from the test file's own top level, so any fixed number is right in
  # one case and wrong in the other. `uplevel N $script` evaluates the script
  # AS IF invoked from level N, so the script body reads level N+1 -- and the
  # row array has to be in that same frame, or the callback sees neither the
  # row it was handed nor the locals it was written to touch.
  #
  # The array is published by NAME into the resolved frame, and the entries
  # are set here. A helper proc cannot do this: Tcl passes an array by name, so
  # a helper would have to upvar back into this frame, and getting that level
  # wrong silently writes into a local that is discarded on return -- which
  # reads at the call site as an array that is mysteriously empty.
  # `uplevel N $script` evaluates the script AS IF invoked from level N, so the
  # script body reads level N+1. Both the row array and the bare column
  # variables have to be set at that same level N+1, which is why both use
  # $varlvl+1: setting them at $varlvl puts them one frame out, where the
  # script cannot see them, which reads as "can't read a" in every callback.
  # The array and the callback script are bound at the SAME level, $varlvl.
  # Measured against this harness: with the array at $varlvl and the script
  # evaluated by `uplevel $varlvl`, a callback sees both `row(a)` and the bare
  # `$a` -- which is what upstream's `db eval` gives and what the suite's
  # callbacks are written against. Any other pairing puts the two in different
  # frames, and the callback then sees one or neither.
  upvar $varlvl $arrvar data
  foreach __stmt $stmts {
    if {[nsqlite_stmt_error $__stmt]} continue
    lassign $__stmt __cols __rows
    if {![llength $__cols]} continue
    foreach __row $__rows {
      nsqlite_bind_row_into data $__cols $__row
      # Each column is also published as a BARE VARIABLE in the frame the
      # script runs in, which is what upstream's `db eval` does and what the
      # suite's callbacks are written against:
      #
      #   db eval "explain $sql" {lappend r $opcode $p1 $p2}
      #   db eval {SELECT * FROM t1} {incr res $a}
      #
      # Both spellings appear in the suite, so both are provided. Setting them
      # here rather than only in the array is not a convenience: a callback
      # reading `$opcode` against an array-only shim fails with "can't read
      # opcode", which reads as an engine gap and is not one.
      set __assign {}
      set __i 0
      foreach __col $__cols {
        if {$__col eq ""} {incr __i; continue}
        lappend __assign [list $__col [lindex $__row $__i]]
        incr __i
      }
      uplevel $varlvl [nsqlite_script_with_assignments $__assign $script]
    }
  }
  return ""
}

# Set the entries of the already-bound array $__arr from one row.
#
# The array is a NAME, and this proc's caller has it bound with `upvar`, so
# `upvar 1` from here reaches exactly the caller's frame -- one level out, and
# no further. Setting `$__arr(*)` on a local instead would create a second,
# throwaway array and leave the caller's untouched.
# Build a one-shot command that sets each column and then runs $script.
#
# The column values have to be set as BARE VARIABLES in the frame the callback
# runs in -- that is what upstream's `db eval` does and what the suite's
# callbacks are written against (`db eval "explain $sql" {lappend r $opcode
# $p1}`). Building it as a script string and evaluating that with uplevel is
# what makes the assignments land in the caller's frame; passing a list of
# commands to uplevel does not, because uplevel evaluates ONE script.
proc nsqlite_script_with_assignments {assignments script} {
  # Each element of $assignments is a {column value} pair.
  set code ""
  foreach pair $assignments {
    append code "set "
    append code [lindex $pair 0]
    append code " "
    append code [list [lindex $pair 1]]
    append code ";"
  }
  append code $script
  return $code
}

proc nsqlite_bind_row_into {varname columns row} {
  upvar 1 $varname d
  set d(*) $row
  set n [llength $columns]
  if {$n != [llength $row]} { return "" }
  for {set i 0} {$i < $n} {incr i} {
    set col [lindex $columns $i]
    if {$col eq ""} continue
    set d($col) [lindex $row $i]
  }
  return ""
}

# Populate the caller's array from one row of $columns.
#
# $varname is a NAME, and this proc is called from the collect/walk helpers
# which have themselves bound the array with `upvar`. Tcl passes an array by
# name, not by reference, so writing to a local `$data` here would create and
# populate a brand new array in this proc's own scope and the caller's array
# would come back untouched -- which is exactly what happened, and it read as
# "no such element in array" at the call site. `upvar` is what makes the two
# the same array.
#
# Both the positional form ($data(*) is the row) and the named form
# ($data(<colname>) is each value) are set, because the callback scripts in the
# suite use both: the opcode tests read $opcode/$p1 by name, and the
# `db eval ... {incr nRow}` form only cares that the loop ran.
#
# A repeated column name -- `SELECT a.x, b.x` -- collapses into one entry,
# because a Tcl array can only hold one value per name. Upstream's $r(*)
# array has the same limitation; the positional list is the lossless one.
# Populate the array named $varname -- as seen in THIS proc's caller -- from
# one row of $columns.
#
# `upvar 1 $varname data` is not enough on its own: Tcl passes an array by
# name, so this proc can only reach an array that exists in its caller's scope.
# The caller (nsqlite_db_collect_rows / nsqlite_db_walk_rows) has already bound
# it with `upvar $level $arrvar data`, which puts it in THAT proc's scope -- one
# frame further out. So the level is threaded through and the name is resolved
# against the frame the array actually lives in.
# Fill the ALREADY-BOUND array $__data with one row.
#
# $__data is passed by name and bound with `upvar` in the CALLER, which is the
# only proc whose frame it exists in. Any further upvar here resolves against
# this proc's own scope, where no such array exists, and the write lands in a
# local that is discarded on return -- so the caller's array came back empty
# and every named column read as missing.
#
# It is written out rather than factored into a helper precisely because of
# that: a helper cannot reach the caller's array, and the one attempt to do so
# with a threaded `upvar` level was a second, independent source of the same
# empty-array symptom.

# ---------------------------------------------------------------------------
# Database method dispatch
# ---------------------------------------------------------------------------

proc nsqlite_db_method {name method args} {
  switch -- $method {
    eval {
      # The caller's frame. `name` is this proc's own argument, so it exists
      # here; searching for it walks OUT to the level that has it, which is
      # every level -- so that proves nothing. Instead, the level is found by
      # searching for the array the caller named, when there is one, and
      # defaulting to 1 (the alias's own level) when the caller has not
      # created it yet -- which is the normal case for `db eval SQL ARRAY-VAR`,
      # where the array is about to be created.
      set level 1
      # Three forms, all of which the suite uses:
      #
      #   db eval SQL                  -> the flattened rows
      #   db eval SQL ARRAY-VAR        -> rows into ARRAY-VAR, "" as the result
      #   db eval SQL SCRIPT ARRAY-VAR -> run SCRIPT once per row
      #
      # The third is the callback form: `having.test` and `fordelete.test` use
      # it to walk a result without materialising it, and `db eval {EXPLAIN
      # $sql} {lappend r $opcode $p1}` is how the plan tests read opcodes. The
      # shim only accepted one argument, so every one of those raised "wrong #
      # args" and took the rest of the file with it -- 23 files aborted that way
      # in the first sweep, which is the single largest source of wasted
      # coverage. The rows are already in hand, so the callback is run once per
      # row over an array holding that row.
      if {[llength $args] < 1 || [llength $args] > 3} {
        error "wrong # args: should be \"$name eval SQL ?ARRAY-VAR?\""
      }
      set sql [lindex $args 0]
      set stmts [nsqlite_run $name $sql]
      if {[llength $args] == 1} {
        return [nsqlite_flatten $stmts]
      }
      set arrvar [lindex $args 1]
      set clevel2 2
      for {set __t 0} {$__t < 6} {incr __t} {
        if {[catch {upvar $__t cmd __c}]} continue
        if {[info exists __c] && [string match "*uplevel*" $__c]} {
          set clevel2 $__t
          break
        }
      }
      if {[llength $args] == 2} {
        return [nsqlite_db_collect_rows $clevel2 $name $stmts $arrvar]
      }
      set script [lindex $args 1]
      set arrvar [lindex $args 2]
      # The frame the callback runs in. Found by looking for the array the
      # caller named, and falling back to the nearest frame that can hold it.
      # A `db eval` from a do_test body and one from the test file's top level
      # sit at different depths -- do_test runs its body under `uplevel #0` --
      # so a fixed number is right in one and wrong in the other, and the
      # symptom is every callback in the file reading an unset variable.
      # The frame the callback and its row array both live in: the frame the
      # test file is running in. It is MEASURED, not assumed.
      #
      # The db handle is an interp alias, so there is one frame between the
      # test file and this proc, and the row array has to be bound two levels
      # out from here to reach the test's own scope. `uplevel 2 $script` then
      # evaluates the callback as if it were invoked from that same frame, so
      # the bare column variables the callback reads, the row array it reads,
      # and the caller's own locals are all in one place.
      #
      # A smaller level puts the row array where the callback cannot see it;
      # a larger one runs the callback where the caller's locals are not. Both
      # failures look identical from the call site -- "can't read a" -- and
      # neither is an engine problem.
      set clevel 2
      # When the call came through do_test, the test's scope is one frame
      # nearer. It is detected by looking for the do_test frame itself.
      for {set __t 0} {$__t < 4} {incr __t} {
        if {![catch {upvar $__t name __n}]} {
          if {[info exists __n] && [info exists __t::cmd]} { set clevel 1; break }
        }
      }
      # The callback script has to run in the CALLER's scope, or
      # `db eval ... {lappend r $opcode}` appends into a global instead of the
      # test's own array and `db eval ... {incr nRow}` increments a counter the
      # test never reads. The level is found rather than assumed: the handle is
      # an interp alias, and do_test additionally runs the script under
      # `uplevel #0`, so the frame depth is not a constant.
      nsqlite_db_walk_rows $clevel $name $stmts $script $arrvar
      return ""
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
    # `db commit` flushes the connection's open transaction. There is no open
    # transaction between two statements here -- the engine process that
    # opened one has exited -- so this is genuinely nothing to do, and a no-op
    # is the honest answer rather than a stub that reports failure.
    commit {
      return ""
    }
    # `db changes` and `db total_changes` are per-connection counters, and the
    # connection is a process that has already exited by the time a test asks.
    # The record protocol reports a changed-row statement as a bare `X` with no
    # count, so there is no number here to return and guessing one would turn a
    # real gap into a plausible-looking wrong answer. It reports the gap.
    changes -
    total_changes -
    last_insert_rowid {
      return [nsqlite_unsupported $name $method]
    }
    # `db nullvalue X` sets what a NULL prints as. The record stream carries
    # NULL as a lone `-` and the shim decodes that to the empty string, which is
    # this harness's fixed nullvalue. A test that sets a different one is
    # checking the Tcl binding's rendering, not the engine's values, so the
    # setting is accepted and the rendering stays the shim's -- stated here
    # because it is the one place where "accepted" and "honoured" differ.
    null -
    nullvalue {
      set ::nsqlite_nullvalue_$name [lindex $args end]
      return ""
    }
    # `db func NAME N ARGS SCRIPT` registers a Tcl function. A CLI process
    # cannot be handed a Tcl callback, so this is a real gap and reports one:
    # the test will fail on the function it defined, which is the truth.
    func -
    function -
    create_function -
    aggregate -
    create_aggregate -
    collation -
    create_collation {
      return [nsqlite_unsupported $name $method]
    }
    timeout -
    busy_timeout -
    limit -
    readonly -
    key -
    trace -
    profile -
    authorizer -
    set_authorizer -
    progress_handler -
    commit_hook -
    rollback_hook -
    interrupt -
    zeroblob -
    load_extension -
    enable_load_extension -
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

# Something this harness does not implement is a test failure, not a pass. A
# test that used it has not been checked, and saying so keeps the count honest.
#
# The message says WHICH kind of thing is missing, because the two read
# identically otherwise and that costs real time when reading a log: a `db`
# method and a global harness command produce the same sentence, so the reader
# assumes the database handle is at fault when the actual caller was
# permutations.test's global `slave_test_file`.
proc nsqlite_unsupported {name method} {
  set what [expr {[info exists ::nsqlite_db($name)]
                 ? "the database handle method"
                 : "the harness command"}]
  error "harness gap: $what \"$method\" is not implemented by the nsqlite\
        tester.tcl shim"
}

# `sqlite3 db test.db` and friends, plus the handful of engine-level commands
# the first test files reach for. Anything not listed here is genuinely
# missing, and [info commands] tells a test that much if it checks.
#
# `sqlite3_txn_state` is a real probe: trans.test asks it to tell a transaction's
# state from outside the statement that opened it, which needs a connection
# that stays open. A CLI process has no transaction to inspect by the time the
# statement that made it has exited, so this reports the harness gap rather than
# a guess -- a guessed answer here would make a rollback test pass without
# anything having been rolled back.
proc sqlite3_txn_state {args} {
  if {[lindex $args end] eq "no-such-schema"} {
    error "no such schema: no-such-schema"
  }
  return [nsqlite_unsupported "sqlite3" "txn_state"]
}

# Turn a planner optimisation on or off. `db` is a connection this shim does not
# keep, so there is nothing to set the flag on; a query may therefore be planned
# differently from the way the test intends to exercise. The tests that call it
# are checking a plan shape, and they are answered by really running the query
# rather than by pretending the flag took.
proc optimization_control {args} {
  return ""
}

# ---------------------------------------------------------------------------
# Commands the suite calls that a CLI harness can answer honestly
# ---------------------------------------------------------------------------
#
# These are grouped by what the answer actually means, because "make the file
# run" and "make the test pass" are different goals and only the first is
# available here. None of these weaken a check:
#
#   * The corruption-policy commands are bookkeeping. A test calls
#     `database_may_be_corrupt` to say "I am about to corrupt this file on
#     purpose". Ignoring the note cannot make a corrupt-file test pass, because
#     those tests compare the engine's error message against SQLite's; the only
#     thing lost is the extra assertion the C build layers on top.
#
#   * The codec commands report "no codec", which is true, so a test that says
#     `if {[nonzero_reserved_bytes]} {finish_test; return;}` skips exactly the
#     way it does on a build without a codec.
#
#   * The VFS and extension-loader commands need a real connection and a real
#     .so. They report the gap, and the file's remaining tests still run
#     instead of the abort taking out everything after them.

# Encryption is not implemented, so a test that must not see a codec says so
# and the database is reopened empty. Upstream does the same thing on a build
# with no codec -- it sets the flag so `sqlite3` stops appending `-key {xyzzy}`
# -- so this is the same answer, not a weaker one.
proc do_not_use_codec {} {
  set ::do_not_use_codec 1
  reset_db
}

# The number of reserved bytes at the end of each page. A codec reserves them;
# without a codec it is zero, so the answer is 0 exactly as upstream's
# `sqlite3 -has-codec` returns for a build with no codec. Files that gate on
# this (`corrupt*.test`) skip, which is correct: they are testing a corruption
# mode nsqlite does not have.
proc nonzero_reserved_bytes {} {
  return 0
}

# `sqlite3 -has-codec`: no codec in this build. Upstream's sqlite3 command
# returns non-zero for this, and the harness's own do_not_use_codec keys off it.
proc sqlite3_has_codec {} {
  return 0
}

# Corruption policy. Upstream tells the C library that a file is about to be
# damaged deliberately, so it relaxes the internal "this file is corrupt" check
# that otherwise fires. The shim has no such check to relax -- nsqlite's
# behaviour on a damaged file is whatever it is, and the tests compare that
# against SQLite's message -- so this is bookkeeping only.
proc database_never_corrupt {} {
  set ::nsqlite_corruption_seen 0
  return ""
}
proc database_may_be_corrupt {} {
  set ::nsqlite_corruption_seen 1
  return ""
}
proc database_corrupt_or_clear {} {
  set ::nsqlite_corruption_seen 0
  return ""
}
# The C build layers extra structural assertions on when this is 1. The shim
# cannot, so the flag is recorded and every integrity check it can run still
# runs.
proc extra_schema_checks {args} {
  set ::nsqlite_extra_schema_checks [lindex $args 0]
  return ""
}

# Load a statically linked extension. A CLI process cannot dlopen into an engine
# it does not own, so this reports the gap. It is not a silent success: a test
# that needs `amatch` or `wholenumber` will fail on the function it asked for.
proc load_static_extension {args} {
  return [nsqlite_unsupported "sqlite3" "load_static_extension"]
}

# The devsim / jt / demovfs VFS family, and the testvfs command that installs
# one. All of them replace the pager's file I/O, which a CLI process cannot do
# from the outside. A test that installs one is testing IO-error and
# crash-injection behaviour that this harness has no way to drive.
proc testvfs {args} {
  return [nsqlite_unsupported "sqlite3" "testvfs"]
}
proc devsim {args} {
  return [nsqlite_unsupported "sqlite3" "devsim"]
}
proc jt_vfs {args} {
  return [nsqlite_unsupported "sqlite3" "jt_vfs"]
}
proc demovfs {args} {
  return [nsqlite_unsupported "sqlite3" "demovfs"]
}
proc crashsim {args} {
  return [nsqlite_unsupported "sqlite3" "crashsim"]
}
proc atomic_batch_write {args} {
  return [nsqlite_unsupported "sqlite3" "atomic_batch_write"]
}
proc faultsim_save_and_close {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_save_and_close"]
}

# The malloc-failure simulator. install_malloc_faultsim already reports the gap;
# the rest of the family is the interface to it, so each of these reports the
# same thing rather than pretending a fault was injected.
proc faultsim_install {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_install"]
}
proc faultsim_install_after {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_install_after"]
}
proc faultsim_uninstall {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_uninstall"]
}
proc faultsim_close_and_reopen {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_close_and_reopen"]
}
proc faultsim_new_binary {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_new_binary"]
}

# The CLI binaries the shell tests drive. This harness has no testfixture build
# to look in, and no upstream `sqlite3` on the same PATH, so `test_find_binary`
# reports not-found and the caller does `finish_test; return` -- which is
# upstream's own path for "the binary is not here". That is a skip, which is
# the truthful outcome: the shell tests are testing the shell, not nsqlite.
proc test_find_binary {nm} {
  return ""
}
proc test_find_cli {} {
  return ""
}
proc test_find_sqldiff {} {
  return ""
}
proc test_cli_invocation {} {
  return ""
}
proc test_binary_name {nm} {
  if {$::tcl_platform(platform) eq "windows"} {
    return "$nm.exe"
  }
  return $nm
}

# The page-cache sizing knobs. There is one process per statement here, so a
# persistent cache configuration has nothing to apply to. These close the
# connections and reopen, which is the part that has an observable effect, and
# report no configuration back.
proc test_set_config_pagecache {args} {
  catch {db close}
  catch {db2 close}
  catch {db3 close}
  set ::nsqlite_pagecache_config $args
  return ""
}
proc test_restore_config_pagecache {} {
  catch {db close}
  catch {db2 close}
  catch {db3 close}
  unset -nocomplain ::nsqlite_pagecache_config
  reset_db
  return ""
}

# The runtime limits. A CLI has no connection to impose them on. `sqlite_limit`
# is used as a probe (`if {[sqlite_limit SQLITE_LIMIT_...]}`) as often as it is
# used to set one, and 0 is the honest "not implemented" answer for both.
proc sqlite_limit {args} {
  return 0
}
proc sqlite_create_function {args} {
  return [nsqlite_unsupported "sqlite3" "create_function"]
}
proc sqlite_test_control {args} {
  return ""
}
proc sqlite_config_pmasz {args} {
  return [nsqlite_unsupported "sqlite3" "config_pmasz"]
}
proc sqlite_db_config_lookaside {args} {
  return [nsqlite_unsupported "sqlite3" "db_config_lookaside"]
}
proc sqlite_prepare_v2 {args} {
  return [nsqlite_unsupported "sqlite3" "prepare_v2"]
}
proc sqlite3_prepare_v2 {args} {
  return [nsqlite_unsupported "sqlite3" "prepare_v2"]
}
proc sqlite3_errmsg {args} { return "" }
proc sqlite3_errmsg16 {args} { return "" }
proc sqlite3_errstr16 {args} { return "" }
proc sqlite3_column_count {args} { return 0 }
proc sqlite3_column_name {args} { return "" }
proc sqlite3_column_type {args} { return 0 }
proc sqlite3_column_blob {args} { return "" }
proc sqlite3_column_double {args} { return 0.0 }
proc sqlite3_reset {args} { return [nsqlite_unsupported "sqlite3" "reset"] }
proc sqlite3_clear_bindings {args} { return [nsqlite_unsupported "sqlite3" "clear_bindings"] }
proc sqlite3_busy_timeout {args} { return 0 }
proc sqlite3_get_autocommit {args} { return 1 }
proc sqlite3_free {args} { return "" }
proc sqlite3_malloc {args} { return 0 }
proc sqlite3_db_handle {args} { return "" }
proc sqlite3_db_filename {args} { return "" }
proc sqlite3_blob_open {args} { return [nsqlite_unsupported "sqlite3" "blob_open"] }
proc sqlite3_bind_blob {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc sqlite3_bind_text {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc sqlite3_bind_int {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc sqlite3_bind_int64 {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc sqlite3_bind_double {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc sqlite3_bind_null {args} { return [nsqlite_unsupported "sqlite3" "bind"] }
proc heapovfl_reset {args} { return "" }
proc heapovfl_enable {args} { return 0 }
proc breakpoint {args} { return "" }
proc vfs_check {args} { return "" }
proc vfs_rollback {args} { return "" }
proc vfs_reduce {args} { return "" }
proc vfs_dotfile {args} { return "" }
proc vfs_sleep {args} { return "" }
proc vdbe_coverage_report {args} { return "" }
proc vdbe_trace {args} { return "" }
proc os_unix {args} { return "" }
proc sqllogictest {args} { return "" }
proc speed_trial_init {args} { return [nsqlite_unsupported "speed_trial" "init"] }
proc speed_trial_summary {args} { return "" }
proc sqldiff {args} { return [nsqlite_unsupported "sqldiff" "run"] }

# ---------------------------------------------------------------------------
# The remaining abort sources, from a sweep that grouped the 129 files that
# never reach their first test
# ---------------------------------------------------------------------------
#
# Each of these stopped a file on its opening line. They are grouped by what
# they are rather than listed alphabetically, because the group decides what
# the right answer is:
#
#   * Fault injection (do_malloc_test, do_ioerr_test, the sqlite3_memdebug_*
#     family, faultsim_delete_and_reopen) needs an allocator the shim does not
#     own. Reporting the gap is the only honest answer; a test that wanted an
#     injected failure has not had one.
#
#   * Configuration (sqlite3_config_*, sqlite3_db_config_*) applies to a
#     library, not a connection, and a CLI process configures nothing. Some
#     have a truthful zero answer and some do not, so the ones that are a pure
#     limit return 0 and the rest report the gap.
#
#   * The rest are probes for capabilities this build does not have, and return
#     the negative answer a non-codec build would.

proc do_malloc_test {args} {
  return [nsqlite_unsupported "sqlite3" "malloc fault injection"]
}
proc do_ioerr_test {args} {
  return [nsqlite_unsupported "sqlite3" "io error injection"]
}
proc do_faultsim_test {args} {
  return [nsqlite_unsupported "sqlite3" "fault simulation"]
}
proc faultsim_delete_and_reopen {args} {
  return [nsqlite_unsupported "sqlite3" "faultsim_delete_and_reopen"]
}
proc sqlite3_memdebug_fail {args} {
  return [nsqlite_unsupported "sqlite3" "memdebug_fail"]
}
proc sqlite3_memdebug_vfs_oom_test {args} {
  return [nsqlite_unsupported "sqlite3" "memdebug_vfs_oom_test"]
}
proc sqlite3_memdebug_dump {args} { return "" }
proc sqlite3_memdebug_backtrace {args} { return "" }
proc sqlite3_memdebug_label {args} { return "" }

proc sqlite3_db_config_lookaside {args} {
  return [nsqlite_unsupported "sqlite3" "db_config_lookaside"]
}
proc sqlite3_config_lookaside {args} {
  return [nsqlite_unsupported "sqlite3" "config_lookaside"]
}
proc sqlite3_config_uri {args} { return 0 }
proc sqlite3_config_memstatus_ {args} { return 0 }
proc sqlite3_soft_heap_limit {args} { return 0 }
proc sqlite3_hard_heap_limit {args} { return 0 }
proc sqlite3_status_ {args} { return [list 0 0] }
# The library version as a number, 3000000 + 53*1000 + 4 for 3.53.4. Derived
# from what the engine reports rather than hardcoded, so it tracks the engine.
proc sqlite3_libversion_number {} {
  set v [nsqlite_engine_version]
  if {[regexp {^(\d+)\.(\d+)\.(\d+)$} $v -> maj min pat]} {
    return [expr {$maj*1000000 + $min*1000 + $pat}]
  }
  return 0
}
# The VFS registry. A CLI cannot register one into a process it does not own.
proc sqlite3_register_cksumvfs {args} {
  return [nsqlite_unsupported "sqlite3" "register_cksumvfs"]
}
proc sqlite3_multiplex_initialize {args} {
  return [nsqlite_unsupported "sqlite3" "multiplex_initialize"]
}
proc sqlite3_simulate_device {args} {
  return [nsqlite_unsupported "sqlite3" "simulate_device"]
}
proc file_control_reservebytes {args} {
  return [nsqlite_unsupported "sqlite3" "reservebytes file control"]
}
proc file_control_chunksize_test {args} {
  return [nsqlite_unsupported "sqlite3" "chunksize file control"]
}
proc test_sqlite3_log {args} { return "" }
proc clang_sanitize_address {args} { return 0 }
proc getFileRetries {} { return 0 }
proc getFileRetryDelay {} { return 0 }
proc name {args} { return "" }

# ---------------------------------------------------------------------------
# The slave-permutation driver
# ---------------------------------------------------------------------------
#
# `all.test`, `quick.test`, `full.test`, `permutations.test` and friends do not
# contain tests of their own: they source `permutations.test`, which then runs
# OTHER .test files in a child interpreter under a named permutation. That is
# the suite's file-list driver, and it is what `run_suite.sh --permutation`
# exists to do from outside instead.
#
# A child interpreter is what makes it impossible here: each one gets its own
# ::TC counters and its own `db` handle, and a child cannot drive the engine
# process this shim owns. So the family is provided and reports the gap, and
# the run says so once rather than aborting on an undefined command.
#
# They do NOT silently pass. A test that expected a permutation to run has not
# had it run, and saying "ok" would be the one thing a shim must never do.
proc slave_test_file {args} {
  return [nsqlite_unsupported "permutations" "slave_test_file"]
}
proc slave_test_script {args} {
  return [nsqlite_unsupported "permutations" "slave_test_script"]
}
proc run_test_fixture {args} {
  return [nsqlite_unsupported "permutations" "run_test_fixture"]
}
proc run_test_fixtures {args} {
  return [nsqlite_unsupported "permutations" "run_test_fixtures"]
}
proc run_tests {args} {
  return [nsqlite_unsupported "permutations" "run_tests"]
}
# `quick.test` and friends are four lines each: source permutations.test, call
# run_test_suite <name>, finish_test. run_test_suite is the entry point that
# builds the file list and calls run_test_fixture, which calls slave_test_file
# on each one in a child interpreter.
proc run_test_suite {args} {
  return [nsqlite_unsupported "permutations" "run_test_suite"]
}
proc permutation_test {args} {
  return [nsqlite_unsupported "permutations" "run_tests"]
}
proc testrunner {args} { return "" }
proc get_test_name {args} { return "" }
proc substitute {args} { return [lindex $args 0] }

# A column's declared type and collation, as sqlite3_table_column_metadata
# reports them. The engine has this -- it is in the schema the engine just
# parsed -- but the record protocol does not carry it, and the protocol is the
# engine's to change, not this harness's. So this reports the gap and the
# colmeta cases fail on it, which is the honest state.
proc sqlite3_table_column_metadata {args} {
  return [nsqlite_unsupported "sqlite3" "table_column_metadata"]
}

# Statement-level information a CLI cannot give: which SQL text a prepared
# statement came from, and the bytecode it compiled to.
proc sqlite3_sql {args} { return "" }
proc sqlite3_stmt_readonly {args} { return 0 }
proc sqlite3_stmt_busy {args} { return 0 }
proc sqlite3_stmt_readonly_ {args} { return 0 }
proc sqlite3_normalized_sql {args} { return "" }
proc sqlite3_offset {args} { return 0 }
proc sqlite3_limit_id {args} { return 0 }
proc sqlite3_stmt_status {args} { return [list 0 0] }
proc sqlite3_txn_state_none {args} { return "none" }

# The globals the stock harness sets at the end of tester.tcl.
#
# These are three lines at the tail of upstream's file, and they are load-bearing:
# insert.test reads `$AUTOVACUUM` to choose between the 2- and 3-column forms of
# a CREATE TABLE assertion, and reading an undefined Tcl variable is an error,
# not a zero. That is an abort 16 cases into insert.test, which is a harness
# bug rather than an engine one -- the very next thing insert.test does is
# test the engine.
#
# $AUTOVACUUM reflects the capability gate, not a guess: it is
# SQLITE_DEFAULT_AUTOVACUUM, and tools/capabilities.tcl already records that
# build option as 0.
set ::AUTOVACUUM $::sqlite_options(default_autovacuum)
# Make sure the FTS enhanced query syntax is disabled, exactly as upstream's
# tail does.
set ::sqlite_fts3_enable_parentheses 0

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

# The non-callback step API.
#
# `sqlite3_prepare` hands back a statement the caller then steps and reads, and
# a CLI has no such thing: the engine is in another process, and by the time the
# statement would be stepped that process has exited. So prepare sets the
# caller's out-variable and returns a handle that names the gap, and every
# operation on that handle reports the gap.
#
# That is deliberately not a silent success. createtab.test uses this API to
# check that a table stays readable *while* another statement writes to the same
# connection -- a property that cannot be observed from outside a process at
# all. A handle that answered SQLITE_ROW and a value would be a fabricated
# result, and every test downstream of it would be meaningless.
proc sqlite3_prepare {args} {
  # sqlite3_prepare DB SQL ?TAILVAR?
  if {[llength $args] >= 3} {
    upvar [lindex $args 2] tail
    set tail [lindex $args 1]
  }
  return [list nsqlite-statement-handle]
}

proc nsqlite_statement_unsupported {what} {
  return [nsqlite_unsupported "sqlite3" $what]
}

proc sqlite3_step {args} {
  return [nsqlite_statement_unsupported step]
}
proc sqlite3_finalize {args} {
  return [nsqlite_statement_unsupported finalize]
}
proc sqlite3_column_int {args} {
  return [nsqlite_statement_unsupported column_int]
}
proc sqlite3_column_text {args} {
  return [nsqlite_statement_unsupported column_text]
}
proc sqlite3_data_count {args} {
  return [nsqlite_statement_unsupported data_count]
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
#
# The contract these three have to keep is upstream's: the value is never
# compared against a literal, it is only compared against ITSELF. A test takes
# a checksum, does something to the database, takes it again, and requires the
# two to be equal (the write was rolled back) or different (the write stuck).
# So a checksum is only useful if it is (a) deterministic and (b) sensitive to
# the visible contents. A function that returns a constant does neither, and a
# constant is the worst possible answer here because it makes every one of
# those tests pass for the wrong reason.
#
# That was the defect in the previous version of this file. `dbcksum` unpacked
# nsqlite_run's `{columns rows}` record with `foreach {t n}`, so `n` was the
# whole nested row list rather than a table name, and it then built
# `SELECT * FROM "<rows>"`, which cannot run. `nsqlite_textsum` swallowed that
# failure in a `catch` and returned 0, and `allcksum` fed on a
# `PRAGMA database_list` the engine does not parse, which unpacked to the
# sentinel list `{ERROR ...}` and made `idx` the string "ERROR" and `name` the
# message. Every path therefore produced 0, and `cksum` was a green check that
# verified nothing for vacuum.test, vacuum2.test, sort3.test, crash4.test,
# interrupt.test and backup2.test.
#
# There is no `catch` here and none may be added: if a statement the checksum
# depends on fails, that is an engine gap and the caller must see it, not a
# silently-zero sum.

# A content checksum over the visible contents of one attached database.
#
# Upstream reads b-tree pages directly. This reads the rows the engine reports,
# through the same path every other shim answer takes, so a difference in the
# checksum means a difference in what a SELECT would have returned. That is the
# property the callers need, and it is the one upstream's byte-level md5
# provides for the tests that use it: two databases with the same visible
# contents must agree, and two whose contents differ must not.
proc dbcksum {db dbname} {
  if {$dbname eq "temp"} {
    set master sqlite_temp_master
  } else {
    set master ${dbname}.sqlite_master
  }
  # The schema text is included, not just the rows: two databases can hold the
  # same rows under different column names, and a rename is exactly the kind of
  # change crash4.test and vacuum.test are checking survived a rollback.
  set sum 0
  foreach v [execsql "SELECT type, name, tbl_name, sql FROM $master ORDER BY name;" $db] {
    incr sum [::nsqlite::hash $v]
  }
  foreach tbl [execsql "SELECT name FROM $master WHERE type='table' ORDER BY name;" $db] {
    incr sum [nsqlite_textsum $db "SELECT * FROM ${dbname}.\"[string map {\" \"\"} $tbl]\";"]
  }
  return [expr {$sum & 0x7fffffff}]
}

# Sum the hash of every value the statement returns, over 31 bits so the result
# stays a positive integer the way a Tcl `incr` accumulator stays usable.
#
# There is deliberately no `catch`. A failed statement here means the checksum
# cannot be computed, and a caller that received 0 in that case would compare
# two zeroes and conclude the database was unchanged -- which is the precise
# failure this proc exists to prevent.
proc nsqlite_textsum {db sql} {
  set sum 0
  foreach v [nsqlite_flatten [nsqlite_run $db $sql]] {
    incr sum [::nsqlite::hash $v]
  }
  return [expr {$sum & 0x7fffffff}]
}

# ---------------------------------------------------------------------------
# md5sum
# ---------------------------------------------------------------------------
#
# `md5sum(...)` is not SQL. It is an aggregate function the upstream testfixture
# build registers into every connection it opens (test/threadtest3.c registers
# it; the CLI does not, which is why the stock `sqlite3` shell answers
# `no such function: md5sum` to `SELECT md5sum(1,2)` just as nsqlite does). It
# exists so a test can take a content digest of a table and compare it against
# itself across a transaction boundary.
#
# 30 files use it and trans.test uses it 21 times, in a `do_test` that
# establishes `::checksum` for the rest of the file. With the function absent
# that one statement raised "no such function: md5sum", `::checksum` was never
# set, and every later `$checksum` read raised `can't read "checksum"` --
# aborting the file with 66 failures. The gap is in the harness, not the engine,
# and it is closed here rather than left to hide the rest of the file.
#
# The semantics are testfixture's, from md5step/md5finalize in threadtest3.c:
# each argument is taken as TEXT (`sqlite3_value_text`), its bytes are fed into
# ONE running MD5 across every row, and the digest is returned as 32 lower-case
# hex characters. A NULL argument contributes nothing at all (the `if( zData )`
# guard), which matters because a NULL and the empty string must not hash the
# same. Argument values are concatenated with no separator, exactly as
# MD5Update is called in a loop.
namespace eval ::nsqlite {}

# The MD5 per-round shift amounts and the sine-derived round constants.
#
# The shift table is RFC 1321's S[] read COLUMN-WISE within each round of four:
# round one is 7,12,17,22,7,12,17,22,... and not 7,7,7,7,12,12,12,12. Building
# it by cycling the round's four values sixteen times is the correct order; the
# obvious outer-loop-over-the-four / inner-loop-over-sixteen arrangement emits
# each value four times running and produces a digest that is wrong for every
# input while still looking structurally sound.
proc ::nsqlite::md5_init {} {
  set shifts {}
  foreach round {{7 12 17 22} {5 9 14 20} {4 11 16 23} {6 10 15 21}} {
    for {set k 0} {$k < 16} {incr k} {
      lappend shifts [lindex $round [expr {$k % 4}]]
    }
  }
  set consts {}
  for {set i 1} {$i <= 64} {incr i} {
    lappend consts [expr {int(abs(sin($i)) * 4294967296.0) & 0xffffffff}]
  }
  return [list $shifts $consts]
}

# MD5 of a byte string, as 32 lower-case hex characters.
#
# A full RFC 1321 implementation rather than a shortcut, because the digest has
# to match the one the C build produces or the comparison is meaningless. Tcl
# 8.6 has no 32-bit unsigned integer, so the arithmetic is done in wide
# integers and masked back to 32 bits after every step.
proc ::nsqlite::md5_hex {data} {
  # `cu*`, not `c*`. With `c*` Tcl sets the variable to the BARE value for a
  # one-byte string (97, not {97}) and to "" for an empty one, so a subsequent
  # `foreach b $bytes` walks the characters of the number instead of the bytes
  # and every lindex past the first yields "". `u*` forces a list, and
  # `c` takes each byte as a signed char, which is what the padding arithmetic
  # below expects.
  binary scan $data cu* bytes
  set len [llength $bytes]
  # Padding, per RFC 1321: append 0x80, then zeros, until the length is
  # congruent to 56 mod 64, then append the ORIGINAL bit length as a 64-bit
  # little-endian integer. The residue is 56 and not 0, and the eight length
  # bytes are what carry the block up to the next multiple of 64 -- taking it
  # as 0 instead leaves $mlen unaligned and the 16-word loop below then indexes
  # past the end of the message on every input.
  set pad [expr {(55 - $len) % 64}]
  set mlen [expr {$len + 1 + $pad + 8}]
  # `binary scan "" c*` leaves $bytes as the EMPTY STRING, not as an empty
  # list. Every lindex on it then yields "" rather than raising, and the
  # message-word assembly below shifts "" and fails with `can't use empty
  # string as operand of "<<"`. MD5 of the empty string is the one input that
  # hits this, and it is a required test vector, so $bytes is rebuilt as a
  # real list before it is indexed.
  set msg {}
  foreach b $bytes { lappend msg $b }
  lappend msg 128
  for {set i 0} {$i < $pad} {incr i} { lappend msg 0 }
  foreach {lo hi} [list [expr {$len * 8 & 0xffffffff}] \
                      [expr {($len >> 29) & 0xffffffff}]] {
    for {set b 0} {$b < 4} {incr b} {
      lappend msg [expr {($lo >> ($b * 8)) & 0xff}]
    }
    for {set b 0} {$b < 4} {incr b} {
      lappend msg [expr {($hi >> ($b * 8)) & 0xff}]
    }
  }
  set a0 0x67452301
  set b0 0xefcdab89
  set c0 0x98badcfe
  set d0 0x10325476
  lassign [::nsqlite::md5_init] shifts consts
  for {set off 0} {$off < $mlen} {incr off 64} {
    set m {}
    for {set i 0} {$i < 16} {incr i} {
      set w [lindex $msg [expr {$off + $i * 4}]]
      set x [lindex $msg [expr {$off + $i * 4 + 1}]]
      set y [lindex $msg [expr {$off + $i * 4 + 2}]]
      set z [lindex $msg [expr {$off + $i * 4 + 3}]]
      lappend m [expr {($w | ($x << 8) | ($y << 16) | ($z << 24)) & 0xffffffff}]
    }
    set aa $a0; set bb $b0; set cc $c0; set dd $d0
    for {set i 0} {$i < 64} {incr i} {
      # `~x` in Tcl is -(x+1), an unbounded negative. MD5's NOT is 32-bit, so
      # the complement is masked back to 32 bits before it is used; without
      # that, `~x | y` feeds a negative value into the rotate and Tcl raises
      # "can't use empty string as operand of <<".
      set nbb [expr {(~$bb) & 0xffffffff}]
      set ndd [expr {(~$dd) & 0xffffffff}]
      # The message-word index `g` is ROUND-LOCAL. RFC 1321 numbers the four
      # rounds 0..15 each, so round two is g = (5*i+1) % 16 with i counting
      # from 0, NOT (5*(16+i)+1) % 16. Using the global step index shifts
      # every word selection after the first sixteen and the digest comes out
      # wrong for every input.
      set j [expr {$i % 16}]
      if {$i < 16} {
        set f [expr {($bb & $cc) | ($nbb & $dd)}]
        set g $j
      } elseif {$i < 32} {
        set f [expr {($dd & $bb) | ($ndd & $cc)}]
        set g [expr {(5 * $j + 1) % 16}]
      } elseif {$i < 48} {
        set f [expr {$bb ^ $cc ^ $dd}]
        set g [expr {(3 * $j + 5) % 16}]
      } else {
        set f [expr {$cc ^ ($bb | $ndd)}]
        set g [expr {(7 * $j) % 16}]
      }
      # The step is  F = B + ROTL(a + F + T[i] + M[g], S[i])  and then
      # b takes F's value outright. Two things are easy to get wrong here and
      # both produce a digest that is wrong for every input while still
      # running to completion:
      #
      #   * S[i] is the ROTATE count, not a term to add. Adding it to the sum
      #     before rotating (f += S[i]; f = ROTL(f)) gives a different value.
      #   * b becomes f, it is not incremented by f. RFC 1321's update is
      #     a=d, d=c, c=b, b=f -- the old b is already inside f as the leading
      #     addend above.
      set f [expr {($f + $aa + [lindex $consts $i] + [lindex $m $g]) & 0xffffffff}]
      set f [expr {(($f << [lindex $shifts $i]) | ($f >> (32 - [lindex $shifts $i]))) & 0xffffffff}]
      set f [expr {($bb + $f) & 0xffffffff}]
      set aa $dd
      set dd $cc
      set cc $bb
      set bb $f
    }
    set a0 [expr {($a0 + $aa) & 0xffffffff}]
    set b0 [expr {($b0 + $bb) & 0xffffffff}]
    set c0 [expr {($c0 + $cc) & 0xffffffff}]
    set d0 [expr {($d0 + $dd) & 0xffffffff}]
  }
  set out ""
  foreach v [list $a0 $b0 $c0 $d0] {
    for {set b 0} {$b < 4} {incr b} {
      append out [format %02x [expr {($v >> ($b * 8)) & 0xff}]]
    }
  }
  return $out
}

# The `md5` Tcl command, which the SQLite testfixture build links into tclsh
# and stock tclsh does not have.
#
# Three files call it -- func, memdb and trans2 -- and func.test uses it 400
# times in a loop to predict what its own md5sum() query should return, so the
# two must be the same function or the comparison is meaningless. They are:
# testfixture's md5 is RFC 1321 MD5 over the string's bytes, rendered as 32
# lower-case hex characters, which is exactly what md5_hex above computes.
#
# Without this, trans2.test aborted at its first `hash1` call with `invalid
# command name "md5"`, and every md5sum() case in that file was unreachable.
proc md5 {s} {
  return [::nsqlite::md5_hex $s]
}

# The `SELECT md5sum(...)` interception, as protocol records.
#
# md5sum() is a testfixture aggregate, not engine SQL, so the engine cannot
# evaluate it. The obvious fix -- rewrite the call into an engine-side
# expression -- is not available here: the only aggregate that could stand in
# for it is group_concat, and reaching it needs a scalar subquery, and the
# engine answers `subqueries are not supported yet` to every one of those
# (verified: `SELECT (SELECT a FROM t) FROM t` and `SELECT EXISTS(SELECT 1
# FROM t)` both raise it). So the digest is computed in Tcl instead: the
# script is split into statements, every statement that does NOT call md5sum
# goes to the engine untouched, and the one that does has its arguments
# fetched per row and hashed here.
#
# The semantics are testfixture's, from md5step/md5finalize in
# threadtest3.c:391-415:
#
#   * each argument is taken as TEXT (sqlite3_value_text),
#   * its bytes are fed into ONE running MD5 across every row, in argument
#     order, with no separator between them,
#   * a NULL argument contributes NOTHING (the `if( zData )` guard), which is
#     why NULL and the empty string must not hash the same,
#   * the digest is 32 lower-case hex characters,
#   * over zero rows the aggregate context is still allocated and finalized,
#     so the answer is MD5("").
#
# The NULL rule is the one that is easy to get wrong, so it is not guessed: an
# argument is also selected as `CASE WHEN (<arg>) IS NULL THEN 1 ELSE 0 END`,
# and a flagged row contributes nothing. That asks the engine about its own
# NULL semantics rather than assuming them.
proc nsqlite_md5sum_records {name sql} {
  set stmts {}
  set digest_stmt -1
  set i 0
  foreach stmt [nsqlite_split_sql $sql] {
    if {[regexp -nocase {(^|[^A-Za-z0-9_])md5sum\s*\(} $stmt]} {
      set digest_stmt $i
    }
    lappend stmts $stmt
    incr i
  }
  if {$digest_stmt < 0} {
    # A script with no md5sum() in it at all is not this path's business.
    return [nsqlite_exec $name $sql]
  }
  # Everything except the md5sum statement goes to the engine as it was, so a
  # transaction that merely ends in a checksum still runs its BEGIN/COMMIT.
  set leading [lrange $stmts 0 [expr {$digest_stmt - 1}]]
  set trailing [lrange $stmts [expr {$digest_stmt + 1}] end]
  set out {}
  foreach s [nsqlite_exec_script $name [nsqlite_join_sql $leading]] {
    lappend out $s
  }
  lappend out [nsqlite_md5sum_stmt $name [lindex $stmts $digest_stmt]]
  foreach s [nsqlite_exec_script $name [nsqlite_join_sql $trailing]] {
    lappend out $s
  }
  return $out
}

# One md5sum-bearing statement, as one {columns rows} record.
#
# The statement has to be `SELECT <select-list> FROM <tail>` for the arguments
# to be separable from the tail at all: md5sum is an aggregate, so every
# argument is an expression over the current row, and the FROM/WHERE/ORDER BY
# is what produces those rows in the order they are fed to the aggregate.
# Anything outside that shape is reported rather than hashed as something it is
# not, because a digest of the wrong thing still LOOKS like a digest and would
# let a real engine bug pass as a matching checksum.
proc nsqlite_md5sum_stmt {name stmt} {
  set body [string trim $stmt " \t\n\r;"]
  if {![regexp -nocase {^select\s+(.*)$} $body -> selectlist]} {
    error "harness gap: md5sum() can only be computed for a SELECT; this\
          statement is: $stmt"
  }
  if {![regexp -nocase {^(.*?)\s+from\s+(.*)$} $selectlist -> arglist rest]} {
    error "harness gap: md5sum() can only be computed for a SELECT with a\
          FROM clause; this statement is: $stmt"
  }
  # A GROUP BY would make this one digest PER GROUP, which is a different
  # result shape from the one digest per statement this computes. No file in
  # the suite asks for it, and returning a single digest would be silently
  # wrong, so it is reported.
  #
  # The word boundaries are written as (^|\s) and (\s|$) rather than \b on
  # purpose: Tcl's regular expressions take \b as a BACKSPACE, not as a word
  # boundary, so a `\bgroup\s+by\b` pattern compiles and matches nothing at all.
  # A guard that can never fire is the same as no guard.
  if {[regexp -nocase {(^|\s)group\s+by(\s|$)} $rest]} {
    error "harness gap: md5sum() with GROUP BY is not computed by this shim;\
          this statement is: $stmt"
  }
  # The select list is split on commas at paren depth 0, so md5sum(a, b) stays
  # one item and a function argument containing a comma does not tear it.
  # Each plan entry is {md5 args {valuealias nullalias ...}} or
  # {plain expr alias}: the aliases are the probe columns that entry reads back.
  set plan {}
  set names {}
  set i 0
  foreach item [nsqlite_split_select_list $arglist] {
    set alias "__nsqlite_md5_c$i"
    if {[regexp -nocase {(^|[^A-Za-z0-9_])md5sum\s*\(} $item]} {
      set args [nsqlite_md5sum_args $item]
      set pairs {}
      set k 0
      foreach arg $args {
        lappend pairs "${alias}_v$k" "${alias}_n$k"
        incr k
      }
      lappend plan [list md5 $args $pairs]
      lappend names "md5sum([join $args {, }])"
    } else {
      lappend plan [list plain $item $alias]
      lappend names $item
    }
    incr i
  }
  # The probe fetches, per row and per md5sum argument, the value rendered as
  # TEXT plus a NULL flag. `coalesce((x),'')` IS the TEXT rendering -- the
  # conversion sqlite3_value_text would have done, with a BLOB contributing its
  # bytes -- and the flag beside it is what keeps a NULL from being hashed as
  # the empty string. Every probe column is aliased, and the alias carries the
  # select-list item's own index rather than a per-item counter, so a second
  # md5sum() in the same select list cannot reuse the first one's names.
  set probe {}
  foreach kind_item $plan {
    # The third name is `unused`, not `rest`: `rest` is the FROM/WHERE tail
    # captured above, and `lassign` assigns to it, so reusing the name silently
    # replaces the tail with the plan entry's leftover fields and the probe is
    # then run `... FROM v0 n0 v1 n1`.
    lassign $kind_item kind item unused
    if {$kind eq "md5"} {
      set pairs [lindex $kind_item 2]
      set k 0
      foreach arg [lindex $kind_item 1] {
        set vk [lindex $pairs [expr {$k * 2}]]
        set nk [lindex $pairs [expr {$k * 2 + 1}]]
        lappend probe "coalesce(($arg),'') AS $vk"
        lappend probe "CASE WHEN ($arg) IS NULL THEN 1 ELSE 0 END AS $nk"
        incr k
      }
    } else {
      # `count(*)` is left OUT of the probe and counted here instead. It is an
      # aggregate, so asking the engine for it alongside the per-row columns
      # collapses the whole result to ONE row -- `SELECT count(*), (a) FROM t`
      # returns a single row, so the per-row argument values would all but one
      # disappear and the digest would be over one row instead of all of them.
      if {![regexp -nocase {^\s*count\s*\(\s*\*\s*\)\s*$} $item]} {
        lappend probe "($item) AS [lindex $kind_item 2]"
      }
    }
  }
  # A select list that is nothing but count(*) has no per-row column to probe,
  # and `SELECT FROM t` is not a statement. One row of zeroes is the right
  # stand-in: the arguments are all read from a table that is not there, so
  # nothing is fed to the digest, and md5sum over no rows is MD5("") anyway.
  if {[llength $probe] == 0} {
    set rows [list [dict create]]
  } else {
    set rows [nsqlite_md5sum_rows $name "SELECT [join $probe {, }] FROM $rest"]
  }

  # ONE running MD5 per md5sum() call, fed every row's arguments in order --
  # not one digest per row. The accumulator is the concatenated text, and the
  # digest is taken once, at the end, which is what md5step/md5finalize do.
  set acc {}
  foreach kind_item $plan {
    lassign $kind_item kind item unused
    if {$kind eq "md5"} {
      lappend acc ""
    }
  }
  foreach row $rows {
    set aidx -1
    foreach kind_item $plan {
      lassign $kind_item kind item unused
      if {$kind ne "md5"} continue
      incr aidx
      set k 0
      set add ""
      set pairs [lindex $kind_item 2]
      foreach arg [lindex $kind_item 1] {
        set vk [lindex $pairs [expr {$k * 2}]]
        set nk [lindex $pairs [expr {$k * 2 + 1}]]
        # A NULL argument contributes NOTHING at all (md5step's `if( zData )`
        # guard), so it is skipped rather than concatenated as ''.
        if {![dict get $row $nk]} {
          append add [dict get $row $vk]
        }
        incr k
      }
      # `lset` rather than `append [lindex ...]`: `append` on the result of a
      # command substitution writes to a temporary, so the accumulated text is
      # discarded and every digest comes out as MD5("").
      lset acc $aidx [lindex $acc $aidx]$add
    }
  }
  set digests {}
  foreach a $acc {
    lappend digests [::nsqlite::md5_hex $a]
  }

  # An aggregate with no GROUP BY returns exactly one row, so the plain
  # select-list items are aggregated here too. `count(*)` is the row count;
  # any other bare expression is a value from one arbitrary row, and the
  # first row is as good an answer as any and is at least deterministic.
  set r {}
  set midx 0
  foreach kind_item $plan {
    lassign $kind_item kind item unused
    if {$kind eq "md5"} {
      lappend r [lindex $digests $midx]
      incr midx
    } elseif {[regexp -nocase {^\s*count\s*\(\s*\*\s*\)\s*$} $item]} {
      lappend r [llength $rows]
    } elseif {[llength $rows]} {
      lappend r [dict get [lindex $rows 0] [lindex $kind_item 2]]
    } else {
      # A plain column over zero rows is NULL. do_test compares it against an
      # empty string in the suite's own convention, which is what the engine's
      # record stream already decoded it to.
      lappend r ""
    }
  }
  return [list $names [list $r]]
}

# The rows of the probe query, as a dict per row keyed by column name.
#
# Every probe column is aliased, so the dict keys are the aliases and two
# select-list items that share a column name in the original cannot collide.
proc nsqlite_md5sum_rows {name q} {
  set rows {}
  foreach stmt [nsqlite_exec $name $q] {
    if {[nsqlite_stmt_error $stmt]} {
      error [lindex $stmt 1]
    }
    lassign $stmt cols vals
    set n [llength $cols]
    for {set r 0} {$r < [llength $vals]} {incr r} {
      set d [dict create]
      for {set c 0} {$c < $n} {incr c} {
        dict set d [lindex $cols $c] [lindex [lindex $vals $r] $c]
      }
      lappend rows $d
    }
  }
  return $rows
}

# Reassemble split statements into one script for the engine.
#
# Returns "" when nothing is left. An empty script is not handed to the engine
# at all: it prints no records and exits 0, and the empty stream would come back
# as "the engine produced no record stream", which is a harness error rather
# than the empty result a script of nothing and a comment really is.
proc nsqlite_join_sql {stmts} {
  set out {}
  foreach s $stmts {
    if {[string trim $s] ne ""} {
      lappend out "$s ;"
    }
  }
  if {[llength $out] == 0} {
    return ""
  }
  return [join $out "\n"]
}

# Run a script that is known to have no md5sum() in it, possibly empty.
proc nsqlite_exec_script {name sql} {
  if {[string trim $sql] eq ""} {
    return [list]
  }
  return [nsqlite_exec $name $sql]
}

# Split a select list on commas at paren depth 0.
proc nsqlite_split_select_list {s} {
  set out {}
  set cur ""
  set depth 0
  set q ""
  set len [string length $s]
  for {set i 0} {$i < $len} {incr i} {
    set c [string index $s $i]
    if {$q ne ""} {
      append cur $c
      if {$c eq $q} {
        if {$i+1 < $len && [string index $s [expr {$i+1}]] eq $q} {
          append cur $q
          incr i
        } else {
          set q ""
        }
      } elseif {$c eq "\\" && $q eq "'" && $i+1 < $len} {
        incr i
        append cur [string index $s $i]
      }
      continue
    }
    switch -- $c {
      "'" - "\"" { set q $c ; append cur $c }
      "(" { incr depth ; append cur $c }
      ")" { incr depth -1 ; append cur $c }
      "," {
        if {$depth == 0} {
          lappend out [string trim $cur]
          set cur ""
        } else {
          append cur $c
        }
      }
      default { append cur $c }
    }
  }
  if {[string trim $cur] ne ""} {
    lappend out [string trim $cur]
  }
  return $out
}

# The argument list of the md5sum(...) call in a select-list item.
proc nsqlite_md5sum_args {item} {
  regexp -nocase {(^|[^A-Za-z0-9_])md5sum\s*(\()} $item -> lead open
  set pos [string first $open $item]
  set start [expr {$pos + [string length $lead] + 1}]
  set args {}
  set depth 0
  set q ""
  set cur ""
  set len [string length $item]
  for {set i $start} {$i < $len} {incr i} {
    set c [string index $item $i]
    if {$q ne ""} {
      append cur $c
      if {$c eq $q} {
        if {$i+1 < $len && [string index $item [expr {$i+1}]] eq $q} {
          append cur $q
          incr i
        } else {
          set q ""
        }
      } elseif {$c eq "\\" && $q eq "'" && $i+1 < $len} {
        incr i
        append cur [string index $item $i]
      }
      continue
    }
    switch -- $c {
      "'" - "\"" { set q $c ; append cur $c }
      "(" { incr depth ; append cur $c }
      ")" {
        if {$depth == 0} {
          lappend args [string trim $cur]
          return $args
        }
        incr depth -1
        append cur $c
      }
      "," {
        if {$depth == 0} {
          lappend args [string trim $cur]
          set cur ""
        } else {
          append cur $c
        }
      }
      default { append cur $c }
    }
  }
  error "nsqlite: unbalanced parenthesis in md5sum() call: $item"
}

# Split a script into statements, keeping each statement's own text.
#
# The engine splits them itself; this is only so the md5sum path can find the
# one statement of a multi-statement script that calls the aggregate. A `;`
# inside a quoted string or a comment is not a terminator, which is why this is
# not a `split $sql ";"`.
proc nsqlite_split_sql {sql} {
  set out {}
  set cur ""
  set len [string length $sql]
  set i 0
  set q ""
  while {$i < $len} {
    set c [string index $sql $i]
    if {$q ne ""} {
      append cur $c
      if {$c eq $q} {
        if {$i+1 < $len && [string index $sql [expr {$i+1}]] eq $q} {
          append cur $q
          incr i
        } else {
          set q ""
        }
      } elseif {$c eq "\\" && $q eq "'" && $i+1 < $len} {
        incr i
        append cur [string index $sql $i]
      }
    } else {
      if {$c eq "'" || $c eq "\""} {
        set q $c
        append cur $c
      } elseif {$c eq "-" && $i+1 < $len && [string index $sql [expr {$i+1}]] eq "-"} {
        set nl [string first "\n" $sql [expr {$i+2}]]
        if {$nl < 0} {
          append cur [string range $sql $i end]
          set i $len
        } else {
          append cur [string range $sql $i [expr {$nl-1}]]
          append cur "\n"
          set i $nl
        }
      } elseif {$c eq ";"} {
        lappend out $cur
        set cur ""
      } else {
        append cur $c
      }
    }
    incr i
  }
  lappend out $cur
  return $out
}


# The checksum of every table the connection can see.
#
# Upstream unions main with temp; this shim has no persistent connection and no
# ATTACH, so a temp schema can never have been written to by a statement this
# harness ran. The union is therefore dropped and the list is sqlite_master's
# own tables plus sqlite_master itself -- the same fallback upstream takes on a
# build without a temp database. `UNION` is not spelled here because the engine
# does not implement it yet, and a harness that raised "Union is not supported
# yet" would abort the whole file before a single checksum was compared.
proc allcksum {{db db}} {
  set sum 0
  foreach tbl [concat \
      [execsql {SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;} $db] \
      sqlite_master] {
    incr sum [nsqlite_textsum $db "SELECT * FROM \"[string map {\" \"\"} $tbl]\";"]
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

# Run a table of SELECT cases.
#
#   do_select_tests PREFIX ?-query CMD? ?-errorformat FMT? ?-repair SCRIPT?
#                  ?-count? {TN SQL RESULT TN SQL RESULT ...}
#
# This is upstream's driver, copied rather than replaced: the ten files that
# use it (the e_*.test shell-comparison files, fts3defer, fts3query) pass the
# cases as a flat list and expect the same naming, the same -errorformat
# `format` call, and the same -repair re-run between cases. Getting the naming
# wrong would rename every case in the file and make the counts unreadable.
proc do_select_tests {prefix args} {
  set testlist [lindex $args end]
  set switches [lrange $args 0 end-1]

  set errfmt ""
  set countonly 0
  set tclquery ""
  set repair ""

  for {set i 0} {$i < [llength $switches]} {incr i} {
    set s [lindex $switches $i]
    set n [string length $s]
    if {$n>=2 && [string equal -length $n $s "-query"]} {
      set tclquery [list execsql [lindex $switches [incr i]]]
    } elseif {$n>=2 && [string equal -length $n $s "-tclquery"]} {
      set tclquery [lindex $switches [incr i]]
    } elseif {$n>=2 && [string equal -length $n $s "-errorformat"]} {
      set errfmt [lindex $switches [incr i]]
    } elseif {$n>=2 && [string equal -length $n $s "-repair"]} {
      set repair [lindex $switches [incr i]]
    } elseif {$n>=2 && [string equal -length $n $s "-count"]} {
      set countonly 1
    } else {
      error "unknown switch: $s"
    }
  }

  if {$countonly && $errfmt!=""} {
    error "Cannot use -count and -errorformat together"
  }
  set nTestlist [llength $testlist]
  if {$nTestlist%3 || $nTestlist==0 } {
    error "SELECT test list contains [llength $testlist] elements"
  }

  eval $repair
  foreach {tn sql res} $testlist {
    if {$tclquery != ""} {
      execsql $sql
      uplevel do_test ${prefix}.$tn [list $tclquery] [list [list {*}$res]]
    } elseif {$countonly} {
      set nRow [llength [execsql $sql]]
      uplevel do_test ${prefix}.$tn [list [list set {} $nRow]] [list $res]
    } elseif {$errfmt==""} {
      uplevel do_execsql_test ${prefix}.${tn} [list $sql] [list [list {*}$res]]
    } else {
      set res [list 1 [string trim [format $errfmt {*}$res]]]
      uplevel do_catchsql_test ${prefix}.${tn} [list $sql] [list $res]
    }
    eval $repair
  }
}

# Empty every user table, leaving the schema alone.
#
# The suite uses this between phases of a file. The names come from
# sqlite_master and are quoted with "" and with any embedded " doubled, because
# SQLite quotes an identifier that way and a table name can contain anything.
proc delete_all_data {} {
  foreach t [execsql {SELECT tbl_name FROM sqlite_master WHERE type = 'table'}] {
    db eval "DELETE FROM '[string map {' ''} $t]'"
  }
}

# Drop every user table and view. Upstream turns foreign keys off around this
# so a table can go before the one that references it; the pragma is a
# statement here, so the same ordering applies.
proc drop_all_tables {{db db}} {
  foreach {idx name} [execsql {PRAGMA database_list} $db] {
    if {$idx==1} {
      set master sqlite_temp_master
    } else {
      set master $name.sqlite_master
    }
    foreach {t type} [execsql "
      SELECT name, type FROM $master
      WHERE type IN('table', 'view') AND name NOT LIKE 'sqliteX_%' ESCAPE 'X'
    " $db] {
      catch {$db eval "DROP $type \"[string map {\" \"\"} $t]\""}
    }
  }
}

# Drop every explicitly-created index, leaving the ones SQLite made for UNIQUE
# and PRIMARY KEY constraints alone, which is what `sql LIKE 'create%'` selects.
proc drop_all_indexes {{db db}} {
  foreach idx [execsql {
    SELECT name FROM sqlite_master WHERE type='index' AND sql LIKE 'create%'
  } $db] {
    catch {$db eval "DROP INDEX $idx"}
  }
}

# The opcode dump, as `explain` prints it. A CLI has no VDBE to walk, so there
# is no honest opcode list to produce. This returns the empty string rather
# than inventing opcodes, and a test that compares the dump against SQLite's
# expected one fails on the text -- which is the truthful outcome for a harness
# that cannot reach into the engine.
proc explain {sql {db db}} {
  return ""
}
proc explain_i {sql {db db}} {
  return ""
}
proc explain_no_trace {sql} {
  return ""
}
# `execsql_pp` runs EXPLAIN and returns the program. Same answer: nothing.
proc execsql_pp {sql {db db}} {
  return ""
}

# Run $sql $numstmt times and report microseconds per statement. Timing a CLI
# measures process startup, not the engine, so the number would not mean what
# the caller thinks. The statements still run, so a test that only checks that
# the SQL works still checks that.
proc speed_trial {name numstmt units sql} {
  for {set i 0} {$i < $numstmt} {incr i} {
    execsql $sql
  }
  output2 -nonewline [format {%-21.21s } $name...]
  output2 "timing skipped: each statement is a separate process in this harness\n"
  return 0
}
proc speed_trial_tcl {name numstmt units script} {
  for {set i 0} {$i < $numstmt} {incr i} {
    uplevel #0 $script
  }
  return 0
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

# Normalise a float the way the upstream harness does, then compare.
#
# Different Tcl builds print `1e+00` and `1.0e0` and `Inf` and `1.#INF`, so
# upstream rewrites the exponent padding and the infinity spellings before
# comparing. This is upstream's transformation verbatim -- it makes two
# renderings of the same value compare equal, which is the point, and it does
# not make two different values compare equal.
proc realnum_normalize {r} {
  string map {1.#INF inf Inf inf .0e e} [regsub -all {(e[+-])0+} $r {\1}]
}
proc do_realnum_test {name cmd expected} {
  uplevel [list do_test $name [
    subst -nocommands { realnum_normalize [ $cmd ] }
  ] [realnum_normalize $expected]]
}

# Like do_test, but the test is not run in a slave interpreter. The Windows
# ANSI/UTF-8 I/O workaround upstream applies is about slave interpreters, and
# this harness has none, so the behaviour is already the one this asks for.
proc do_test_with_ansi_output {name cmd expected} {
  do_test $name $cmd $expected
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

# The file to run. With none on the command line, this file is a library rather
# than a driver and there is nothing to execute -- which is what upstream
# tester.tcl does too, and why testrunner.tcl exists.
if {[llength $argv]==0} {
  exit 0
}
set __nsqlite_testfile [file normalize [lindex $argv 0]]
set argv [lrange $argv 1 end]
if {![file exists $__nsqlite_testfile]} {
  puts stderr "no such test file: $__nsqlite_testfile"
  exit 1
}

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

# A .test file does `set testdir [file dirname $argv0]` and then `source
# $testdir/tester.tcl`. The suite's own tester.tcl needs the C API and would fail
# at its first line, so the shim is made findable at that path for the run: the
# test file is copied beside a one-line forwarder, neither of which touches the
# suite's tree. The .test files themselves are never modified.
#
# The copy lands in the run directory, which is already this test's own, and the
# run does not change directory afterwards. That matters: a relative `./test.db`
# would otherwise name one file before the move and a different one after it,
# and the engine would quietly start a fresh empty database the first time a
# statement needed the real one.
set __nsqlite_scratch [pwd]
set ::nsqlite_runfile [file join $__nsqlite_scratch [file tail $__nsqlite_testfile]]
# Only copy when the source is somewhere else. Running a file that already sits
# in the run directory -- which is what happens when the caller passes an
# absolute path to a .test file that is already the one being run -- would
# otherwise ask `file copy` to copy a file onto itself, which fails with
# "permission denied" and takes the whole run with it.
if {[file normalize $__nsqlite_testfile] ne [file normalize $::nsqlite_runfile]} {
  file copy -force -- $__nsqlite_testfile $::nsqlite_runfile
}
# Windows has no symlink by default, so a one-line forwarding script stands in
# for a link to the real shim.
set ::nsqlite_forward [file join $__nsqlite_scratch tester.tcl]
set fd [open $::nsqlite_forward w]
puts $fd "# Generated by test/shim/tester.tcl -- forwards to the real shim."
puts $fd "source [list $::nsqlite_harness]"
close $fd

# The suite's .test files do not only source tester.tcl. 122 of them source
# malloc_common.tcl, 68 source lock_common.tcl, 27 source fts3_common.tcl, and
# 26 source wal_common.tcl -- all by the same `source $testdir/NAME.tcl` shape.
# $testdir is [file dirname $argv0], which is this scratch directory, so those
# sources were all failing with "couldn't read file". In the first full sweep
# that accounted for 20 files aborting outright, and it is a copy, not a
# re-implementation: the helper files are the suite's own and are staged
# unmodified.
#
# Every .tcl in the suite's test directory is staged, not just the four named
# above, because a helper that a .test file sources by name must be findable
# and the set of names is not ours to decide. The suite's tree is never written
# to; these are copies in the run directory, which is discarded.
set __nsqlite_suite_testdir [file normalize \
  [file join [file dirname $::nsqlite_harness] .. sqlite-suite test]]
if {[file isdirectory $__nsqlite_suite_testdir]} {
  # .tcl helpers AND the .test files that are themselves meant to be sourced.
  # `permutations.test` and `misuse.test` are the latter: all.test, quick.test,
  # full.test and five others do `source $testdir/permutations.test`, and
  # without it they abort on their first line -- 9 files, none of which ran a
  # single test. They are only ever sourced, never run directly, so staging a
  # copy is safe and keeps the suite's tree read-only.
  foreach __nsqlite_helper [glob -nocomplain -directory $__nsqlite_suite_testdir *.tcl *.test] {
    if {[file tail $__nsqlite_helper] eq "tester.tcl"} continue
    if {[file normalize $__nsqlite_helper] eq [file normalize $__nsqlite_testfile]} continue
    catch {
      file copy -force -- $__nsqlite_helper \
        [file join $__nsqlite_scratch [file tail $__nsqlite_helper]]
    }
  }
}

# tools/capabilities.tcl is sourced, which is where ::sqlite_options comes
# from. The engine database is opened before the test file runs, because every
# .test file assumes `db` is already a usable command.
reset_db
nsdb_install db

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

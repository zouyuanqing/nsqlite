#!/usr/bin/env bash
# Runs .test files from the official SQLite TCL suite against nsqlite, through
# the tester.tcl shim in test/shim.
#
# WHAT THIS IS
#
# The suite never talks to an engine over a subprocess: its own harness calls
# sqlite3_initialize, install_malloc_faultsim and autoinstall_test_functions
# before the first test executes, all of which come from the SQLite "testfixture"
# build. test/shim/tester.tcl is a replacement harness with the same command
# vocabulary that drives the nsqlited CLI over `exec` instead, which is the only
# option that does not need a Tcl extension for the C API.
#
# That means this script drives test/shim/tester.tcl, NOT the suite's
# testrunner.tcl. A shim replaces tester.tcl, not the driver, and the shim is
# self-driving: it takes <permutation> <file.test> and runs the file itself.
# Pointing this at testrunner.tcl would re-enter the stock harness, which
# cannot work -- see docs/testing.md.
#
# OUTPUT
#
# One line per file, machine-readable, and a summary sorted by how close each
# file is to passing. The per-file numbers come from the shim's own final
# summary line ("N errors out of M tests"), which is the same counter
# finalize_testing prints upstream. A file that aborts before any test runs has
# no such line, and is reported as ABORTED with 0/0 -- a distinct, visible
# state from "ran 50 tests and failed 1", which is the number that matters when
# deciding what to work on next.
#
# Usage:
#   tools/run_suite.sh 'select1.test'      # one file
#   tools/run_suite.sh 'select*'           # a glob over the suite
#   tools/run_suite.sh 'select1.test' 'types.test'   # several files
#   tools/run_suite.sh --permutation full 'alter*'
#   tools/run_suite.sh --list              # list the files, run nothing
#   tools/run_suite.sh --quiet 'select*'   # summary only
#   tools/run_suite.sh --jobs 8 'select*'  # run files in parallel
#   tools/run_suite.sh --maxerror 1 'select1.test'   # stop a file after 1 failure
#
#   tools/run_suite.sh 'shimselftest.test'  # this harness's own tests
#
# Environment:
#   TCLSH      tclsh to use       (default: tclsh on PATH, else the ucrt64 build)
#   NSQLITED   the engine binary   (default: target/debug/nsqlited.exe)
#   TCLTEST_PART  passed through if set in the environment
#
# Exit status:
#   0  every matched file ran its tests and passed them
#   1  something did not pass: a failure, an abort, or a file that ran no tests
#   2  bad arguments, or no test files matched
#   3  the harness, the engine, or tclsh is missing
#
# Exit 1 covers three outcomes that are all "not verified": a file that ran and
# failed some, a file that aborted before finishing, and a file whose whole body
# was behind an ifcapable that did not hold and so ran 0 of 0. The last is a
# legitimate result and is reported as SKIPPED, but it is still not a pass, and
# a run that treats it as one is reporting a number rather than a measurement.
###
set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
SUITE="$DIR/test/sqlite-suite/test"
SHIM="$DIR/test/shim/tester.tcl"
SHIM_DIR="$DIR/test/shim"
RUN_ROOT="$DIR/test/sqlite-run"
PERMUTATION=""
PATTERNS=()
JOBS=1
QUIET=0
LIST=0
MAXERROR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --_run-one)
            # Internal: the per-file worker the --jobs path re-enters. It is
            # hidden because it is not a useful thing to type, but it is a real
            # mode rather than a `bash -c` string so that run_one has exactly
            # one definition.
            shift
            PATTERNS=("$1"); shift
            QUIET=1
            ;;
        --permutation)   PERMUTATION="${2:-}"; shift 2 ;;
        --permutation=*) PERMUTATION="${1#--permutation=}"; shift ;;
        --jobs|-j)       JOBS="${2:-}"; shift 2 ;;
        --jobs=*)        JOBS="${1#--jobs=}"; shift ;;
        -j*)             JOBS="${1#-j}"; shift ;;
        --maxerror)      MAXERROR="${2:-}"; shift 2 ;;
        --maxerror=*)    MAXERROR="${1#--maxerror=}"; shift ;;
        -q|--quiet)      QUIET=1; shift ;;
        --list)          LIST=1; shift ;;
        -h|--help)       sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)              echo "error: unknown option '$1' (try --help)" >&2; exit 2 ;;
        *)               PATTERNS+=("$1"); shift ;;
    esac
done

case "$JOBS" in
    ''|*[!0-9]*) echo "error: --jobs wants a positive integer, got '$JOBS'" >&2; exit 2 ;;
esac
if [ "$JOBS" -lt 1 ]; then
    echo "error: --jobs wants a positive integer, got '$JOBS'" >&2
    exit 2
fi

if [ "${#PATTERNS[@]}" -eq 0 ]; then
    echo "error: no glob pattern given (try --help)" >&2
    exit 2
fi

[ -f "$SHIM" ] || { echo "error: harness not found: $SHIM" >&2; exit 3; }
[ -d "$SUITE" ] || { echo "error: suite not fetched: $SUITE" >&2; exit 3; }

# --- locate tclsh ----------------------------------------------------------
#
# Every probe must be a real FILE, not a here-doc: tclsh reads a piped script,
# but the mingw and ucrt64 builds do not report its failure in the exit status
# (both return 0 even when the script raised an error), so a here-doc probe
# would silently pass everything.
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
printf 'puts [info patchlevel]\n' > "$SCRATCH/ver.tcl"

TCLSH="${TCLSH:-}"
if [ -z "$TCLSH" ]; then
    if command -v tclsh >/dev/null 2>&1; then
        TCLSH="$(command -v tclsh)"
    elif [ -x "C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/tclsh.exe" ]; then
        TCLSH="C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/tclsh.exe"
    else
        echo "error: tclsh not found on PATH" >&2
        echo "       set TCLSH=/path/to/tclsh, or install tcl 8.6+" >&2
        exit 3
    fi
fi
if ! "$TCLSH" "$SCRATCH/ver.tcl" > "$SCRATCH/ver.out" 2>&1; then
    echo "error: TCLSH=$TCLSH is not a working tclsh" >&2
    exit 3
fi
TCL_VERSION="$(tr -d '\r' < "$SCRATCH/ver.out")"

NSQLITED="${NSQLITED:-$DIR/target/debug/nsqlited.exe}"
if [ ! -x "$NSQLITED" ]; then
    echo "error: engine not built: $NSQLITED" >&2
    echo "       run 'cargo build -p nsqlited' first" >&2
    exit 3
fi
export NSQLITED

# --- resolve the globs to concrete files -----------------------------------
#
# A bare name like "select1.test" is not a glob; a pattern like "select*" is.
# Several patterns are allowed and are de-duplicated, so a list of files works
# as well as a single glob.
#
# Two roots are searched, not one. $SUITE is the pristine suite checkout, and
# $SHIM_DIR holds this harness's own .test files -- `shimselftest.test`, which
# tests the harness rather than the engine and has to be runnable through the
# same entry point as everything else, or it is a test nobody runs. Matches are
# recorded as a path relative to the repo root so the two roots never collide
# on a name.
MATCHES=()
for PATTERN in "${PATTERNS[@]}"; do
    found=0
    for root in "$SUITE" "$SHIM_DIR"; do
        if [ -f "$root/$PATTERN" ]; then
            MATCHES+=("$root/$PATTERN")
            found=1
        else
            for f in "$root"/*.test; do
                [ -e "$f" ] || continue
                b="$(basename "$f")"
                # shellcheck disable=SC2254 # $PATTERN is a glob on purpose
                case "$b" in $PATTERN) MATCHES+=("$f"); found=1 ;; esac
            done
        fi
    done
    [ "$found" -eq 1 ] || NO_MATCH="${NO_MATCH:-} $PATTERN"
done
if [ "${#MATCHES[@]}" -eq 0 ]; then
    echo "error: no test files match '${NO_MATCH:-${PATTERNS[*]}}'" >&2
    echo "       looked in $SUITE ($(ls "$SUITE" | grep -c '\.test$') .test files)" >&2
    echo "       and in $SHIM_DIR ($(ls "$SHIM_DIR" | grep -c '\.test$') .test files)" >&2
    exit 2
fi
# de-duplicate, keeping first-seen order
UNIQ=()
for m in "${MATCHES[@]}"; do
    dup=0
    for u in "${UNIQ[@]}"; do [ "$u" = "$m" ] && { dup=1; break; }; done
    [ "$dup" -eq 0 ] && UNIQ+=("$m")
done
MATCHES=("${UNIQ[@]}")

SUITE_VER="$(tr -d ' \t\n\r' < "$DIR/test/sqlite-suite/VERSION" 2>/dev/null || echo unknown)"

if [ "$LIST" -eq 1 ]; then
    printf '%s\n' "${MATCHES[@]}"
    exit 0
fi

# --- report what is about to happen ----------------------------------------
echo "nsqlite test runner (shim)"
echo "  suite      $SUITE_VER  ($SUITE)"
echo "  pattern    ${PATTERNS[*]}  ->  ${#MATCHES[@]} file(s)"
echo "  tclsh      $TCLSH (Tcl $TCL_VERSION)"
echo "  harness    $SHIM"
echo "  engine     $NSQLITED"
[ -n "$PERMUTATION" ] && echo "  permutation $PERMUTATION"
[ -n "$MAXERROR" ] && echo "  maxerror   $MAXERROR"
[ "$JOBS" -gt 1 ] && echo "  jobs       $JOBS"
[ -n "${TCLTEST_PART:-}" ] && echo "  TCLTEST_PART $TCLTEST_PART (from environment)"
echo "  scratch    $RUN_ROOT"
echo

# The .test file locates its harness from [file dirname $argv0]; the shim makes
# that resolve to itself. TESTDIR is still exported because the harness and
# several .test files read it anyway.
export TESTDIR="$SUITE"
export SQLITE_TEST_DIR="$SUITE"

# --- run -------------------------------------------------------------------
#
# Each file gets its own scratch directory. The suite deletes and reopens
# test.db constantly, so sharing one would make a result depend on leftovers
# and make a crash hard to attribute.
#
# The per-file result is three fields: errors, tests, aborted. They come from
# the shim's own summary, which is the counter finalize_testing maintains --
# the same counter upstream's own harness prints, so the number is comparable
# to an upstream run rather than a shim invention.
mkdir -p "$RUN_ROOT"

# Parse one log into "errors tests aborted" and print it. `aborted` is 1 when
# the file stopped on an uncaught error before finishing: a distinct state from
# a run whose tests failed, because it hides every test after the abort point.
summarize() {
    local log="$1"
    local line errs tots ab
    line="$(grep -E '^[0-9]+ errors? out of [0-9]+ tests' "$log" 2>/dev/null | tail -1)"
    errs="$(printf '%s' "$line" | grep -oE '^[0-9]+' | head -1)"
    tots="$(printf '%s' "$line" | grep -oE 'out of [0-9]+ tests' | grep -oE '[0-9]+' | head -1)"
    ab="$(grep -c '^! <file aborted>' "$log" 2>/dev/null)"
    printf '%s %s %s\n' "${errs:-NA}" "${tots:-0}" "${ab:-0}"
}

run_one() {
    local path="$1"
    local name
    name="$(basename "$path")"

    local work="$RUN_ROOT/${name%.test}"
    local log="$work/output.txt"
    local shim_args=()
    [ -n "$PERMUTATION" ] && shim_args=("$PERMUTATION")
    [ -n "$MAXERROR" ] && shim_args+=("--maxerror=$MAXERROR")

    # The path is made ABSOLUTE before anything changes directory, because the
    # subshell below cd's into the work directory. A relative test-file path
    # then stops resolving and the shim reports "no such test file", which
    # reads as a file that produced no results rather than as a bug in this
    # script. It only shows up with --jobs, because that is the only path where
    # the argument arrives relative.
    # $path may be a resolved full path (the sequential and reporting paths
    # pass one) or a bare file name (the --jobs worker re-resolves its single
    # argument through the same roots, so it hands run_one a name). Both are
    # turned into an absolute path here, because the subshell cd's into the
    # work directory and a relative one stops resolving there.
    local abs
    if [ -f "$path" ]; then
        abs="$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
    else
        for root in "$SUITE" "$SHIM_DIR"; do
            if [ -f "$root/$path" ]; then
                abs="$root/$path"
                break
            fi
        done
        [ -n "${abs:-}" ] || abs="$path"
    fi
    rm -rf "$work"
    mkdir -p "$work"
    ( cd "$work" && "$TCLSH" "$SHIM" "${shim_args[@]}" "$abs" ) > "$log" 2>&1
    local rc=$?
    local errs tots ab
    read -r errs tots ab <<< "$(summarize "$log")"
    # A file that never printed a summary is an abort, whatever the exit status
    # said: exit 0 with no summary is a harness crash, not a pass.
    #
    # A file that DID print one, with 0 tests in it, is not an abort -- that is
    # a capability skip (`ifcapable !vtab` around a whole file) and the summary
    # is how the harness says so. Forcing ab=1 there would report every skipped
    # file as a crash, which is the opposite of what happened.
    if [ "$errs" = "NA" ]; then
        ab=1
    fi
    # rc non-zero with a summary that says zero errors is a contradiction: the
    # file's own exit said it failed, but nothing in the summary says why. That
    # is an abort's signature, not a pass, so it is recorded as one. The
    # previous form of this branch assigned `errs` to itself, which changed
    # nothing and left a non-zero exit reporting `ok` -- the exit status was
    # recorded in column 5 all along, so no number was ever wrong, but the code
    # claimed to enforce something it did not.
    if [ "$rc" -ne 0 ] && [ "$errs" = "0" ] && [ "$tots" != "0" ]; then
        ab=1
    fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$errs" "$tots" "$ab" "$rc" > "$work/result.tsv"
    if [ "$QUIET" -eq 0 ]; then
        if [ "$errs" = "NA" ] || [ "$ab" != "0" ]; then
            printf '=== %s === aborted before the summary\n' "$name"
        fi
    fi
    return 0
}

export -f summarize run_one 2>/dev/null || true

# The --jobs path re-enters this script once per file with this flag. It runs
# that one file, writes its result.tsv, and exits -- the parent collects those
# files and does the reporting. It has to sit HERE, immediately after run_one is
# defined: further down it ran the whole pipeline for one file, so the worker
# printed a summary and then the parent's next `rm -rf` of the same work
# directory deleted the result the parent was about to read.
if [ "${NSQLITE_WORKER:-0}" = "1" ]; then
    run_one "${PATTERNS[0]}"
    exit 0
fi

if [ "$JOBS" -gt 1 ]; then
    # Re-enter this script per file rather than re-implementing the tclsh and
    # engine discovery in a `bash -c` string. One code path means a fix to
    # run_one cannot be applied to one and missed in the other -- which is how a
    # "passes with --jobs 1, fails with --jobs 8" bug gets in.
    printf '%s\n' "${MATCHES[@]}" \
        | xargs -P "$JOBS" -I@ env NSQLITE_WORKER=1 "$0" --_run-one "@" >/dev/null 2>&1

    # A worker that died leaves no result.tsv, and the summary below would then
    # report that file as having produced no results. That is the honest
    # reading of a crash, but a crash caused by the runner is this script's
    # bug and must not be reported as the engine's. So anything still missing
    # is retried once, serially, where a failure is visible rather than
    # swallowed by the redirect above.
    for path in "${MATCHES[@]}"; do
        work="$RUN_ROOT/$(basename "${path%.test}")"
        if [ ! -f "$work/result.tsv" ]; then
            echo "run_suite: retrying $(basename "$path") serially" >&2
            run_one "$path"
        fi
    done
else
    for path in "${MATCHES[@]}"; do
        run_one "$path"
    done
fi

# --- summary ---------------------------------------------------------------
RESULTS="$RUN_ROOT/.results.tsv"
: > "$RESULTS"
for path in "${MATCHES[@]}"; do
    name="$(basename "$path")"
    work="$RUN_ROOT/${name%.test}"
    if [ -f "$work/result.tsv" ]; then
        cat "$work/result.tsv" >> "$RESULTS"
    else
        # The file never produced a result row: treat as a hard abort.
        printf '%s\tNA\t0\t1\t?\n' "$name" >> "$RESULTS"
    fi
done

echo
echo "========================================"
echo "per file  (sorted by how close to passing: pass, then fewest errors)"
echo
printf '%-28s %7s %7s %8s\n' "file" "errors" "tests" "aborted"
printf '%-28s %7s %7s %8s\n' "----------------------------" "-------" "-------" "--------"

# Sort key, in order of what "closest to passing" means:
#
#   0  passed: ran tests, no failures
#   1  ran tests, some failed -- ordered by fewest errors, then most tests
#   2  produced no summary at all (a harness or engine crash)
#   3  ran no tests -- "0 errors" here means no information, not correctness
#
# The whole line is carried through the key as a single trailing field rather
# than being re-sliced, so a name containing a tab cannot shift the columns.
sort -t$'\t' -k1,1 "$RESULTS" \
  | awk -F'\t' -v OFS='\t' '
      function key(  e, t, a) {
        e = $2; t = $3 + 0; a = $4 + 0
        # 0 = passed, 1 = ran to the end and failed,
        # 2 = aborted before or during the file, 3 = ran no tests.
        # A 0/0 file lands in 3 whatever its error count: it produced no
        # evidence either way.
        #
        # An abort lands in 2 whether it happened before the first test or
        # half way through. The two are not equally far from passing -- one has
        # not started, the other has a real error count that flatters it by
        # hiding everything after the abort -- so the second key ranks a
        # mid-file abort by the errors it did record, which is what makes
        # "closest to passing" mean something for those files.
        if (t == 0)                return 3
        if (a != 0)                return 2
        if (e == "0")              return 0
        return 1
      }
      { print key(), (($2 == "NA") ? 999999 : $2 + 0), -($3 + 0), $0 }
    ' \
  | sort -t$'\t' -k1,1n -k2,2n -k3,3n \
  | cut -f4- \
  | while IFS=$'\t' read -r name errs tots ab rc; do
      # Five states, kept distinct on purpose. A file that ran no tests is NOT
      # a pass: `ifcapable !vtab` around a whole file, a crash before the first
      # test, and a file that genuinely has nothing to check all produce 0/0,
      # and only the first is a legitimate outcome. Reporting it as "ok" would
      # make a broken harness look like a clean sheet.
      #
      # A file that ran tests and then ABORTED partway is also its own state,
      # and it is not the same as one that ran to the end. Every case after the
      # abort point never executed, so its error count covers only the part of
      # the file that got that far: the ratio flatters it. select1, types and
      # trans all do this. Calling that FAILED says the file failed; it does not
      # say how much of it was even looked at, which is the number that decides
      # whether the file is close to passing or is barely started.
      if [ "$tots" = "0" ]; then
        if [ "$errs" = "NA" ]; then
          status="no summary"
        elif [ "$ab" != "0" ]; then
          status="no results"
        else
          status="SKIPPED (0 tests ran)"
        fi
      elif [ "$ab" != "0" ]; then
        status="ABORTED after $tots tests"
      else
        case "$errs" in
            0) status="ok" ;;
            *) status="FAILED" ;;
        esac
      fi
      printf '%-30s %7s %7s %8s   %s\n' "$name" "$errs" "$tots" "$ab" "$status"
    done

total=${#MATCHES[@]}
passed=$(awk -F'\t' '$2==0 && $3+0>0 && $4+0==0' "$RESULTS" | wc -l | tr -d ' ')
# A file is "aborted" when it produced no summary, when it produced no results,
# or when it stopped part way through. The third case used to be counted only
# as a failure, which made the total line disagree with the per-file states.
aborted=$(awk -F'\t' '$4+0!=0 || $2=="NA" || ($3+0==0 && $2!="0")' "$RESULTS" | wc -l | tr -d ' ')
failed=$(awk -F'\t' '$2+0>0' "$RESULTS" | wc -l | tr -d ' ')
run_tests=$(awk -F'\t' '{s+=$3} END{print s+0}' "$RESULTS")
run_errs=$(awk -F'\t' '$2!="NA"{s+=$2} END{print s+0}' "$RESULTS")

echo
echo "========================================"
skipped=$(awk -F'	' '$2+0==0 && $3+0==0 && $4+0==0' "$RESULTS" | wc -l | tr -d ' ')
# The counts overlap on purpose and the line says so: a file that ran tests and
# then aborted is both "with test failures" and "aborted", because it failed
# some cases and never reached the rest. The four numbers are not a partition,
# and a reader who assumes they are will conclude the total is wrong.
echo "files:  $total total, $passed passed, $failed with test failures, $aborted aborted (incl. part-way), $skipped ran no tests"
echo "cases:  $run_tests tests run, $run_errs failed"
echo "artifacts under $RUN_ROOT"
# A file that ran no tests is not a pass, and neither is one that aborted part
# way through, so the exit status is non-zero unless every matched file ran all
# of its cases to the end and none failed.
[ "$failed" -eq 0 ] && [ "$aborted" -eq 0 ] && [ "$skipped" -eq 0 ]
exit $?

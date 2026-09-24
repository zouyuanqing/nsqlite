#!/usr/bin/env bash
# Runs a subset of the official SQLite TCL suite against nsqlite.
#
# The suite needs a Tcl interpreter that can drive the engine. Two ways to get
# one, both described in docs/testing.md:
#
#   (a) a Tcl extension exposing the C API, so test/tester.tcl runs unmodified
#   (b) a shim tester.tcl that drives a CLI binary via exec (the Turso approach)
#
# Neither exists yet. This script therefore has a job even with no engine: it
# resolves the glob to real files, checks the interpreter, and says plainly what
# is missing instead of dumping a Tcl backtrace.
#
# Usage:
#   tools/run_suite.sh 'select*'              # glob over the suite
#   tools/run_suite.sh 'select1.test'         # one file
#   tools/run_suite.sh --part 2/8 'select*'   # one shard, via TCLTEST_PART
#   tools/run_suite.sh --permutation full 'alter*'
#
# Environment:
#   TCLSH        tclsh to use          (default: tclsh on PATH, else ucrt64)
#   TESTER       tester.tcl override   (default: <suite>/test/tester.tcl)
#   TCLTEST_PART passed through if set in the environment
set -euo pipefail

SUITE_VERSION_DEFAULT="3.53.4"
PERMUTATION=""
PART=""
PATTERN=""

usage() {
    sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --part)          PART="${2:-}"; shift 2 ;;
        --part=*)        PART="${1#--part=}"; shift ;;
        --permutation)   PERMUTATION="${2:-}"; shift 2 ;;
        --permutation=*) PERMUTATION="${1#--permutation=}"; shift ;;
        -h|--help)       usage; exit 0 ;;
        --)              shift; break ;;
        -*)              echo "error: unknown option '$1' (try --help)" >&2; exit 2 ;;
        *)
            if [ -n "$PATTERN" ]; then
                echo "error: expected one glob pattern, got '$PATTERN' and '$1'" >&2
                exit 2
            fi
            PATTERN="$1"; shift ;;
    esac
done
if [ $# -gt 0 ] && [ -z "$PATTERN" ]; then PATTERN="$1"; shift; fi

[ -n "$PATTERN" ] || { echo "error: no glob pattern given (try --help)" >&2; exit 2; }

DIR="$(cd "$(dirname "$0")/.." && pwd)"
SUITE="$DIR/test/sqlite-suite"
TEST_DIR="$SUITE/test"
TESTER="${TESTER:-$TEST_DIR/tester.tcl}"

# --- 1. is the suite there? ------------------------------------------------
if [ ! -d "$TEST_DIR" ]; then
    cat >&2 <<EOF
error: the sqlite TCL suite is not present.

expected: $TEST_DIR
found:    nothing

Fetch it first:

    tools/fetch_sqlite_suite.sh

That downloads the pinned $SUITE_VERSION_DEFAULT suite from the
sqlite/sqlite GitHub mirror and verifies it against the fossil manifest.
EOF
    exit 1
fi
if [ ! -f "$TESTER" ]; then
    echo "error: harness not found: $TESTER" >&2
    if [ "$TESTER" != "$TEST_DIR/tester.tcl" ]; then
        echo "       TESTER is set to $TESTER" >&2
    fi
    echo "       re-run tools/fetch_sqlite_suite.sh --force" >&2
    exit 1
fi

# --- 2. locate tclsh -------------------------------------------------------
# Scratch dir for the small probe scripts below. Each probe must be a real FILE,
# not a here-doc: tclsh reads a piped script, but the mingw and ucrt64 builds
# do not report its failure in the exit status (both return 0 even when the
# script raised an error), so a here-doc probe would silently pass everything.
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
TMPVER="$SCRATCH/version.tcl"
printf 'puts [info patchlevel]\n' > "$TMPVER"

TCLSH="${TCLSH:-}"
if [ -z "$TCLSH" ]; then
    if command -v tclsh >/dev/null 2>&1; then
        TCLSH="$(command -v tclsh)"
    elif [ -x "C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/tclsh.exe" ]; then
        TCLSH="C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/tclsh.exe"
    else
        echo "error: tclsh not found on PATH" >&2
        echo "       set TCLSH=/path/to/tclsh, or install tcl 8.6+" >&2
        exit 1
    fi
fi
if ! "$TCLSH" "$TMPVER" > "$SCRATCH/ver.out" 2>&1; then
    echo "error: TCLSH=$TCLSH is not a working tclsh" >&2
    exit 1
fi
TCL_VERSION="$(tr -d '\r' < "$SCRATCH/ver.out")"

# --- 3. resolve the glob to concrete files ---------------------------------
# A bare name like "select1.test" is not a glob; a pattern like "select*" is.
MATCHES=()
if [ -f "$TEST_DIR/$PATTERN" ]; then
    MATCHES=("$PATTERN")
else
    for f in "$TEST_DIR"/*.test; do
        [ -e "$f" ] || continue
        b="$(basename "$f")"
        # shellcheck disable=SC2254 # $PATTERN is a glob on purpose
        case "$b" in $PATTERN) MATCHES+=("$b") ;; esac
    done
fi

if [ "${#MATCHES[@]}" -eq 0 ]; then
    echo "error: no test files match '$PATTERN'" >&2
    echo "       looked in $TEST_DIR for *.test" >&2
    echo "       $(ls "$TEST_DIR" | grep -c '\.test$') .test files are available there" >&2
    exit 1
fi

SUITE_VER="$(tr -d ' \t\n\r' < "$SUITE/VERSION" 2>/dev/null || echo unknown)"

# --- 4. report what is about to happen -------------------------------------
echo "nsqlite test runner"
echo "  suite      $SUITE_VER  ($TEST_DIR)"
echo "  pattern    $PATTERN  ->  ${#MATCHES[@]} file(s)"
echo "  tclsh      $TCLSH (Tcl $TCL_VERSION)"
echo "  harness    $TESTER"
echo "  scratch    $DIR/test/sqlite-run"
[ -n "$PERMUTATION" ] && echo "  permutation $PERMUTATION"
if [ -n "$PART" ]; then
    echo "  TCLTEST_PART $PART"
elif [ -n "${TCLTEST_PART:-}" ]; then
    echo "  TCLTEST_PART $TCLTEST_PART (from environment)"
fi
echo

# --- 5. preflight: does a usable interpreter exist? ------------------------
# The suite talks to the engine through Tcl, never through a subprocess.
#
# Checking only that `package require sqlite3` succeeds is not enough. A stock
# sqlite3 Tcl extension satisfies that and still fails at line 102 of
# tester.tcl, because the suite needs commands that only the testfixture build
# registers. So probe for those. These four are the first calls tester.tcl
# makes, and it fails on the first one that is missing.
PREFLIGHT="$SCRATCH/preflight.tcl"
cat > "$PREFLIGHT" <<'PREFLIGHT_EOF'
package require sqlite3
set needed {
    sqlite3_initialize
    sqlite3_test_control_pending_byte
    install_malloc_faultsim
    autoinstall_test_functions
}
set missing {}
foreach c $needed {
    if {[llength [info commands $c]] == 0} { lappend missing $c }
}
if {[llength $missing]} {
    puts stderr "MISSING: $missing"
    exit 1
}
PREFLIGHT_EOF

PREFLIGHT_OUT="$SCRATCH/preflight.out"
if ! "$TCLSH" "$PREFLIGHT" > "$PREFLIGHT_OUT" 2>&1; then
    MISSING="$(sed -n 's/^MISSING: //p' "$PREFLIGHT_OUT" | tr -d '\r' | head -1)"
    DETAIL="$(tr -d '\r' < "$PREFLIGHT_OUT" | grep -v '^MISSING:' | head -2 | tr '\n' ' ')"
    cat >&2 <<EOF
error: this tclsh cannot drive the sqlite TCL suite.

  tclsh:  $TCLSH (Tcl $TCL_VERSION)
  reason: ${MISSING:-the preflight probe did not complete}
  detail: $DETAIL

The suite never talks to the engine over a subprocess. Its harness calls
sqlite3_initialize, install_malloc_faultsim and autoinstall_test_functions
before the first test executes, so a stock sqlite3 Tcl extension is not enough
either: the suite needs the test-only commands that the SQLite "testfixture"
build adds. There is nothing for the harness to fall back on.

Two ways forward, both described in docs/testing.md:

  (a) Tcl extension exposing the C API, so the stock harness runs unmodified.
      Build it and put it on the package path:
          export TCLLIBPATH=/path/to/nsqlite-tcl
      It must also register the test-only commands above, or tester.tcl fails
      at line 102 no matter what else works.

  (b) tester.tcl shim driving a CLI binary over exec (the approach Turso takes).
      That sidesteps every command above, because the shim only shells out:
          TESTER=$DIR/tools/tester_shim.tcl tools/run_suite.sh '$PATTERN'

Until one of those exists, nsqlite cannot be measured against the official
suite. That is the expected state of this track, not a failure of this script.
EOF
    exit 3
fi

# --- 6. run ----------------------------------------------------------------
# Each file gets its own scratch directory. The suite deletes and reopens
# test.db constantly; sharing one directory across files makes a result depend
# on leftovers and makes a crash hard to attribute.
RUN_ROOT="$DIR/test/sqlite-run"
mkdir -p "$RUN_ROOT"

# The harness derives its own testdir from the path of the script being run, so
# these point at the directory holding the .test files, not at the scratch dir.
export TESTDIR="$TEST_DIR"
export SQLITE_TEST_DIR="$TEST_DIR"
[ -n "$PART" ] && export TCLTEST_PART="$PART"

overall=0
failed=()

for name in "${MATCHES[@]}"; do
    work="$RUN_ROOT/${name%.test}"
    rm -rf "$work"
    mkdir -p "$work"

    echo "=== $name ==="
    set +e
    if [ -n "$PERMUTATION" ]; then
        ( cd "$work" && "$TCLSH" "$TESTER" "$PERMUTATION" "$TEST_DIR/$name" )
    else
        ( cd "$work" && "$TCLSH" "$TESTER" "$TEST_DIR/$name" )
    fi
    rc=$?
    set -e

    if [ "$rc" -ne 0 ]; then
        echo "--- $name: FAILED (exit $rc)"
        failed+=("$name")
        overall=1
    else
        echo "--- $name: ok"
    fi
    echo
done

# --- 7. summary ------------------------------------------------------------
echo "========================================"
if [ "${#failed[@]}" -eq 0 ]; then
    echo "all ${#MATCHES[@]} file(s) passed"
else
    echo "${#failed[@]} of ${#MATCHES[@]} file(s) failed:"
    for f in "${failed[@]}"; do echo "  $f"; done
fi
echo "artifacts under $RUN_ROOT"
exit "$overall"

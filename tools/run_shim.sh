#!/usr/bin/env bash
# Runs .test files from the official SQLite suite against nsqlite, through the
# tester.tcl shim in test/shim.
#
# Each file gets its own scratch directory, because the suite deletes and reopens
# test.db constantly: sharing one would make a result depend on leftovers and
# make a crash hard to attribute.
#
# Usage:
#   tools/run_shim.sh 'select1.test'      # one file
#   tools/run_shim.sh 'select*'           # a glob over the suite
#   tools/run_shim.sh --permutation full 'alter*'
#
# Environment:
#   TCLSH      tclsh to use       (default: tclsh on PATH, else the ucrt64 build)
#   NSQLITED   the engine binary   (default: target/debug/nsqlited.exe)
#
# Exit status is 0 when every matched file passed, 1 otherwise.
###
set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
SUITE="$DIR/test/sqlite-suite/test"
SHIM="$DIR/test/shim/tester.tcl"
RUN_ROOT="$DIR/test/sqlite-run"
PERMUTATION=""

while [ $# -gt 0 ]; do
    case "$1" in
        --permutation)   PERMUTATION="${2:-}"; shift 2 ;;
        --permutation=*) PERMUTATION="${1#--permutation=}"; shift ;;
        -h|--help)       sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)              echo "error: unknown option '$1' (try --help)" >&2; exit 2 ;;
        *)               PATTERN="$1"; shift ;;
    esac
done

[ -n "${PATTERN:-}" ] || { echo "error: no glob pattern given (try --help)" >&2; exit 2; }
[ -f "$SHIM" ] || { echo "error: harness not found: $SHIM" >&2; exit 1; }
[ -d "$SUITE" ] || { echo "error: suite not fetched: $SUITE" >&2; exit 1; }

# Each probe must be a real FILE, not a here-doc: the mingw and ucrt64 tclsh
# builds do not report a piped script's failure in the exit status, so a
# here-doc probe would silently pass everything.
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
printf 'puts [info patchlevel]\n' > "$SCRATCH/ver.tcl"

# Which tclsh. The ucrt64 build is preferred over a bare `tclsh` on PATH,
# because on this machine PATH resolves to the mingw64 build (8.6.17) while the
# project pins ucrt64 (8.6.18), and a suite result that depends on which of two
# Tcl versions the shell happened to find is not a result. TCLSH overrides.
TCLSH="${TCLSH:-}"
if [ -z "$TCLSH" ]; then
    PINNED="C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/tclsh.exe"
    if [ -x "$PINNED" ]; then
        TCLSH="$PINNED"
    elif command -v tclsh >/dev/null 2>&1; then
        TCLSH="$(command -v tclsh)"
    else
        echo "error: no tclsh found; set TCLSH" >&2
        exit 1
    fi
fi
if ! "$TCLSH" "$SCRATCH/ver.tcl" > "$SCRATCH/ver.out" 2>&1; then
    echo "error: TCLSH=$TCLSH is not a working tclsh" >&2
    exit 1
fi

NSQLITED="${NSQLITED:-$DIR/target/debug/nsqlited.exe}"
if [ ! -x "$NSQLITED" ]; then
    echo "error: engine not built: $NSQLITED" >&2
    echo "       run 'cargo build --workspace' first" >&2
    exit 1
fi

# The shim drives the engine through `--testsuite`, and a binary built before
# that mode existed exits 0 without writing a record stream. Every statement
# then raises "the engine produced no record stream", the test file aborts, and
# the run reports zero tests -- which reads as a clean run of a file that has
# hundreds of cases. So the engine is asked for one row and the answer is
# checked: a `C` record with a value means the protocol is there. A binary
# without `--testsuite` cannot answer, and the run stops here instead of
# reporting a vacuous pass.
printf 'SELECT 1;\n' > "$SCRATCH/probe.sql"
"$NSQLITED" --testsuite "$SCRATCH/probe.db" < "$SCRATCH/probe.sql" \
    > "$SCRATCH/probe.out" 2>&1
if ! grep -qE '^C 1 ' "$SCRATCH/probe.out" || ! grep -qE '^R ' "$SCRATCH/probe.out"; then
    echo "error: $NSQLITED does not speak --testsuite:" >&2
    echo "       $(head -1 "$SCRATCH/probe.out" | tr -d '\r')" >&2
    echo "       run 'cargo build -p nsqlited' first" >&2
    exit 1
fi
# An engine that cannot report its own version is not usable either.
if ! "$NSQLITED" --version > /dev/null 2>&1; then
    echo "error: $NSQLITED does not run: '$(head -1 "$SCRATCH/probe.out" | tr -d '\r')'" >&2
    echo "       run 'cargo build -p nsqlited' first" >&2
    exit 1
fi
export NSQLITED

# A bare name like "select1.test" is not a glob; a pattern like "select*" is.
MATCHES=()
if [ -f "$SUITE/$PATTERN" ]; then
    MATCHES=("$PATTERN")
else
    for f in "$SUITE"/*.test; do
        [ -e "$f" ] || continue
        b="$(basename "$f")"
        # shellcheck disable=SC2254 # $PATTERN is a glob on purpose
        case "$b" in $PATTERN) MATCHES+=("$b") ;; esac
    done
fi
if [ "${#MATCHES[@]}" -eq 0 ]; then
    echo "error: no test files match '$PATTERN' in $SUITE" >&2
    exit 1
fi

echo "nsqlite test runner (shim)"
echo "  suite      $SUITE"
echo "  pattern    $PATTERN  ->  ${#MATCHES[@]} file(s)"
echo "  tclsh      $TCLSH (Tcl $(tr -d '\r' < "$SCRATCH/ver.out"))"
echo "  engine     $NSQLITED"
[ -n "$PERMUTATION" ] && echo "  permutation $PERMUTATION"
echo

# The .test file locates its harness from [file dirname $argv0]; the shim makes
# that resolve to itself. TESTDIR is still exported because the harness and
# several .test files read it.
export TESTDIR="$SUITE"
export SQLITE_TEST_DIR="$SUITE"

mkdir -p "$RUN_ROOT"
overall=0
failed=()

for name in "${MATCHES[@]}"; do
    work="$RUN_ROOT/${name%.test}"
    rm -rf "$work"
    mkdir -p "$work"
    log="$work/output.txt"

    echo "=== $name ==="
    ( cd "$work" && "$TCLSH" "$SHIM" "$PERMUTATION" "$SUITE/$name" ) > "$log" 2>&1
    rc=$?

    # The shim's own summary is the one to read: "N errors out of M tests".
    summary="$(grep -E '^[0-9]+ errors? out of [0-9]+ tests' "$log" | tail -1)"
    if [ -z "$summary" ]; then
        summary="did not reach the summary"
    fi
    echo "  $summary"

    if [ "$rc" -ne 0 ]; then
        echo "--- $name: FAILED (exit $rc), log at $log"
        failed+=("$name")
        overall=1
    else
        echo "--- $name: ok, log at $log"
    fi
done

echo
echo "========================================"
if [ "${#failed[@]}" -eq 0 ]; then
    echo "all ${#MATCHES[@]} file(s) passed"
else
    echo "${#failed[@]} of ${#MATCHES[@]} file(s) failed:"
    for f in "${failed[@]}"; do echo "  $f"; done
fi
echo "artifacts under $RUN_ROOT"
exit "$overall"

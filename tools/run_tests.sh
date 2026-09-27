#!/usr/bin/env bash
# Runs the whole workspace test suite, choosing the shell the differential
# tester needs.
#
# The differential tests drive a POSIX shell script. On Windows the plain `bash`
# on PATH is the WSL launcher, which cannot see a `D:\` path and exits 127
# before the script starts, so the tester probes for a shell that can and falls
# back to the bare name. Pointing it at Git Bash directly skips the probe and is
# the reliable choice on a machine that has it.
#
# The differential tests are slow: each case runs a statement through both this
# engine and the real sqlite3, and a full pass is tens of minutes. Everything
# else runs in about a minute.
#
#   tools/run_tests.sh              # everything
#   tools/run_tests.sh --fast       # everything except the differential tests
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FAST=0
for arg in "$@"; do
  case "$arg" in
    --fast) FAST=1 ;;
    *) echo "usage: $0 [--fast]" >&2; exit 2 ;;
  esac
done

# Git Bash, if it is installed. The differential tester accepts a path here and
# uses it instead of probing.
if [ -z "${DIFFTEST_BASH:-}" ]; then
  for candidate in \
    "/c/Program Files/Git/bin/bash.exe" \
    "C:/Program Files/Git/bin/bash.exe"
  do
    if [ -x "$candidate" ]; then
      export DIFFTEST_BASH="$candidate"
      break
    fi
  done
fi

# The differential tests, when they are wanted.
DIFF_TESTS=()
if [ "$FAST" -eq 0 ]; then
  for t in differential differential2 differential3 differential4; do
    [ -f "crates/nsqlite/tests/$t.rs" ] && DIFF_TESTS+=(--test "$t")
  done
fi

status=0

echo "== unit and integration tests =="
if ! cargo test --workspace "${DIFF_TESTS[@]}"; then
  status=1
fi

if [ "$FAST" -eq 1 ]; then
  echo
  echo "differential tests skipped (--fast)"
fi

exit $status

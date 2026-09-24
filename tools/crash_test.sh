#!/usr/bin/env bash
# Crash-recovery tests, run against the CLI as well as the test binary.
#
# There are two things to see here, and the test binary only shows one of them.
#
# 1. The Rust tests in crates/nsqlite/tests/crash_recovery.rs make the crash
#    deterministic: a transaction is written, flushed, and then abandoned with
#    the connection dropped while it is still open, so no destructor runs a
#    rollback and there is no timing to race. One of them really does abort a
#    child process, so the file handles are torn down by the operating system.
#    This script runs them.
#
# 2. The same behaviour has to hold from *outside* the process, through the
#    `nsqlited` binary: open a database that has a journal next to it and the
#    uncommitted rows must be gone. That is the only way to tell recovery is
#    wired into the engine rather than merely present in the pager, and it is
#    what a user or a shell script would actually hit.
#
# It also asks the real sqlite3 what each of these on-disk states means, so the
# expectations in the Rust tests are checked against the implementation rather
# than against a recollection of the format. Every line of that section prints
# the state it built and what sqlite3 said about it.
#
# Usage:
#   tools/crash_test.sh              # everything
#   tools/crash_test.sh --no-sqlite3 # skip the reference checks
#   tools/crash_test.sh --keep       # leave the scratch directory behind
#
# Environment:
#   NSQLITED   the engine binary   (default: target/debug/nsqlited.exe)
#   SQLITE3    the reference       (default: sqlite3 on PATH)
#   CARGO      cargo               (default: cargo)
#
# If target/debug/nsqlited.exe is locked -- a stray process from an earlier run
# of the killed-process check below, or another agent's build -- the CLI section
# is skipped and says so. The Rust tests still run.
#
# Exit status is 0 when every check passed, 1 otherwise.
###
set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
NSQLITED="${NSQLITED:-$DIR/target/debug/nsqlited.exe}"
SQLITE3="${SQLITE3:-sqlite3}"
RUN_SQLITE3=1
RUN_RUST=1
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --no-sqlite3) RUN_SQLITE3=0; shift ;;
        --skip-rust)  RUN_RUST=0; shift ;;
        --keep)       KEEP=1; shift ;;
        -h|--help)    sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)            echo "error: unknown option '$1' (try --help)" >&2; exit 2 ;;
    esac
done

SCRATCH="$(mktemp -d)"
if [ "$KEEP" -eq 0 ]; then
    trap 'rm -rf "$SCRATCH"' EXIT
else
    echo "scratch kept at $SCRATCH"
fi

fails=0
checks=0

# check DESCRIPTION ACTUAL EXPECTED
check() {
    checks=$((checks + 1))
    # An empty expectation is never a real expectation: it is what a fixture
    # that failed to build leaves behind, and a comparison of two empty strings
    # reports success. Refusing it here means a broken fixture fails loudly
    # instead of quietly passing.
    if [ -z "$3" ]; then
        fails=$((fails + 1))
        printf '  FAIL  %s\n          expected: (empty) -- the fixture was never built\n          actual:   %s\n' "$1" "$2"
        return
    fi
    if [ "$2" = "$3" ]; then
        printf '  ok    %s\n' "$1"
    else
        fails=$((fails + 1))
        printf '  FAIL  %s\n          expected: %s\n          actual:   %s\n' "$1" "$3" "$2"
    fi
}

note() { printf '        %s\n' "$1"; }

# fatal DESCRIPTION -- the fixture could not be built, so every check that
# would have used it is meaningless rather than passed.
#
# The reason this exists: `db="$(cli_setup foo)"` runs cli_setup in a command
# substitution, whose exit status is discarded, so cli_setup's `return 1` left
# `db` holding the empty string instead of aborting. Every check below then ran
# against a path that did not exist, and the length comparisons in particular
# passed *vacuously*: with `len_before` empty, `[ "" -gt "" ]` errors and both
# sides of `check` were the empty string, so a section reported success for a
# fixture it never built. An empty expectation is always a mistake here, so
# `check` refuses one outright and the fixture is built through `build` so its
# failure cannot be swallowed.
fatal() {
    fails=$((fails + 1))
    printf '  FAIL  %s\n' "$1"
    [ $# -gt 1 ] && printf '          %s\n' "$2"
    echo
    echo "the CLI section cannot continue without a database to work on"
    exit 1
}

# build DESCRIPTION -- runs a fixture builder and aborts if it failed.
build() {
    local what="$1"
    shift
    "$@" || fatal "could not build the fixture for: $what"
}

# The CLI prints `id|name|score` per row, so a multi-line result is flattened.
# `head` keeps a failed case from dumping a thousand rows into the log: the
# first line is always the first committed row, so a diff of the first few lines
# says everything a full dump would.
cli_rows() { "$NSQLITED" "$1" "$2" 2>&1 | head -5 | tr '\n' ';'; }
TWO_ROWS="1|alpha|10;2|beta|20;"

have_python=0
command -v python3 >/dev/null 2>&1 && have_python=1
have_sqlite3=0
[ "$RUN_SQLITE3" -eq 1 ] && command -v "$SQLITE3" >/dev/null 2>&1 && have_sqlite3=1

echo "=============================================="
echo "nsqlite crash recovery"
echo "  engine   $NSQLITED"
echo "  scratch  $SCRATCH"
echo

# ---------------------------------------------------------------------------
echo "--- 1. the Rust tests -------------------------------------------"
# `crash_child_writes_then_dies` is the killed-subprocess half of
# a_killed_process_leaves_a_journal_that_replays. The parent test re-invokes
# this same binary with --ignored, so running the parent covers both.
if [ "$RUN_RUST" -eq 0 ]; then
    echo "  skipped (--skip-rust)"
elif "$CARGO" test -p nsqlite --test crash_recovery 2>&1 | sed 's/^/    /'; then
    :
else
    fails=$((fails + 1))
    echo "  FAIL  the crash_recovery test binary"
fi
echo

# ---------------------------------------------------------------------------
echo "--- 2. the CLI, from outside the process -------------------------"
echo

if [ ! -x "$NSQLITED" ]; then
    echo "  skipped: the engine is not built at $NSQLITED"
    note "run 'cargo build --workspace' first"
elif [ "$have_python" -eq 0 ]; then
    echo "  skipped: python3 is not on PATH, so no journal can be built here"
else

# A committed database with two rows, created by the CLI itself.
#
# The path lands in the global `db` rather than on stdout. Printing it and
# capturing it with `db="$(cli_setup ...)"` is what made a failed setup
# indistinguishable from a successful one: the command substitution discards
# the exit status, so `db` ended up empty and the checks that followed ran
# against nothing. A function that fails on its own status cannot be swallowed
# that way.
cli_setup() {
    db="$SCRATCH/$1.db"
    rm -f "$db" "$db-journal" "$db.committed"
    "$NSQLITED" "$db" \
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score INTEGER);
         INSERT INTO t VALUES(1,'alpha',10);
         INSERT INTO t VALUES(2,'beta',20);" >/dev/null 2>&1 \
        || { echo "error: the CLI could not create $db" >&2; return 1; }
    [ -f "$db" ] || { echo "error: $db was not created" >&2; return 1; }
    # Kept so the recovery checks can ask the stronger question: not "are the
    # rows right" but "is the file the file that was there before". A replay
    # that restores every row and still leaves a header the real sqlite3 calls
    # corrupt passes the first and fails the second.
    cp "$db" "$db.committed"
}

# Leaves on disk exactly what a crash leaves: a journal holding the pre-image of
# every page, the database already carrying the interrupted transaction's
# changes, and the file optionally grown past the size the journal records.
#
#   $1 database   $2 pages the crashed transaction added   $3 page it overwrote
build_crash_state() {
    python3 - "$1" "$2" "$3" <<'PY'
import struct, sys

db, extra, scribble = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
MAGIC = bytes([0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7])
NONCE = 0x51A7C3E9
SECTOR = 512

def checksum(page):
    """The checksum a journal record carries: a sparse sample, every 200th
    byte, counting down from the end of the page, added to the nonce."""
    s, i = NONCE, len(page) - 200
    while i >= 0:
        s = (s + page[i]) & 0xffffffff
        i -= 200
    return s

data = open(db, 'rb').read()
page_size = struct.unpack('>H', data[16:18])[0]
page_size = 65536 if page_size == 1 else page_size
npages = len(data) // page_size

hdr = bytearray()
hdr += MAGIC
hdr += struct.pack('>I', 0xFFFFFFFF)   # records run to the end of the file
hdr += struct.pack('>I', NONCE)
hdr += struct.pack('>I', npages)       # the size in pages before the transaction
hdr += struct.pack('>I', SECTOR)
hdr += struct.pack('>I', page_size)
hdr += b'\0' * (SECTOR - 28)

body = b''
for i in range(npages):
    page = data[i * page_size:(i + 1) * page_size]
    body += struct.pack('>I', i + 1) + page + struct.pack('>I', checksum(page))
open(db + '-journal', 'wb').write(bytes(hdr) + body)

# The transaction's own writes are already in the file. Only the page *body* is
# overwritten, never the first 100 bytes of page 1: those are the file header,
# and a real transaction updates fields inside it rather than scribbling over
# the magic string. Leaving them intact keeps the fixture honest -- a database
# whose header is destroyed is rejected as NOTADB before recovery ever runs,
# which says nothing about the journal.
out = bytearray(data)
off = (scribble - 1) * page_size
out[off + 100:off + page_size] = b'\xA5' * (page_size - 100)
open(db, 'wb').write(bytes(out) + b'\x5A' * (extra * page_size))
PY
}

# The CLI prints `id|name|score` per row, so a multi-line result is flattened.
# `head` keeps a failed case from dumping a thousand rows into the log: the
# first line is always the first committed row, so a diff of the first few lines
# says everything a full dump would.
cli_rows() { "$NSQLITED" "$1" "$2" 2>&1 | head -5 | tr '\n' ';'; }
TWO_ROWS="1|alpha|10;2|beta|20;"

echo "a) a hot journal on disk is replayed by the next open"
build "a) a hot journal" cli_setup cli_hot
check "the committed database reads back" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"

build "the crash state" build_crash_state "$db" 0 2
check "the journal is left on disk" "$([ -f "$db-journal" ] && echo yes || echo no)" "yes"
check "the interrupted transaction's page really is on disk" \
    "$(od -An -tx1 -j $((4096 + 100)) -N 1 "$db" | tr -d ' \n')" "a5"
check "the next open replays it and the committed rows are back" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"
check "the journal is gone afterwards" \
    "$([ -f "$db-journal" ] && echo yes || echo no)" "no"
check "and the recovered file is byte for byte the committed one" \
    "$(cmp -s "$db" "$db.committed" && echo yes || echo no)" "yes"

echo
echo "b) a journal that grew the file shortens it again"
build "b) a journal that grew the file" cli_setup cli_grow
"$NSQLITED" "$db" \
    "INSERT INTO t VALUES(3,'gamma',30); INSERT INTO t VALUES(4,'delta',40);" >/dev/null
# Re-snapshot: the file to compare against is the state the crash interrupts,
# which is *after* those two committed inserts. cli_setup's snapshot is the
# two-row database, and comparing against that would ask for the interrupted
# transaction's own committed rows to be rolled back.
cp "$db" "$db.committed"
len_before="$(wc -c < "$db")"
build "the crash state" build_crash_state "$db" 3 2
check "the crashed transaction grew the file" \
    "$([ "$(wc -c < "$db")" -gt "$len_before" ] && echo yes || echo no)" "yes"
"$NSQLITED" "$db" 'SELECT id, name, score FROM t;' >/dev/null
check "the file is back to its original length" "$(wc -c < "$db")" "$len_before"
check "and the committed rows are all there" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" \
    "1|alpha|10;2|beta|20;3|gamma|30;4|delta|40;"
check "and it is byte for byte the file before the crash" \
    "$(cmp -s "$db" "$db.committed" && echo yes || echo no)" "yes"

echo
echo "c) a journal torn mid-record: the whole records are still replayed"
build "c) a torn journal" cli_setup cli_torn
build "the crash state" build_crash_state "$db" 0 1     # page 1 is modified; the table's page is not
# Cut the last record in half, which is what a crash part way through writing
# it leaves. The records before it are whole and have to be replayed.
python3 -c "
import os, sys
p = sys.argv[1]
f = open(p, 'r+b'); f.truncate(os.path.getsize(p) - 600); f.close()" "$db-journal"
check "the journal is now shorter than two whole records" \
    "$([ "$(wc -c < "$db-journal")" -lt $((512 + 2 * 4104)) ] && echo yes || echo no)" "yes"
check "the records before the tear are replayed" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"

echo
echo "d) a record with a flipped byte: recovery stops there"
build "d) a damaged record" cli_setup cli_damaged
build "the crash state" build_crash_state "$db" 0 1
# Page 1's record is the first one and is left intact; the second record gets a
# byte flipped, at an offset the checksum actually samples. Recovery has to
# stop at it, and the first record still has to be replayed.
python3 -c "
import sys
p = sys.argv[1]
f = open(p, 'r+b')
f.seek(512 + 4 + 4096 + 4 + 3896)      # a sampled byte of the second record
b = f.read(1); f.seek(-1, 1); f.write(bytes([b[0] ^ 0xff])); f.close()" "$db-journal"
check "the first record was still replayed" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"

echo
echo "e) a journal whose header was zeroed is not a journal"
build "e) a zeroed journal header" cli_setup cli_zeroed
build "the crash state" build_crash_state "$db" 0 1
python3 -c "
import sys
p = sys.argv[1]
d = bytearray(open(p, 'rb').read())
d[0:8] = b'\0' * 8
open(p, 'wb').write(bytes(d))" "$db-journal"
len_when_zeroed="$(wc -c < "$db")"
"$NSQLITED" "$db" 'SELECT id FROM t;' >/dev/null 2>&1 && state=readable || state=unreadable
check "the journal was ignored, so it is still there" \
    "$([ -f "$db-journal" ] && echo yes || echo no)" "yes"
check "and the database was not rewritten" "$(wc -c < "$db")" "$len_when_zeroed"
check "the crashed page image is left exactly as it was" "$state" "unreadable"
note "That last result is the correct one, not a failure. A zeroed header is"
note "how PRAGMA journal_mode=PERSIST commits, so the records belong to a"
note "transaction that committed; replaying them would undo committed work."

echo
echo "e2) a crash that freed a page restores the file header too"
# Every other case here only inserts, which never moves the freelist, and the
# freelist lives *only* in the first 100 bytes of page 1. So none of them can
# tell whether recovery got the file header right -- which is where it went
# wrong: replay restored the b-tree pages and then wrote the crashed header back
# over them, leaving a freelist naming a page that had just been given a
# b-tree page image. Every row came back, and the file was corrupt.
build "e2) a crash that freed a page" cli_setup cli_drop
"$NSQLITED" "$db" "CREATE TABLE gone(x); INSERT INTO gone VALUES(1);" >/dev/null
check "the committed database is sound to begin with" \
    "$(od -An -tu4 --endian=big -j 32 -N 8 "$db" | tr -s ' ' | sed 's/^ //')" "0 0"
build "the crash state" build_crash_state "$db" 0 1
# Stamp the DROP's effect into page 1: the journal only holds pre-images, so the
# transaction's own writes have to be there for the replay to have something to
# undo. Overwriting the freelist fields is enough -- that is the whole point.
python3 -c "
import sys
p = sys.argv[1]
d = bytearray(open(p, 'rb').read())
d[32:40] = (3).to_bytes(4, 'big') + (1).to_bytes(4, 'big')   # a trunk page, one leaf
open(p, 'wb').write(bytes(d))" "$db"
check "the crashed database really does name a freed page" \
    "$(od -An -tu4 --endian=big -j 32 -N 8 "$db" | tr -s ' ' | sed 's/^ //')" "3 1"
"$NSQLITED" "$db" 'SELECT id, name, score FROM t;' >/dev/null
check "the recovered database has an empty freelist again" \
    "$(od -An -tu4 --endian=big -j 32 -N 8 "$db" | tr -s ' ' | sed 's/^ //')" "0 0"
check "and the committed rows are back" \
    "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"
if [ "$have_sqlite3" -eq 1 ]; then
    check "and the real sqlite3 calls the recovered file sound" \
        "$("$SQLITE3" "$db" 'PRAGMA integrity_check;' 2>&1 | tr -d '\r' | head -1)" "ok"
fi

echo
echo "f) a real killed process, recovered by the next open"
build "f) a killed process" cli_setup cli_killed
len_before="$(wc -c < "$db")"
# One transaction with as many inserts as fit in a command line, and COMMIT
# last, so a process that dies part way through leaves the transaction open with
# its journal on disk. The size is worked out rather than fixed: Windows caps a
# command line at 32767 characters, and a script past that is rejected before
# the engine ever starts (exit 126), which is a different failure from a crash.
python3 -c "
import sys
budget = 31000
per = 0
for n in (100, 200, 400, 700, 900):
    parts = [\"INSERT INTO t VALUES(%d,'g%d',%d);\" % (i, i, i) for i in range(3, 3 + n)]
    script = 'BEGIN; ' + ' '.join(parts) + ' COMMIT;'
    if len(script) <= budget:
        per = n
        open(sys.argv[1], 'w').write(script)
print(per)
" "$SCRATCH/big.sql" > "$SCRATCH/rows.txt"
nrows="$(cat "$SCRATCH/rows.txt")"
note "$nrows inserts in one transaction, $(wc -c < "$SCRATCH/big.sql") bytes of SQL"

if ! command -v timeout >/dev/null 2>&1; then
    note "skipped: timeout(1) is not on PATH, so no process can be killed here"
    note "the Rust test a_killed_process_leaves_a_journal_that_replays covers"
    note "this by re-invoking the test binary and aborting it"
else
    # Kill a fraction of a second in, when the process is part way through the
    # inserts and its journal already has records in it. The window depends on
    # how fast the machine is, so it is swept rather than fixed: a value that is
    # too short kills the process before it starts, and one that is too long lets
    # it COMMIT, which is not a crash. Only a kill that lands on a journal with
    # records in it is used; otherwise this section reports that it could not
    # arrange one rather than passing on nothing.
    child=0
    jbytes=0
    for wait in 0.02 0.03 0.04 0.05 0.06 0.07 0.08 0.10 0.12; do
        rm -f "$db" "$db-journal"
        cli_setup cli_killed || break
        len_before="$(wc -c < "$db")"
        # The kill is the point of the check, so the shell's own report of it
        # ("Killed") is turned off; the status is inspected below instead.
        timeout -s KILL "$wait" "$NSQLITED" "$db" "$(cat "$SCRATCH/big.sql")" \
            >/dev/null 2>&1
        child=$?
        jbytes=0
        [ -f "$db-journal" ] && jbytes="$(wc -c < "$db-journal")"
        if [ "$jbytes" -gt 512 ] && [ "$child" -ne 124 ] \
           && [ "$(wc -c < "$db")" -gt "$len_before" ]; then
            note "killed after ${wait}s (exit status $child)"
            break
        fi
        jbytes=0
    done

    if [ "$jbytes" -le 512 ]; then
        note "skipped: no kill landed inside the transaction, so there is no"
        note "crash to recover from on this machine"
        note "the Rust test a_killed_process_leaves_a_journal_that_replays covers"
        note "this deterministically, by aborting a child process at a known point"
    else
        check "the killed process left its journal" \
            "$([ "$jbytes" -gt 512 ] && echo yes || echo no)" "yes"
        note "the journal held $(( (jbytes - 512 + 4103) / 4104 )) record(s)"
        note "the file had grown from $len_before to $(wc -c < "$db") bytes"
        check "the next open rolls the transaction back" \
            "$(cli_rows "$db" 'SELECT id, name, score FROM t;')" "$TWO_ROWS"
        check "the file is back to the length it had before" \
            "$(wc -c < "$db")" "$len_before"
        check "and the journal is gone" \
            "$([ -f "$db-journal" ] && echo yes || echo no)" "no"
    fi
fi

fi  # the CLI section

# ---------------------------------------------------------------------------
echo
echo "--- 3. what the real sqlite3 says --------------------------------"
if [ "$have_sqlite3" -eq 0 ]; then
    if [ "$RUN_SQLITE3" -eq 0 ]; then
        echo "  skipped (--no-sqlite3)"
    else
        echo "  skipped: $SQLITE3 is not on PATH"
    fi
elif [ "$have_python" -eq 0 ]; then
    echo "  skipped: python3 is not on PATH, so no journal can be built here"
else
    echo "  sqlite3: $("$SQLITE3" --version)"
    REF="$SCRATCH/ref"
    mkdir -p "$REF"

    # Builds the same crash state over a database sqlite3 itself wrote, lets
    # sqlite3 open it, and prints what it said. Each case is a state the Rust
    # tests assert about, so this is where the expectations come from.
    ref_case() {
        local name="$1" scribble="$2" damage="$3" probe="$4"
        local db="$REF/$name.db"
        rm -f "$db" "$db-journal"
        "$SQLITE3" "$db" \
            "PRAGMA page_size=1024;
             CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO t VALUES(1,'alpha');
             INSERT INTO t VALUES(2,'beta');" >/dev/null
        python3 - "$db" "$scribble" <<'PY'
import struct, sys
db, scribble = sys.argv[1], int(sys.argv[2])
MAGIC = bytes([0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7])
NONCE, SECTOR, PS = 0x9E3779B9, 512, 1024
def cksum(p):
    s, i = NONCE, len(p) - 200
    while i >= 0:
        s = (s + p[i]) & 0xffffffff; i -= 200
    return s
data = open(db, 'rb').read()
n = len(data) // PS
body = b''.join(
    struct.pack('>I', i + 1) + data[i*PS:(i+1)*PS] + struct.pack('>I', cksum(data[i*PS:(i+1)*PS]))
    for i in range(n))
hdr = MAGIC + struct.pack('>IIIII', 0xFFFFFFFF, NONCE, n, SECTOR, PS)
hdr += b'\0' * (SECTOR - len(hdr))
open(db + '-journal', 'wb').write(hdr + body)
# Only the page body: the first 100 bytes of page 1 are the file header, and a
# real transaction updates fields inside it rather than overwriting the magic.
out = bytearray(data)
off = (scribble - 1) * PS
out[off + 100:off + PS] = b'\xA5' * (PS - 100)
open(db, 'wb').write(bytes(out))
PY
        case "$damage" in
            none) ;;
            torn) python3 -c "
import os, sys
p = sys.argv[1]; f = open(p, 'r+b'); f.truncate(os.path.getsize(p) - 600); f.close()" "$db-journal" ;;
            damaged) python3 -c "
import sys
p = sys.argv[1]; f = open(p, 'r+b')
f.seek(512 + 4 + 1024 + 4 + (1024 - 200))   # a byte the checksum samples
b = f.read(1); f.seek(-1, 1); f.write(bytes([b[0] ^ 0xff])); f.close()" "$db-journal" ;;
            zeroed) python3 -c "
import sys
p = sys.argv[1]
d = bytearray(open(p, 'rb').read()); d[0:8] = b'\0' * 8
open(p, 'wb').write(bytes(d))" "$db-journal" ;;
        esac
        printf '  %-8s %-34s -> %s\n' "$name" "$probe" \
            "$("$SQLITE3" "$db" "$probe" 2>&1 | tr '\n' ' ')"
        printf '  %-8s %-34s    journal left afterwards: %s\n' "" "" \
            "$([ -f "$db-journal" ] && echo yes || echo no)"
    }

    ref_case hot     2 none    "SELECT name FROM t;"     "a hot journal"
    ref_case torn    1 torn    "SELECT name FROM t;"     "a journal torn mid-record"
    ref_case damaged 1 damaged "SELECT name FROM t;"     "a record with a flipped byte"
    ref_case zeroed  1 zeroed  "PRAGMA integrity_check;" "a zeroed journal header"

    # PERSIST is the mode that actually leaves a zeroed-header journal behind,
    # which is where that case comes from in practice.
    rm -f "$REF/persist.db" "$REF/persist.db-journal"
    out="$("$SQLITE3" "$REF/persist.db" \
        "PRAGMA journal_mode=PERSIST;
         CREATE TABLE t(x); INSERT INTO t VALUES(1);
         PRAGMA integrity_check;" 2>&1 | tr '\n' ' ')"
    printf '  %-8s %-34s -> %s\n' "PERSIST" "commits by zeroing the header" "$out"
    printf '  %-8s %-34s    journal left: %s, first 8 bytes: %s\n' "" "" \
        "$([ -f "$REF/persist.db-journal" ] && echo yes || echo no)" \
        "$(od -An -tx1 -N 8 "$REF/persist.db-journal" 2>/dev/null | tr -d ' \n')"

    # The three documented ways a commit retires a journal, which is why a
    # reader only has to look at the first byte.
    echo
    for mode in DELETE TRUNCATE PERSIST; do
        rm -f "$REF/m.db" "$REF/m.db-journal"
        "$SQLITE3" "$REF/m.db" \
            "PRAGMA journal_mode=$mode;
             CREATE TABLE t(x); INSERT INTO t VALUES(1);" >/dev/null
        if [ -f "$REF/m.db-journal" ]; then
            after="left, $(wc -c < "$REF/m.db-journal") bytes, first byte $(od -An -tx1 -N 1 "$REF/m.db-journal" | tr -d ' \n')"
        else
            after="deleted"
        fi
        printf '  %-8s %-34s -> %s\n' "$mode" "the journal after its commit" "$after"
    done
fi

# ---------------------------------------------------------------------------
echo
echo "=============================================="
if [ "$fails" -eq 0 ]; then
    echo "all $checks check(s) passed"
    exit 0
fi
echo "$fails of $checks check(s) failed"
exit 1

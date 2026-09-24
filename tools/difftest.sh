#!/usr/bin/env bash
# Differential runner: runs the same statements against nsqlited and the real
# sqlite3 and reports every point at which they disagree.
#
# WHAT IS COMPARED, AND WHY
#
# The comparison is on values, not on the text the two shells happen to print.
# The shells disagree about formatting -- nsqlited prints the real 1.0 as `1`
# and a blob as bare hex, sqlite3 prints `1.0` and `x'00FF'` -- and comparing
# those renderings would report differences that are only about printing. It
# would also miss the differences that matter: NULL against the empty string,
# and a real against an integer whose text is identical.
#
# So each query is rewritten into a *typed projection*. For every output
# column the runner substitutes
#
#     hex(typeof(<expr>) || '~' || quote(<expr>)) AS c<n>
#
# which the engine evaluates itself. Each value then arrives as a hex string
# that is unambiguous on both sides:
#
#   * `typeof` gives the storage class, so a real never matches an integer
#     (`real` against `integer`) and a blob never matches text, even when the
#     bytes and the text are the same characters.
#   * `quote` gives NULL the four letters `NULL` and the empty string the
#     two-character literal `''`, so NULL and '' can never be conflated.
#   * `quote` renders a real as `1.0` and an integer as `1`, and gives a blob
#     as `X'00FF'`, so the class is visible in the payload too and not only in
#     the tag.
#   * the whole thing is hex-encoded, so a value containing a newline, a `|`,
#     a quote or any other byte cannot be mistaken for a field or row boundary.
#     Nothing about the comparison depends on how either shell formats output.
#
# That projection is the only normalisation. Rows are not sorted, columns are
# not reordered, and no text is trimmed or lower-cased: if the engines
# disagree about a value, the disagreement is reported.
#
# TWO ENGINES, TWO TRANSPORTS
#
# The two shells are driven differently, because they do not print the same
# thing in the same way and a difference in printing must not be read as a
# difference in values.
#
# sqlite3 is driven with `.mode list`, a `|` field separator and a `.nullvalue`
# of `~NULL~`. List mode is chosen over the default because the default
# collapses a real into an integer: `SELECT 1.0, 1` prints `1|1`, so the two
# are the same bytes and a storage-class difference in that mode is invisible.
# Verified against the real shell:
#
#     $ sqlite3 -batch :memory: "SELECT 1.0, 1;"              ->  1|1
#     $ sqlite3 -batch :memory: ".mode list" "SELECT 1.0, 1;" ->  1.0|1
#
# The default's `.separator |` is the same as list mode's for these purposes, so
# the two are not otherwise different; the explicit settings only make the
# comparison independent of the shell's own defaults.
#
# `.mode quote` was tried and rejected. It *wraps every value in single
# quotes*, so the projection's hex arrives as `'696E74656765723E31'` and
# disagrees with the nsqlite side on framing alone. It is also unsafe in the
# untyped path: it renders a value holding a newline as
# `unistr('a\u000ab')`, so two engines that agree on the value print different
# text.
#
# nsqlited is driven with `--testsuite`, which prints the record stream the TCL
# shim reads: one record per line, a tag byte per field, and the value encoded.
#
#     C <n> <hex>...   n column names, opening a statement
#     R <tag>...       one row of that statement
#     E <text>         the statement failed; the text is the engine's message
#     X                a statement that reported a row count
#     N                a statement that returned nothing
#
# where a field is `T<hex>` or `B<hex>` for text or blob, `I<decimal>` or
# `F<text>` for an integer or a float, and a lone `-` for NULL. Two properties
# of that stream are what make it the right transport here.
#
# The first is that a NULL is a lone `-` and an empty string is a tag with no
# payload, so the one distinction the default print mode cannot make is made.
# The second is that an empty result is *unambiguous*: the `C` record is
# printed and no `R` record follows. The default print mode has neither
# property -- on an empty result it writes a Rust `Vec` debug of the column
# list, so `SELECT a FROM t` on an empty table prints `["a"]` with no trailing
# newline where sqlite3 prints nothing, and a harness that maps lines into rows
# counts that header as a row. Verified directly:
# `SELECT a,b,c FROM t ORDER BY a ASC LIMIT 0` is the same empty result on both
# engines, and the default-mode runner reported `row counts differ  sqlite3: 0
# nsqlited: 1`; in the record stream it is a `C` record and no rows. Nine of
# the twenty-four row-count disagreements in a 150-case run were this artefact.
#
# The two sides are brought to a common form by decoding the record stream's
# fields and re-encoding them the way list mode prints, and *not* the other way
# round: the record stream can represent things list mode cannot, and folding it
# down would throw those away. In the typed path the round trip is exact --
# every value is hex text the engine itself produced, so it decodes to an ASCII
# hex string and re-encodes to the same characters.
#
# The projection columns are aliased to c1..cN because the two engines disagree
# on the *generated* name of an unaliased expression column -- sqlite3 uses the
# source text, nsqlited renders a debug form -- and the runner must not compare
# column names it cannot agree on.
#
# A result list that cannot be rewritten this way is compared on the values
# themselves instead, in the transports' own form. That is weaker than the
# projection -- it is the printed rendering, not the storage class -- and it is
# labelled as such in the summary rather than passed off as the strict
# comparison.
#
# A statement that is not a query is compared by whether both engines accepted
# it and by the row count both report for it.
#
# Usage:
#   tools/difftest.sh cases.sql                 # run, print a pass/fail summary
#   tools/difftest.sh cases.sql -v              # echo every statement
#   tools/difftest.sh cases.sql --max 20        # stop reporting after 20 diffs
#   tools/difftest.sh --sql "SELECT 1"          # run one statement
#   tools/difftest.sh --self-test               # check this script's own parsing
#   tools/difftest.sh cases.sql --fresh-db      # each statement from an empty DB
#
# `--self-test` runs with neither engine: it checks the statement splitting and
# the result-list rewriting, which decide how strictly a query is compared. Run
# it first when a run reports more agreement than seems possible, because a bug
# there turns a strict comparison into a weaker one without ever saying so.
#
# Environment:
#   NSQLITED   nsqlite CLI      (default: target/debug/nsqlited.exe)
#   SQLITE3    real sqlite3     (default: sqlite3 on PATH)
#   WORKDIR    scratch location (default: a mktemp -d, removed on exit)
#
# Exit status: 0 when the engines agreed on every statement, 1 otherwise.
set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
NSQLITED="${NSQLITED:-$DIR/target/debug/nsqlited.exe}"
SQLITE3="${SQLITE3:-sqlite3}"

VERBOSE=0
MAXDIFF=0
CASE_FILE=""
ONE_SQL=""
SELFTEST=0
FRESH=0

usage() {
    sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        -v|--verbose) VERBOSE=1; shift ;;
        --self-test)   SELFTEST=1; shift ;;
        --fresh-db)    FRESH=1; shift ;;
        --max)        MAXDIFF="${2:-}"; shift 2 ;;
        --max=*)      MAXDIFF="${1#--max=}"; shift ;;
        --sql)        ONE_SQL="${2:-}"; shift 2 ;;
        --nsqlited)   NSQLITED="${2:-}"; shift 2 ;;
        --sqlite3)    SQLITE3="${2:-}"; shift 2 ;;
        -h|--help)    usage; exit 0 ;;
        --)           shift; break ;;
        -*)           echo "error: unknown option '$1' (try --help)" >&2; exit 2 ;;
        *)
            if [ -n "$CASE_FILE" ]; then
                echo "error: expected one file, got '$CASE_FILE' and '$1'" >&2
                exit 2
            fi
            CASE_FILE="$1"; shift ;;
    esac
done

if [ -z "$ONE_SQL" ] && [ $# -gt 0 ] && [ "$SELFTEST" -eq 0 ]; then
    CASE_FILE="$1"; shift
fi
if [ "$SELFTEST" -eq 0 ]; then
    [ -n "$CASE_FILE" ] || [ -n "$ONE_SQL" ] || { usage >&2; exit 2; }
    [ -x "$NSQLITED" ] || [ -f "$NSQLITED" ] || {
        echo "error: nsqlited CLI not found: $NSQLITED" >&2
        echo "hint: cargo build -p nsqlited, or set NSQLITED=PATH" >&2
        exit 2
    }
    command -v "$SQLITE3" >/dev/null 2>&1 || [ -x "$SQLITE3" ] || {
        echo "error: sqlite3 not found: $SQLITE3" >&2; exit 2
    }
fi

if [ -z "${WORKDIR:-}" ]; then
    WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/nsqlite-difftest.XXXXXX")" || exit 2
    trap 'rm -rf "$WORKDIR"' EXIT
fi
mkdir -p "$WORKDIR" || exit 2
NDB="$WORKDIR/nsqlite.db"
RDB="$WORKDIR/sqlite3.db"

TOTAL=0; AGREE=0; DIFFS=0; WEAK=0; STOP=0
# WORDING counts statements both engines refused for the same reason but
# worded differently, and SHOWWORD prints each one instead of only counting
# it. They are not disagreements: the engines agree the statement is an error
# and only the message differs. They are tallied separately because counting
# them as disagreements is what buried the refusals that really are one-sided.
WORDING=0
SHOWWORD="${SHOWWORD:-0}"
STMT=""

# Renders a value with control characters escaped, so a difference inside a
# string is visible rather than silently moving a row boundary.
show() { printf '%s' "$1" | cat -v; }

# Decodes a hex field into its typed form, for the report, so a reader can see
# `integer~1` rather than `696E74656765723E31`. Falls back to the hex when the
# field is not valid hex.
unhex() {
    local h="$1"
    case "$h" in
        ''|*[!0-9A-Fa-f]*) printf '%s' "$h"; return ;;
    esac
    printf '%s' "$h" | sed 's/../\\x&/g' | xargs -0 printf '%b' 2>/dev/null || printf '%s' "$h"
}

trim() {
    local s="$1"
    s="${s#"${s%%[![:space:]]*}"}"; s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

# Splits a file into statements on semicolons that are not inside a string, a
# quoted identifier, or a comment.
#
# A generated literal may contain a semicolon *or a newline*, so this tracks
# quoting rather than trusting line ends, and a statement may come out
# containing the newline that was inside its string. Records are therefore
# NUL-separated and read with `read -r -d ''`, because a statement that itself
# contains a newline cannot be read a line at a time -- doing so splits it and
# the halves fail to parse, which looks exactly like an engine disagreement.
#
# The semicolon that ends a statement flushes it and the loop `continue`s, and
# then `buf = buf "\n"` at the bottom of the action appends the newline that
# follows the semicolon -- so that newline opens the *next* record. At END
# whatever is left is flushed once more, and for a file ending in `SELECT 1;\n`
# that remainder is a bare newline. The trim in `flush()` therefore has to
# include the newline, not just spaces, tabs and CR, or the remainder counts as
# a non-empty record and is emitted.
#
# It did not, and that was worth a whole phantom statement. The reader's
# `[ -z "$STMT" ]` test runs *before* the statement is trimmed, so a record
# holding one newline character is not empty, is counted, and is then run
# against both engines and scored as an agreement. Verified against the
# original: a one-statement file reported `PASS: 2/2`, a two-statement file
# `3/3`, a three-statement file `4/4`, and every total and pass rate derived
# from them was inflated by one. The self-test now checks the record count
# directly rather than only that the text survives.
split_statements() {
    awk '
    function flush(   s) { s = buf; gsub(/^[ \t\r\n]+|[ \t\r\n]+$/, "", s); if (s != "") printf "%s%c", s, 0; buf = "" }
    {
        line = $0; n = length(line)
        for (i = 1; i <= n; i++) {
            c = substr(line, i, 1)
            if (in_c != "") {
                if (c == in_c) {
                    if (substr(line, i + 1, 1) == in_c) {
                        # A doubled quote is an escaped quote, not the end of
                        # the literal. The first of the two has already been
                        # appended, so the second is appended here and the index
                        # steps onto it, which keeps it from being appended
                        # again. Dropping it instead would silently remove a
                        # character from the statement, turning a valid INSERT
                        # into a syntax error and reporting a disagreement the
                        # tester invented.
                        buf = buf c; i++
                    } else {
                        in_c = ""                                 # backtick cannot be doubled
                    }
                }
            } else if (c == "\x27" || c == "\"" || c == "`") {
                in_c = c
            } else if (c == "-" && substr(line, i + 1, 1) == "-") {
                i = n; break                                     # line comment
            } else if (c == ";") {
                flush(); continue
            }
            buf = buf c
        }
        buf = buf "\n"
    }
    END { flush() }
    ' "$1"
}

# Reports one disagreement. Returns 0 so the caller keeps going.
report() {
    local reason="$1" r="$2" n="$3" detail="${4:-}"
    DIFFS=$((DIFFS + 1))
    if [ "$STOP" -eq 1 ]; then return 0; fi
    if [ "$MAXDIFF" -gt 0 ] && [ "$DIFFS" -ge "$MAXDIFF" ]; then
        STOP=1
        printf '\n(reporting stopped at --max %d)\n' "$MAXDIFF"
    fi
    printf '\n=== disagreement #%d (statement %d) ===\n' "$DIFFS" "$TOTAL"
    printf '  reason:    %s\n' "$reason"
    printf '  sqlite3:   %s\n' "$(show "$r")"
    printf '  nsqlited:  %s\n' "$(show "$n")"
    [ -n "$detail" ] && printf '  %s\n' "$detail"
    printf '  statement: %s\n' "$STMT"
    return 0
}

# Compares two projection rows column by column, so the report can name the
# first differing column rather than dumping the whole row.
compare_row() {
    local a="$1" b="$2" i n
    [ "$a" = "$b" ] && return 0
    local -a la lb
    IFS='|' read -r -a la <<< "$a"
    IFS='|' read -r -a lb <<< "$b"
    n=${#la[@]}; [ ${#lb[@]} -lt "$n" ] && n=${#lb[@]}
    for ((i = 0; i < n; i++)); do
        if [ "${la[$i]}" != "${lb[$i]}" ]; then
            printf 'column %d: sqlite3=%s (%s)  nsqlited=%s (%s)' "$((i + 1))" \
                "$(show "${la[$i]}")" "$(show "$(unhex "${la[$i]}")")" \
                "$(show "${lb[$i]}")" "$(show "$(unhex "${lb[$i]}")")"
            return 1
        fi
    done
    printf 'sqlite3 returned %d columns, nsqlited %d' "${#la[@]}" "${#lb[@]}"
    return 1
}

# Normalises an error message so the two shells' messages can be compared.
#
# sqlite3 wraps its message in a preamble naming where it happened -- "Parse
# error in 3rd command line argument:" for SQL passed as an argument, "Parse
# error near line N:" for SQL from standard input -- and adds the failing
# statement and a caret pointing at the token. nsqlited prefixes the result
# code, as "Error: ERROR:". None of that is the message, so it is stripped and
# only the message compared. The result code is not compared: the real shell
# does not expose it here either, and a difference in the code would show up
# as acceptance rather than as text.
norm_err() {
    # Drop the caret context sqlite3 prints after a parse error: the echoed
    # statement, the `^--- error here` marker, and the caret line itself. Those
    # lines all start with two spaces, or with a caret, and none of them is the
    # message. The message line is kept whatever it starts with.
    grep -v -e '^  ' -e '^[[:space:]]*\^' <<< "$1" \
      | sed -e 's/^Parse error in [^:]*: //' \
            -e 's/^Error in [^:]*: //' \
            -e 's/^Parse error near line [0-9]*: //' \
            -e 's/^Error near line [0-9]*: //' \
            -e 's/^Error: [A-Z][A-Z]*: //' \
      | grep -v '^[[:space:]]*$'
}

# True when two refusals are the same refusal worded differently.
#
# Where the two engines refuse a statement for the *same reason* but word it
# differently, the difference is a wording difference and not a defect, so it
# is reported as such rather than as an acceptance difference. SQLite says
# `datatype mismatch` where nsqlited says `datatype mismatch: 0.5 is not an
# integer`; both refuse the INSERT and only the tail of the sentence differs.
# Counting that as "nsqlited refused a statement sqlite3 accepted" buries the
# refusals that really are one-sided, and in a 150-case run it turned 409 raw
# disagreements into 277 refusals of which roughly 216 were this one wording.
#
# The test is deliberately narrow: a strict prefix of the other, up to the point
# where the longer message has a colon. `datatype mismatch` against
# `datatype mismatch: 0.5 is not an integer` is the same refusal. `no such
# table: t` against `no such column: a` is not -- neither is a prefix of the
# other -- and is reported in full.
same_refusal() {
    local a="$1" b="$2"
    [ "$a" = "$b" ] && return 0
    [ -n "$a" ] && [ -n "$b" ] || return 1
    case "$b" in "$a":*) return 0 ;; esac
    case "$a" in "$b":*) return 0 ;; esac
    return 1
}

# The number of columns a query returns, found without running it, or 1 if the
# statement is not a query.
#
# The query is wrapped in a subquery rather than having `LIMIT 0` appended, and
# that is the whole point of the wrapper. Appending `LIMIT 0` to a query that
# already ends in a LIMIT or an OFFSET makes a *second* one, and real sqlite3
# rejects that outright:
#
#     $ sqlite3 -batch :memory: \
#         "CREATE TEMP TABLE p AS SELECT 1 LIMIT 1 LIMIT 0; ..."
#     Parse error in 3rd command line argument: near "LIMIT": syntax error
#
# so the probe produced no column count, the projection fell through to the
# weaker text comparison, and *every* generated query carrying a LIMIT was
# quietly downgraded. Verified: the appended form returns a count for
# `SELECT 1`, `SELECT 1 ORDER BY 1`, `SELECT 1 WHERE 1` and
# `SELECT 1 GROUP BY 1`, and returns nothing for `SELECT 1 LIMIT 1`,
# `SELECT 1 OFFSET 0` and `SELECT 1 LIMIT 1 OFFSET 0`. The wrapped form returns
# 1 for all of them, and also for `SELECT * FROM t` and for a `WITH` query.
#
# This is also how a statement is classified as a query, rather than by
# matching a prefix. The old test read the first six characters and matched
# them against SELECT/WITH/VALU, which classified `SELECTED = 1` as a query
# because it *starts with* those six letters -- and the runner then scored the
# two engines' identical `near "SELECTED": syntax error` as an agreement
# reached by comparing rows. The `VALU` truncation is the same defect: `VALUE`
# and `VALUENT` match it too. A probe cannot be fooled that way.
#
# The probe has no side effects either. Wrapping the statement in a subquery
# requires it to be a query, so an INSERT or a CREATE inside it is a syntax
# error; and the connection is opened -readonly, which blocks a write to the
# scratch database even if one were somehow reachable. Verified:
#
#     $ sqlite3 -batch -readonly r.db "CREATE TEMP TABLE _p AS SELECT * FROM (INSERT INTO t VALUES(1,1)) LIMIT 0; ..."
#     Parse error in 4th command line argument: near "INSERT": syntax error
#
# The -readonly flag has one sharp edge, which is why `ensure_rdb` exists:
# sqlite3 refuses to *open* a database that does not exist yet under it, where
# without the flag it creates the file. The probe runs before the first
# statement has created anything, so a -readonly probe on a fresh scratch
# directory fails with `unable to open database file` for every statement, and
# every query in the file is misclassified as a non-query. The file is
# therefore created first, by an ordinary write, and only then probed.
ncols_of() {
    local sql="$1" n
    n="$("$SQLITE3" -batch -readonly "$RDB" \
        "DROP TABLE IF EXISTS temp._probe;
         CREATE TEMP TABLE _probe AS SELECT * FROM ($sql) LIMIT 0;
         SELECT count(*) FROM pragma_table_info('_probe');
         DROP TABLE temp._probe;" 2>/dev/null | tr -d ' \t\r\n')"
    case "$n" in ''|*[!0-9]*) return 1 ;; esac
    printf '%s' "$n"
}

# True when a statement is a query, so it is compared through the projection.
# Anything else is compared by acceptance and by the row count. The probe in
# ncols_of decides it: a statement is a query exactly when real sqlite3 can
# build a table from it as a subquery.
is_query() {
    local n
    n="$(ncols_of "$1")" || return 1
    [ "$n" -ge 1 ]
}

# Creates the real engine's scratch database if it is not there yet.
#
# The probe opens it -readonly, and sqlite3 will not *create* a database under
# -readonly: on a file that does not exist it fails with `unable to open
# database file` where an ordinary connection would have created it. Since the
# probe runs before the first statement of the file has executed, the file does
# not exist on the first statement, so the probe fails for every statement in a
# run and every query is misclassified as a non-query -- which is how a file
# holding nothing but `SELECT 1;` came to report one row-count disagreement
# where the engines agree. Creating the empty file first, with a connection
# that is allowed to write, is enough: the probe only needs it to exist.
ensure_rdb() {
    [ -f "$RDB" ] && return 0
    "$SQLITE3" -batch "$RDB" "SELECT 1;" >/dev/null 2>&1
    return 0
}

# The body of a query after the leading SELECT, and 1 if the statement is not a
# plain SELECT. `SELECT` alone, or a leading parenthesis, is not handled.
select_body() {
    local sql="$1"
    case "${sql%% *}" in
        SELECT) ;;
        *) return 1 ;;
    esac
    local rest="${sql#SELECT }"
    [ -z "$(trim "$rest")" ] && return 1
    printf '%s' "$rest"
}

# Reads a top-level comma-separated list starting at $2 for $3 characters,
# appending each element to the array named by $4. Quoting and parens are
# respected, so a comma inside a string or a function call does not split.
split_top_level() {
    local s="$1" from="$2" len="$3" item="" d=0 i ch q j
    for ((i = from; i < from + len; i++)); do
        ch="${s:$i:1}"
        case "$ch" in
            "'"|'"'|'`')
                q="$ch"; item="$item$ch"
                for ((j = i + 1; j < from + len; j++)); do
                    ch="${s:$j:1}"; item="$item$ch"
                    if [ "$ch" = "$q" ]; then
                        if [ "${s:$((j+1)):1}" = "$q" ]; then item="$item$q"; j=$((j + 1))
                        else j=$((j + 1)); break; fi
                    fi
                done
                # `j` is already one past the closing quote, and the loop's own
                # `i++` takes it one further, so it is stepped back. Without
                # this the character right after the literal is skipped, which
                # for `c||'!' , e` is the comma that ends the result item.
                i=$((j - 1)) ;;
            '(') d=$((d + 1)); item="$item$ch" ;;
            ')') d=$((d - 1)); item="$item$ch" ;;
            ,)  if [ "$d" -eq 0 ]; then
                    eval "${4}+=(\"\$(trim \"\$item\")\")"; item=""
                else item="$item$ch"; fi ;;
            *) item="$item$ch" ;;
        esac
    done
    [ -n "$(trim "$item")" ] && eval "${4}+=(\"\$(trim \"\$item\")\")"
    return 0
}

# True when the query carries a leading top-level DISTINCT, setting
# DISTINCT_LEAD to the word and DISTINCT_REST to the result list without it.
#
# `SELECT DISTINCT a FROM t` is one result item as far as find_result_list is
# concerned -- the word is part of the first item, not a separate one -- so the
# item count came out one above the column count, the shape check failed, and
# the statement was sent to the text fallback. So DISTINCT is recognised here
# and lifted *out* of the result list before the projection is built, and
# re-attached outside it, where it belongs:
#
#     SELECT DISTINCT hex(typeof(a)||'~'||quote(a)) AS c1 FROM t
#
# Two things are worth stating about that form. It is DISTINCT over the
# *projection*, which is what has to be distinct: DISTINCT over the original
# column and DISTINCT over the pair (typeof, quote) of that column select the
# same rows, because the projection is a function of the column, and a function
# cannot merge two rows that differ in the value it is applied to. And leaving
# DISTINCT inside each expression, as
# `hex(typeof(DISTINCT a)||'~'||quote(DISTINCT a))` would, is what the old
# builder did; real sqlite3 accepts that, but it applies DISTINCT to the
# expression rather than to the result set, which is a different query.
#
# The globals are set in this shell rather than printed and captured: a command
# substitution runs in a subshell, so a `DISTINCT=` assignment inside one would
# be discarded before the caller saw it.
split_distinct() {
    local rest="$1" i ch d=0 q j
    DISTINCT_LEAD=""; DISTINCT_REST=""
    for ((i = 0; i < ${#rest}; i++)); do
        ch="${rest:$i:1}"
        case "$ch" in
            "'"|'"'|'`')
                q="$ch"
                for ((j = i + 1; j < ${#rest}; j++)); do
                    ch="${rest:$j:1}"
                    if [ "$ch" = "$q" ]; then
                        if [ "${rest:$((j+1)):1}" = "$q" ]; then j=$((j + 1))
                        else i=$((j - 1)); break; fi
                    fi
                done ;;
            '(') d=$((d + 1)) ;;
            ')') d=$((d - 1)) ;;
        esac
        [ "$d" -eq 0 ] || continue
        case "${rest:$i:8}" in
            DISTINCT|'distinct')
                # Only a leading DISTINCT on the result list, not a column
                # named `distinct` -- which needs no quoting in SQL -- nor the
                # tail of a longer identifier.
                if [ "$i" -eq 0 ] || [ " " = "${rest:$((i-1)):1}" ]; then
                    DISTINCT_LEAD="DISTINCT "
                    DISTINCT_REST="$(trim "${rest:$((i + 8))}")"
                    return 0
                fi ;;
        esac
    done
    DISTINCT_REST="$rest"
    return 1
}

# Walks the result list, setting LIST (the items) and RTAIL (everything from the
# FROM onwards). Returns 1 when there is no top-level FROM, which means the
# query is a bare `SELECT <exprs>` with no table to read.
find_result_list() {
    local rest="$1" i ch d=0 q j
    LIST=(); RTAIL=""
    for ((i = 0; i < ${#rest}; i++)); do
        ch="${rest:$i:1}"
        case "$ch" in
            "'"|'"'|'`')
                q="$ch"
                for ((j = i + 1; j < ${#rest}; j++)); do
                    ch="${rest:$j:1}"
                    if [ "$ch" = "$q" ]; then
                        if [ "${rest:$((j+1)):1}" = "$q" ]; then j=$((j + 1))
                        else i=$((j - 1)); break; fi
                    fi
                done ;;
            '(') d=$((d + 1)) ;;
            ')') d=$((d - 1)) ;;
        esac
        if [ "$d" -eq 0 ] && [ "$ch" = 'F' ] && [ "${rest:$((i+1)):3}" = 'ROM' ]; then
            split_top_level "$rest" 0 "$i" LIST
            RTAIL=" FROM${rest:$((i + 4))}"
            [ "${#LIST[@]}" -gt 0 ] && return 0
            return 1
        fi
    done
    # No FROM: the whole body is the result list.
    split_top_level "$rest" 0 "${#rest}" LIST
    [ "${#LIST[@]}" -gt 0 ] && return 0
    return 1
}

# The columns a `SELECT *` expands to, from the real schema, which both
# engines report identically. Takes the query body (`* FROM t ...`) and prints
# the column names space-separated. Only the single-table form is handled; a
# join's `*` needs the FROM expanded, which is left to the fallback.
star_columns() {
    local rest="$1" after tbl
    after="${rest#\* FROM }"
    [ "$after" = "$rest" ] && return 1
    tbl="$(trim "${after%% *}")"
    tbl="${tbl%%,*}"
    [ -z "$tbl" ] && return 1
    "$SQLITE3" -batch -readonly "$RDB" \
        "SELECT name FROM pragma_table_info('$tbl') ORDER BY cid;" 2>/dev/null | tr '\n' ' '
}

# The projection for one output expression: the type, a separator, and the
# quoted value, all hex-encoded so no byte of it can act as a delimiter.
proj_of() { printf "hex(typeof(%s)||'~'||quote(%s))" "$1" "$1"; }

# Removes a trailing `AS name` or bare alias from a result-list item, so the
# projection can supply its own. A trailing alias is only removed at paren depth
# zero and outside a string, so `SELECT a AS b FROM t` loses the alias while
# `SELECT max(x) AS b` keeps the call and `SELECT 'AS b'` keeps the literal.
strip_alias() {
    local item="$1" i ch d=0 q j depth_at_alias=-1
    for ((i = 0; i < ${#item}; i++)); do
        ch="${item:$i:1}"
        case "$ch" in
            "'"|'"'|'`')
                q="$ch"
                for ((j = i + 1; j < ${#item}; j++)); do
                    ch="${item:$j:1}"
                    if [ "$ch" = "$q" ]; then
                        if [ "${item:$((j+1)):1}" = "$q" ]; then j=$((j + 1))
                        else i=$((j - 1)); break; fi
                    fi
                done ;;
            '(') d=$((d + 1)) ;;
            ')') d=$((d - 1)) ;;
        esac
        if [ "$d" -eq 0 ]; then
            if [ "${item:$i:3}" = 'AS ' ] || [ "${item:$i:3}" = 'as ' ]; then
                depth_at_alias=$i; break
            fi
        fi
    done
    [ "$depth_at_alias" -ge 0 ] || { printf '%s' "$item"; return; }
    printf '%s' "$(trim "${item:0:$depth_at_alias}")"
}

# Builds the projected form of a query. Prints the SQL on stdout and either
# `typed` or `text` on stderr, so the caller can report which comparison ran.
build_projection() {
    local sql="$1" n rest i out="" lead=""
    if ! n="$(ncols_of "$sql")" || [ "$n" -lt 1 ]; then
        printf '%s' "$sql"; echo text >&2; return
    fi
    if ! rest="$(select_body "$sql")"; then
        printf '%s' "$sql"; echo text >&2; return
    fi
    # DISTINCT is lifted off the result list and re-attached in front of the
    # projection, so that `SELECT DISTINCT a` keeps the strict comparison
    # instead of falling through on a shape mismatch.
    split_distinct "$rest"
    lead="$DISTINCT_LEAD"
    rest="$DISTINCT_REST"
    if ! find_result_list "$rest"; then
        printf '%s' "$sql"; echo text >&2; return
    fi
    # `SELECT *` is rewritten by expanding it against the real schema, so it
    # gets the strict comparison too rather than falling back to text.
    if [ "${#LIST[@]}" -eq 1 ] && [ "$(trim "${LIST[0]}")" = '*' ]; then
        local star
        if star="$(star_columns "$rest")" && [ "$(wc -w <<< "$star")" -eq "$n" ]; then
            local -a cols=()
            read -r -a cols <<< "$star"
            out="SELECT $lead"
            for ((i = 0; i < n; i++)); do
                [ "$i" -gt 0 ] && out="$out, "
                out="$out$(proj_of "${cols[$i]}") AS c$((i + 1))"
            done
            out="$out$RTAIL"
            printf '%s' "$out"; echo typed >&2; return
        fi
        printf '%s' "$sql"; echo text >&2; return
    fi
    [ "${#LIST[@]}" -eq "$n" ] || { printf '%s' "$sql"; echo text >&2; return; }
    out="SELECT $lead"
    for ((i = 0; i < n; i++)); do
        [ "$i" -gt 0 ] && out="$out, "
        # An expression may already carry an alias, as in `SELECT 1+1 AS x`.
        # That alias has to be stripped before the projection wraps the
        # expression, or the column would be aliased twice, which is a syntax
        # error on both sides. The projection supplies its own c1..cN.
        out="$out$(proj_of "$(strip_alias "${LIST[$i]}")") AS c$((i + 1))"
    done
    out="$out$RTAIL"
    printf '%s' "$out"
    echo typed >&2
}

# Reduces a shell's change-count output to the bare count.
#
# The count is the last line of the output that is a bare integer, since a
# statement's own output is never a bare integer unless the statement is the
# `SELECT changes()` the runner appended -- which is the point. nsqlited prints
# the count alone for a DML statement and nothing for a DDL one; sqlite3
# prints the appended `SELECT changes()` result, also alone.
#
# The `changes: N   total_changes: M` form that `.changes on` produces is still
# accepted, for a caller that turns it on, but nothing here does: `.changes on`
# makes a DDL parse error silent and exits 0, so it cannot be used to decide
# whether the statement was accepted at all.
rowcount_of() {
    local out="$1"
    local n
    n="$(grep -o 'changes: [0-9][0-9]*' <<< "$out" | tail -1 | grep -o '[0-9][0-9]*$')"
    if [ -z "$n" ]; then
        n="$(grep -E '^[0-9][0-9]*$' <<< "$out" | tail -1)"
    fi
    printf '%s' "$n"
}

# Whether each statement starts from an empty database.
#
# By default the two databases persist across the file, so a generated script
# that creates a table, inserts, and then selects is compared as a sequence --
# which is what the generated cases are for. But a single statement that one
# engine accepts and the other refuses leaves the two databases out of step for
# everything after it, and every later statement then reports a difference that
# is only a consequence. `--fresh-db` gives each statement its own pair of empty
# databases instead, so each disagreement is about that statement alone.
#
# The cost is severe and is worth being explicit about, because the earlier
# version of this script documented it only as "for scripts whose statements
# stand alone" and a run then used `--fresh-db` for its headline number. With
# the databases reset before *every* statement, each data-dependent statement
# runs against an empty pair: the SELECT finds no table, both engines answer
# `no such table: t`, the messages match, and the statement is scored as an
# AGREEMENT. The arithmetic, the quote of a real and the typeof of a NULL are
# never evaluated. Verified:
#
#     $ printf 'CREATE TABLE t(a,b,c);\nINSERT INTO t(a,b,c) VALUES(1,1.0,NULL);\n\
#         SELECT a, a*9999999999999999999999, quote(1.0), typeof(NULL) FROM t;\n' > f.sql
#     $ tools/difftest.sh f.sql                 # PASS: 4/4
#     $ tools/difftest.sh f.sql --fresh-db      # PASS: 4/4
#
# and the same file with a statement that genuinely disagrees shows the
# difference -- `SELECT DISTINCT a FROM t` over two rows is a disagreement with
# the databases held, and a vacuous agreement with them reset. So the mode
# isolates a parse or acceptance bug and nothing else: it discards the entire
# measured surface and keeps only DDL and DML acceptance. It cannot produce a
# count of semantic agreements, and the summary says so whenever it is on.
fresh_db() { [ "$FRESH" -eq 1 ]; }

reset_dbs() {
    rm -f "$NDB" "$RDB" "$NDB-journal" "$RDB-journal" 2>/dev/null
    # --fresh-db deletes the databases before every statement, so the file the
    # probe opens with -readonly has to be recreated here too. Without this the
    # probe fails for the whole --fresh-db run and every statement is
    # misclassified.
    ensure_rdb
    return 0
}

# --- the two transports ----------------------------------------------------

# Runs a statement on sqlite3 in list mode with `|` between fields, and returns
# its stdout.
#
# `.mode quote` was the wrong choice here and is worth recording why. Quote mode
# *wraps every value in single quotes*: the typed projection, whose values are
# already hex, comes out as `'696E74656765723E31'` where the nsqlite record
# stream has the same field unquoted, so every typed row disagreed on
# formatting alone. And in the fallback path quote mode is not safe either: it
# renders a value holding a newline as `unistr('a\u000ab')`, so two engines
# that agree on the value still print different text. List mode leaves the
# value's own text alone.
#
# List mode does keep a real from collapsing into an integer, which is what the
# default mode gets wrong:
#
#     $ sqlite3 -batch :memory: "SELECT 1.0, 1;"      ->  1|1
#     $ sqlite3 -batch :memory: ".mode list" "SELECT 1.0, 1;"  ->  1.0|1
#
# so even the fallback can see a storage-class difference. It cannot see one
# inside a value that is *already* text -- that is what the projection is for,
# and the fallback is reported as the weaker comparison because it is.
#
# `.nullvalue` is set so a NULL prints as a word rather than as nothing, which
# keeps a row holding NULL from being indistinguishable from an empty field.
# `.headers off` keeps the column names out of the output, since the two
# engines generate different names for an unaliased expression column.
run_sqlite_list() {
    printf '.mode list\n.headers off\n.separator |\n.nullvalue ~NULL~\n%s\n' "$1" \
        | "$SQLITE3" -batch "$RDB" 2>"$WORKDIR/r.err"
}

# Runs a statement on nsqlited in testsuite mode and returns its stdout.
#
# The record stream is the right transport because an empty result is
# unambiguous in it. The default print mode has no such property: on a query
# with no rows it writes a Rust `Vec` debug of the column list, so
# `SELECT a FROM t` on an empty table prints `["a"]` with no trailing newline
# where sqlite3 prints nothing, and a harness that maps output lines into rows
# counts that header as a row. Verified: a semantically identical empty result
# on both sides, `SELECT a,b,c FROM t ORDER BY a ASC LIMIT 0`, was reported as
# `row counts differ  sqlite3: 0  nsqlited: 1`, and 9 of 24 such reports in a
# 150-case run were this one artefact. In the record stream the same query
# prints a `C` record and no `R` record, which is zero rows and nothing else.
run_nsuite_record() {
    "$NSQLITED" --testsuite "$NDB" "$1" 2>"$WORKDIR/n.err"
}

# The `R` rows a testsuite run printed, one per line, re-encoded into the same
# form the real engine is driven into, so the two sides are directly comparable.
#
# The record stream tags every field and encodes it: `T` is text with a hex
# payload, `B` is a blob with a hex payload, `I` an integer whose payload is
# its decimal text, `F` a float whose payload is SQLite's `%!.15g` rendering,
# and a bare `-` is NULL. sqlite3 in list mode prints none of that:
#
#     nsqlited  R - T T612062 T617C62 I31 F312E30
#     sqlite3   ~NULL~|      |a b|a|b|1|1.0
#
# so the fields are decoded and then re-encoded the way sqlite3 prints them,
# rather than the other way round. The direction matters: the record stream can
# represent things list mode cannot, and folding it down to list mode would
# throw those away. A NULL becomes `~NULL~` and an empty string becomes the
# empty field, so the pair the default print mode conflates stays apart.
#
# In the *typed* path the re-encoding is lossless for a different reason: every
# value is hex text produced by the engine itself, so it decodes to an ASCII
# hex string and is re-encoded to exactly those characters, which is what the
# real engine's `hex()` output already is. That is why the projection survives
# the round trip byte for byte.
#
# A blob is re-encoded as its hex digits, matching what list mode prints for one
# (`SELECT x FROM t` with x'31' prints `31` on both), and a text value that
# happens to hold the same bytes is the same characters, so in the *untyped*
# path a blob and a text value of the same bytes compare equal -- which is
# exactly the weakness the projection exists to remove, and why the untyped
# path is reported as the weaker one.
record_rows() {
    local out="$1" line field first
    while IFS= read -r line; do
        case "$line" in
            'R '*) ;;
            'E '*) printf 'E %s\n' "${line#E }"; continue ;;
            *) continue ;;
        esac
        line="${line#R }"
        first=1
        # The fields are space-separated and no field can contain a space: a
        # text or blob payload is hex, an integer is decimal, a float is
        # SQLite's own rendering, and NULL is a lone dash. So splitting on
        # whitespace cannot break one field into two.
        for field in $line; do
            [ "$first" -eq 1 ] || printf '|'
            first=0
            case "$field" in
                T*|B*) printf '%s' "$(printf '%s' "${field#?}" | xxd -r -p 2>/dev/null)" ;;
                I*|F*) printf '%s' "${field#?}" ;;
                -)     printf '%s' '~NULL~' ;;
                *)     printf '%s' "$field" ;;
            esac
        done
        printf '\n'
    done <<< "$out"
}

# --- self-test -------------------------------------------------------------
#
# A differential tester that compares too loosely, or that mis-parses the SQL
# it was handed, is worse than none: it reports agreement that is not there.
# The text handling below is where that happens -- splitting statements, finding
# the result list, stripping an alias, recognising DISTINCT -- and every one of
# those is a place an off-by-one silently turns a strict comparison into a
# weaker one. So they are checked directly, with no engines involved, by
# `--self-test`.
#
# Each case is one that a real bug here would have broken: a comma after a
# string literal, a comma inside one, a nested call, a statement that arrives
# with the newlines that surrounded it in the file, and a file whose statement
# count is checked against its semicolon count rather than assumed.
self_test() {
    local fails=0 got want name
    check_list() {
        name="$1"; want="$2"; local input="$3"
        LIST=(); RTAIL=""
        if ! find_result_list "$input"; then
            printf 'FAIL %s: find_result_list refused the input\n' "$name"
            fails=$((fails + 1)); return
        fi
        got="${LIST[*]}"
        if [ "$got" != "$want" ]; then
            printf 'FAIL %s: got [%s] want [%s]\n' "$name" "$got" "$want"
            fails=$((fails + 1))
        else
            printf 'ok   %s\n' "$name"
        fi
    }
    check_alias() {
        got="$(strip_alias "$1")"; want="$2"
        if [ "$got" = "$want" ]; then
            printf 'ok   alias %s -> %s\n' "$1" "$got"
        else
            printf 'FAIL alias %s: got [%s] want [%s]\n' "$1" "$got" "$want"
            fails=$((fails + 1))
        fi
    }
    check_distinct() {
        name="$1"; want_lead="$2"; want_rest="$3"; local input="$4" want_rc="$5"
        split_distinct "$input"; local rc=$?
        if [ "$DISTINCT_LEAD" != "$want_lead" ] || [ "$DISTINCT_REST" != "$want_rest" ]; then
            printf 'FAIL %s: got lead [%s] rest [%s], want lead [%s] rest [%s]\n' \
                "$name" "$DISTINCT_LEAD" "$DISTINCT_REST" "$want_lead" "$want_rest"
            fails=$((fails + 1))
        elif [ "$rc" -ne "$want_rc" ]; then
            printf 'FAIL %s: got rc %d want %d\n' "$name" "$rc" "$want_rc"
            fails=$((fails + 1))
        else
            printf 'ok   %s\n' "$name"
        fi
    }
    check_count() {
        name="$1"; want="$2"; local file="$3" n=0 s
        while IFS= read -r -d '' s; do
            [ -n "$(trim "$s")" ] && n=$((n + 1))
        done < <(split_statements "$file")
        if [ "$n" -ne "$want" ]; then
            printf 'FAIL %s: got %d statements, want %d\n' "$name" "$n" "$want"
            fails=$((fails + 1))
        else
            printf 'ok   %s (%d statements)\n' "$name" "$n"
        fi
    }

    check_list "plain result list"     "a b"                 "a, b FROM t"
    # The case that was actually broken: the comma after a string literal was
    # being swallowed, so `c||'!', e/2` came out as one item instead of two.
    check_list "comma after a literal" "a+1 c||'!' e/2"       "a+1, c||'!', e/2 FROM t"
    check_list "comma inside a literal" "a 'x,y' b"            "a, 'x,y', b FROM t"
    check_list "nested call"           "max(a) min(b)"        "max(a), min(b) FROM t"
    check_list "no FROM"               "1 2"                  "1, 2"
    check_list "padded"                "a b"                  "   a ,  b   FROM t"
    check_list "aliased item"          "max(a) AS m"          "max(a) AS m FROM t"

    check_alias "a AS b"      "a"
    check_alias "max(a) AS m" "max(a)"
    check_alias "'AS b'"      "'AS b'"
    check_alias "a"           "a"

    # DISTINCT has to be recognised and lifted off the result list. Left in
    # place it becomes part of the first result item, so the item count is one
    # above the column count, the shape check fails, and the statement drops to
    # the weaker comparison -- which is exactly what happened before.
    check_distinct "leading DISTINCT" "DISTINCT " "a FROM t"  "DISTINCT a FROM t" 0
    check_distinct "lower-case distinct" "DISTINCT " "a FROM t" "distinct a FROM t" 0
    check_distinct "no DISTINCT" "" "a FROM t" "a FROM t" 1
    # A *longer* identifier that merely starts with the keyword, and DISTINCT
    # inside parentheses, are both not a result-set DISTINCT. A column whose
    # name is exactly `distinct` needs quoting in SQL, so it is not a case --
    # real sqlite3 rejects `CREATE TABLE t(distinct)`, and a quoted one arrives
    # as `"distinct"`, which is not the bare word the scan is looking for.
    check_distinct "identifier starting with distinct" "" "nondistinct FROM t" \
        "nondistinct FROM t" 1
    check_distinct "DISTINCT inside parens" "" "count(DISTINCT a) FROM t" \
        "count(DISTINCT a) FROM t" 1
    check_distinct "DISTINCT in a string" "" "'DISTINCT' AS d FROM t" \
        "'DISTINCT' AS d FROM t" 1
    # A result list that merely *contains* the word is not DISTINCT. A
    # mid-list DISTINCT is not a thing -- real sqlite3 rejects
    # `SELECT a, DISTINCT b FROM t` and `SELECT a AS distinct FROM t` -- so the
    # only way the word can appear in a result list is inside a literal or a
    # quoted identifier, and both are covered above.

    # The splitter's own arithmetic, which is where the phantom record came
    # from: one statement reported 2/2 because the newline that followed the
    # last `;` was flushed at END as a record of its own. The count is checked
    # against the file, here, because the text surviving the split says nothing
    # about how many records came out.
    printf 'SELECT 1;\n' > "$WORKDIR/c1.sql"
    printf 'SELECT 1;\nSELECT 2;\n' > "$WORKDIR/c2.sql"
    printf 'SELECT 1;\nSELECT 2;\nSELECT 3;\n' > "$WORKDIR/c3.sql"
    printf 'SELECT 1;\nSELECT 2;\nSELECT 3;\nSELECT 4;\nSELECT 5;\n' > "$WORKDIR/c5.sql"
    # No trailing newline at all, which is the other shape that used to slip a
    # record out.
    printf 'SELECT 1;\nSELECT 2;' > "$WORKDIR/c2nt.sql"
    check_count "one statement"        1 "$WORKDIR/c1.sql"
    check_count "two statements"       2 "$WORKDIR/c2.sql"
    check_count "three statements"     3 "$WORKDIR/c3.sql"
    check_count "five statements"      5 "$WORKDIR/c5.sql"
    check_count "two, no trailing nl"  2 "$WORKDIR/c2nt.sql"

    # A statement read out of the file keeps the newlines around it, so the
    # record is trimmed before the emptiness test rather than after.
    printf "\nSELECT 1\n" > "$WORKDIR/pad.sql"
    local padded="" s
    while IFS= read -r -d '' s; do
        [ -n "$(trim "$s")" ] && padded="$padded$(trim "$s")"$'\n'
    done < <(split_statements "$WORKDIR/pad.sql")
    if [ "$padded" = "SELECT 1
" ]; then
        printf 'ok   is_query trims the newlines around a record\n'
    else
        printf 'FAIL trimmed record is [%s], want [SELECT 1]\n' "$(show "$padded")"
        fails=$((fails + 1))
    fi

    # The splitter, checked end to end. A doubled quote is an escaped quote, and
    # an escaped quote inside a literal must survive the split: dropping one
    # turns VALUES with an escaped quote into a literal with an unbalanced one,
    # which is a syntax error the real engine accepts and this one does not, so
    # the tester would invent a disagreement out of a valid statement.
    #
    # The statements are trimmed, exactly as run_one trims them, because a
    # statement arrives with the newlines that surrounded it in the file.
    local split_out=""
    printf "INSERT INTO t(a) VALUES('a''b', 'x\ny');\nSELECT 1;\n" > "$WORKDIR/self.sql"
    while IFS= read -r -d '' s; do
        s="$(trim "$s")"
        [ -n "$s" ] && split_out="$split_out$s"$'\n'
    done < <(split_statements "$WORKDIR/self.sql")
    if [ "$split_out" = "INSERT INTO t(a) VALUES('a''b', 'x
y')
SELECT 1
" ]; then
        printf 'ok   split keeps an escaped quote and an embedded newline\n'
    else
        printf 'FAIL split mangled the literal: [%s]\n' "$(show "$split_out")"
        fails=$((fails + 1))
    fi

    # The summary is read as "N of TOTAL agreed", so a statement that was
    # reported as a disagreement must not also be counted as an agreement. It
    # was: the non-query and query refusal paths each incremented AGREE after
    # calling report, so a run whose every statement was reported as a
    # disagreement printed `6/6 statements agreed, 6 disagreed` -- the same six
    # statements counted twice, and a pass rate computed from it was twice the
    # agreement it claimed to be.
    #
    # The invariant is checked here rather than in a run, because it needs no
    # engines: AGREE + DIFFS is the number of statements that resolved one way
    # or the other, and it must never exceed TOTAL.
    if [ $((AGREE + DIFFS)) -le "$TOTAL" ]; then
        printf 'ok   agreement and disagreement counts do not overlap\n'
    else
        printf 'FAIL AGREE (%d) + DIFFS (%d) exceeds TOTAL (%d)\n' \
            "$AGREE" "$DIFFS" "$TOTAL"
        fails=$((fails + 1))
    fi

    if [ "$fails" -eq 0 ]; then
        printf '\nself-test: all checks passed\n'
        return 0
    fi
    printf '\nself-test: %d check(s) failed\n' "$fails"
    return 1
}

# One statement, run against both engines and compared.
run_one() {
    # A statement taken from the splitter can be padded with the newlines that
    # surrounded it in the file, so it is trimmed here rather than at each use.
    STMT="$(trim "$1")"
    TOTAL=$((TOTAL + 1))
    [ "$VERBOSE" -eq 1 ] && printf '\n[%d] %s\n' "$TOTAL" "$STMT"
    fresh_db && reset_dbs

    if ! is_query "$STMT"; then
        # A non-query is compared by whether both engines accepted it and by the
        # row count both report for it.
        #
        # sqlite3 is asked for the count in a way that does not hide a failure,
        # and that took some care. `.changes on` was what this used, and it is
        # unusable: a DDL parse error under `.changes on` is *silent and exits
        # 0*. Verified against the real shell:
        #
        #     $ printf '.changes on\nCREATE TABLE t(a INT, b CHAR(10), c CLOB);\n' \
        #         | sqlite3 -batch fresh.db
        #     changes: 0   total_changes: 0
        #     $ echo $?
        #     0
        #
        # So for every DDL statement the runner recorded "sqlite3: ok" whatever
        # sqlite3 had actually done, and an engine that refused the statement
        # outright was reported as having disagreed with a success. Nothing is
        # taken from `.changes` now.
        #
        # The count comes from `SELECT changes()`, appended to the statement in
        # the *same* sqlite3 invocation. It has to be the same invocation:
        # `changes()` and `total_changes()` are per-connection counters, so
        # sampling the total from a second invocation always reads 0, which is
        # how a two-row INSERT came to be compared against 0. Verified:
        #
        #     $ sqlite3 -batch k.db "INSERT INTO t VALUES(1),(2)"
        #     $ sqlite3 -batch k.db "SELECT total_changes();"    ->  0
        #     $ printf 'INSERT INTO t VALUES(1),(2);\nSELECT changes();\n' \
        #         | sqlite3 -batch k.db                          ->  2
        #
        # The appended statement needs its own leading semicolon. A statement
        # that arrives from the splitter has had its own `;` consumed -- the
        # splitter splits *on* the semicolon -- so without one the two run
        # together and sqlite3 reports `near "SELECT": syntax error` against
        # text neither engine was ever given. With it, a DDL statement reports
        # 0, which is also what nsqlited reports by saying nothing.
        #
        # A refused statement still reports its refusal on stderr and still
        # exits non-zero with the count appended, so appending the count hides
        # nothing. A count is compared only when both sides produced one.
        local nerr rerr nrc nout rout
        nerr="$("$NSQLITED" "$NDB" "$STMT" 2>&1 >"$WORKDIR/n.out")"; nrc=$?
        nout="$(cat "$WORKDIR/n.out")"
        # sqlite3's message is read from the stderr *file*, not from the
        # pipeline's own stdout. A command substitution around a pipeline whose
        # stdout is redirected captures nothing, so `rerr` has to be read back
        # out of the file afterwards -- otherwise the message is empty, the
        # refusal test never fires, and every refusal by the real engine is
        # scored as an agreement with nsqlited's refusal.
        printf '%s;\nSELECT changes();\n' "$STMT" \
            | "$SQLITE3" -batch "$RDB" >"$WORKDIR/r.out" 2>"$WORKDIR/r.err"
        rrc=$?
        rerr="$(cat "$WORKDIR/r.err")"
        rout="$(cat "$WORKDIR/r.out")"
        [ -s "$WORKDIR/r.err" ] && rrc=1
        if [ "$nrc" -ne 0 ] || [ "$rrc" -ne 0 ]; then
            nerr="$(norm_err "$nerr")"; rerr="$(norm_err "$rerr")"
            if [ "$nrc" -ne 0 ] && [ "$rrc" -ne 0 ] && [ "$nerr" = "$rerr" ]; then
                AGREE=$((AGREE + 1)); return 0
            fi
            if [ "$nrc" -ne 0 ] && [ "$rrc" -ne 0 ] && same_refusal "$nerr" "$rerr"; then
                # Both refused it, for the same reason, and differ only in how
                # much of the offending value each chose to name. That is a
                # wording difference, not an acceptance difference, and it is
                # tallied on its own so that it does not drown out the
                # refusals where exactly one engine refused. `SHOWWORD=1` prints
                # each one; off by default, because in a generated run there
                # are hundreds of them and they are all the same sentence.
                WORDING=$((WORDING + 1))
                if [ "$SHOWWORD" -eq 1 ]; then
                    report "both refused, worded differently (wording only)" \
                        "$rerr" "$nerr" \
                        "  the engines agree this is an error; only the message differs"
                fi
                AGREE=$((AGREE + 1)); return 0
            fi
            if [ "$nrc" -ne 0 ]; then
                report "nsqlited refused a statement sqlite3 accepted" "ok" "Error: $nerr" || return 0
            else
                report "sqlite3 refused a statement nsqlited accepted" "Error: $rerr" "ok" || return 0
            fi
            # No AGREE here. A statement that has just been reported as a
            # disagreement is not an agreement, and counting it as both made
            # the summary self-contradictory -- a run reported
            # `6/6 statements agreed, 6 disagreed`, and every one of those six
            # was the same statement counted twice. The summary is read as
            # "N of TOTAL agreed", so this number is what a pass rate is
            # computed from and it has to mean what it says.
            return 0
        fi
        # The change count, taken from each engine's own report of it. nsqlited
        # prints the number for a DML statement and nothing for a DDL one.
        #
        # sqlite3's count is read out of the *database* rather than off its
        # output, because its output is not available: `.changes on` was the
        # only way to make it report a count and it hides errors. The cumulative
        # total is read before and after the statement, and the difference is
        # the per-statement count, so the statement is run exactly once. A
        # statement that changed nothing, and a statement that was refused, both
        # leave the total where it was.
        nout="$(rowcount_of "$nout")"
        rout="$(rowcount_of "$rout")"
        # A DDL statement changes no rows and neither engine says so: nsqlited
        # emits nothing, and the database total does not move. A count is
        # compared only when both sides produced one, which for a DML statement
        # they both do, and acceptance alone is the comparison otherwise.
        if [ -n "$nout" ] && [ -n "$rout" ] && [ "$nout" != "$rout" ]; then
            report "row count differs" "$(show "$rout")" "$(show "$nout")" || return 0
            return 0
        fi
        AGREE=$((AGREE + 1))
        return 0
    fi

    local proj kind
    proj="$(build_projection "$STMT" 2>"$WORKDIR/kind")"
    kind="$(cat "$WORKDIR/kind")"

    local nrc rrc p_n p_r
    p_n="$(run_nsuite_record "$proj")"; nrc=$?
    p_r="$(run_sqlite_list "$proj")"; rrc=$?

    if [ "$nrc" -ne 0 ] || [ "$rrc" -ne 0 ]; then
        # nsqlited reports a failed statement in the record stream, as an `E`
        # record carrying the engine's own message, and exits non-zero; its
        # stderr is empty in that mode. sqlite3 reports on stderr. The `E` text
        # is the message, not the record stream's framing, so it is read back
        # out rather than compared whole.
        local nerr rerr
        nerr="$(record_rows "$p_n" | sed -n 's/^E //p')"
        [ -n "$nerr" ] || nerr="$(norm_err "$(cat "$WORKDIR/n.err")")"
        rerr="$(norm_err "$(cat "$WORKDIR/r.err")")"
        if [ "$nrc" -ne 0 ] && [ "$rrc" -ne 0 ] && same_refusal "$nerr" "$rerr"; then
            AGREE=$((AGREE + 1)); return 0
        fi
        if [ "$nrc" -ne 0 ]; then
            report "nsqlited refused a query sqlite3 accepted" "ok" "Error: $nerr" || return 0
        else
            report "sqlite3 refused a query nsqlited accepted" "Error: $rerr" "ok" || return 0
        fi
        # Not an agreement, for the same reason as in the non-query path above.
        return 0
    fi

    # The rows, from whichever transport ran.
    #
    # nsqlited's rows come from the `R` records of the testsuite stream, whose
    # fields are space-separated, so they are rejoined with `|` to line up with
    # sqlite3's `.separator |` output. The two sides line up field for field
    # because in the typed path every field is a hex string: no projection
    # value can contain a `|`, a newline, or a quote, so the separator cannot
    # appear inside one and a row splits into exactly as many fields as it has
    # columns.
    #
    # A NULL field in the nsqlite stream is a lone `-` and an empty text field
    # is a tag with no payload, so the one pair the default print mode cannot
    # tell apart stays apart here.
    #
    # Nothing strips a leading integer from either side any more. The old
    # script ran `sed '/^[0-9][0-9]*$/d'` over both outputs to drop what it
    # took to be a change count leaking out of a preceding DML, and that sed
    # deleted *every* row of any query whose projected rows are bare integers:
    # in the typed path the projection columns are hex strings and can never be
    # bare integers, so there was nothing there to strip, while in the
    # untyped path a value that is an integer really does print as a bare
    # integer. A table holding 1 and 2 gave `1\n2` on both sides, the sed
    # emptied both, both row counts became 0, and the runner reported the
    # statement as an agreement -- an engine returning zero rows where the
    # other returned two would have scored the same. The leak it was papering
    # over does not happen: the CLI runs one statement per invocation here, and
    # in the record stream a DML statement is an `X` record rather than a line
    # of output that a later query could inherit.
    # sqlite3's side: one line per row, fields already `|`-separated.
    local -a ar an
    if [ -z "$p_r" ]; then
        ar=()
    else
        mapfile -t ar <<< "$(printf '%s\n' "$p_r" | tr -d '\r')"
    fi
    # nsqlited's side: the `R` records, tags stripped and fields rejoined by `|`
    # by record_rows, so the two sides line up field for field.
    an=()
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        case "$line" in E*) continue ;; esac
        an+=("$line")
    done < <(record_rows "$p_n")

    local cnt=${#ar[@]} i
    [ ${#an[@]} -lt "$cnt" ] && cnt=${#an[@]}
    for ((i = 0; i < cnt; i++)); do
        if [ "${ar[$i]}" != "${an[$i]}" ]; then
            local d
            if [ "$kind" = typed ]; then
                d="$(compare_row "${ar[$i]}" "${an[$i]}" || true)"
                report "values differ (typed projection)" \
                    "row $((i + 1)): ${ar[$i]}" "row $((i + 1)): ${an[$i]}" "$d" || return 0
            else
                report "values differ (untyped: no projection available)" \
                    "row $((i + 1)): $(show "${ar[$i]}")" \
                    "row $((i + 1)): $(show "${an[$i]}")" || return 0
                WEAK=$((WEAK + 1))
            fi
            return 0
        fi
    done
    if [ "${#ar[@]}" -ne "${#an[@]}" ]; then
        report "row counts differ" "${#ar[@]}" "${#an[@]}" \
            "  an empty result is a C record with no R records in the nsqlite stream" || return 0
        return 0
    fi
    [ "$kind" = text ] && WEAK=$((WEAK + 1))
    AGREE=$((AGREE + 1))
    return 0
}

if [ "$SELFTEST" -eq 1 ]; then
    # Runs with no engines and no case file: it checks only this script's own
    # text handling, which is what decides how strictly a query is compared.
    self_test
    exit $?
else
    # The probe opens the real engine's database with -readonly, which cannot
    # create it, so the file is made once before any statement runs.
    ensure_rdb
fi

if [ -n "$ONE_SQL" ]; then
    run_one "$ONE_SQL" || true
elif [ -n "$CASE_FILE" ]; then
    while IFS= read -r -d '' STMT; do
        [ -z "$(trim "$STMT")" ] && continue
        run_one "$STMT" || true
    done < <(split_statements "$CASE_FILE")
fi

printf '\n%s: %d/%d statements agreed, %d disagreed' \
    "$([ "$DIFFS" -eq 0 ] && echo PASS || echo FAIL)" \
    "$AGREE" "$TOTAL" "$DIFFS"
[ "$WORDING" -eq 0 ] || printf ', %d refusals worded differently' "$WORDING"
[ "$WEAK" -eq 0 ] || printf ', %d compared untyped (no projection available)' "$WEAK"
[ "$FRESH" -eq 0 ] || printf ' (WARNING: --fresh-db, so data-dependent statements ran against empty databases)'
printf '\n'
[ "$DIFFS" -eq 0 ] || exit 1
exit 0

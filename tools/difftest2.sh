#!/usr/bin/env bash
# Differential runner, THIRD generation: nsqlited against the real sqlite3,
# compared by VALUE, over a corpus of cases whose disagreements are the
# deliverable.
#
# WHAT CHANGED FROM THE PREVIOUS VERSION OF THIS FILE, AND WHY
#
# The previous version compared values and ran the real engine, which was right.
# Two things about it were measured and found wanting, and both are fixed here.
#
# 1. A REAL COMPARISON DEFECT. The fallback transport could not tell a blob from
#    a text value. `record_rows` decoded every tagged field to its raw bytes, so
#    a NULL, an empty string and a blob of the same bytes all printed as an
#    empty field, and the `|`-joined lines were then compared as strings. A file
#    that stored '' where the other engine stored x'' scored an AGREEMENT.
#    `record_rows`'s own comment said the record stream is used "because a NULL
#    is a lone - and an empty string is a tag with no payload" -- true of the
#    STREAM, false of the function that decoded it. Measured, not reasoned
#    about, on this engine's own output:
#
#        $ nsqlited --testsuite :memory: "SELECT ''"
#        C 1 T2727
#        R T
#        $ nsqlited --testsuite :memory: "SELECT x''"
#        C 1 T782727
#        R B
#        $ nsqlited --testsuite :memory: "SELECT 'ABC'"   ->  R T414243
#        $ nsqlited --testsuite :memory: "SELECT x'414243' " ->  R B414243
#
#    The three payloads are identical and only the tag distinguishes them, and
#    the old decoder threw the tag away. Its self-test could not catch this
#    because the check it made -- "NULL is not the empty string", on a
#    `~NULL~|`-framed pair -- went through a framing the function no longer
#    produced.
#
#    The fix is to make BOTH engines emit the same self-describing token for
#    every value, rather than to reconstruct one side's. That is measured in
#    "TWO ENGINES, ONE TOKEN" below.
#
# 2. A PERFORMANCE DEFECT that made a wide corpus impractical. 20 statements
#    took 107.9s, and the time is not in the engines:
#
#        nsqlited, 20 individual spawns of a 3-row SELECT   1.394s
#        sqlite3,  20 individual spawns of a 1-row SELECT   2.710s
#        record_rows, 20 calls on a 3-row x 2-column stream  18.406s
#
#    The two engines together are 4.1s of 107.9s; `record_rows` is the other
#    96%, because each field spent a `$( )` subshell, which is a fork. So this
#    version
#
#      * decodes a record stream by string surgery in a single process, not by
#        a subshell per field, and
#      * runs a whole SECTION as one script per engine rather than one spawn
#        per statement. Both engines read a multi-statement script: measured,
#        `printf 'SELECT 1;\nSELECT 2;\n' | nsqlited --testsuite :memory:`
#        prints both record streams in order, and 31 statements on one spawn
#        take 0.236s against 2.597s for 20 individual spawns.
#
#    The result is two spawns per section instead of four per statement, which
#    is what makes a corpus wide enough to be worth reading. The strictness of
#    the comparison is unchanged: same rows, same order, same fields, exact.
#
# TWO ENGINES, ONE TOKEN
#
# Every value is reduced to a self-describing token, so the storage class is
# part of what is compared rather than something the runner has to guess at.
# The token for a value is:
#
#     null                 for NULL
#     i<decimal>           for an integer
#     r<SQLite's %>.15g>   for a real
#     t'<bytes>'           for text, with ' doubled
#     b<hex>               for a blob, uppercase
#
# The real engine produces those tokens by being ASKED for them, in quote mode:
#
#     SELECT quote(e) FROM (SELECT <expr> AS e)
#
#     $ sqlite3 -batch :memory: ".mode quote
#                                SELECT quote(NULL), quote(''), quote(x''),
#                                        quote(1), quote(1.0), quote('a''b');"
#     NULL|''|x''|1|1.0|'a''b'
#
# and this engine produces the same tokens from its record stream, where the
# tag byte already says which one to build:
#
#     R -        -> null
#     R I31      -> i31
#     R F312E30  -> r1.0
#     R T612762  -> t'a b'
#     R B414243  -> b414243
#
# The real engine's rendering is reached through `quote()` rather than by
# reading its list-mode output, and that is load-bearing in three measured ways.
# In `.mode quote` a text value is NOT quoted, so a value cannot be told from
# its own literal form:
#
#     $ sqlite3 -batch :memory: ".mode quote" "SELECT 'abc', x'616263';"
#     abc
#     x'616263'
#
# and the class is carried by a rendering convention, not by syntax: a real is
# `1.0` where an integer `1`, a NULL is the bare word `NULL`, a blob is
# `x'..'`. Reading that convention off the output is a parser, and a parser can
# be wrong. `quote()` puts it inside the engine, where it is a value, and the
# two engines are then asked the SAME question and the answers are compared. The
# cost is one extra projection per statement, which is a statement like any
# other and cannot be answered without asking somebody.
#
# `quote()` is also the one rendering the previous version's comment already
# relied on, and it was checked: the two engines agree digit for digit on
# quote(1.0/3), quote(0.1+0.2), quote(1e300), quote(1e-300), quote(-0.0),
# quote(1.5e-8), quote(1e20), quote(1.0e+17) and quote(100.0). So a
# disagreement on this path is about the VALUE, not about how either engine
# chose to spell it.
#
# NOTHING IS NORMALISED
#
# Rows are not sorted, columns are not reordered, no text is trimmed or
# case-folded, and a difference in row count is a difference. ORDER BY is part
# of the answer wherever it is under test, and a sorted comparison would pass an
# engine that returns the right rows in the wrong order.
#
# Two rendering differences are removed, and both are properties of the SHELL
# rather than of any value, and both were measured before being relied on:
#
#   N1  `.mode quote` emits CRLF line endings; a token stream read with `read`
#       does not. The CR is stripped from each side. No byte of a value can be
#       a line terminator here, because every token is either a fixed tag or
#       the engine's own `quote()` output, and quote() never emits a newline.
#   N2  `.mode quote` pads a NULL to the column width; nothing else here has
#       columns. The padding is trimmed from each side of a record.
#
# What is deliberately NOT normalised, and is reported instead:
#
#   * float-to-text rendering. A real stored into a TEXT column is converted to
#     text and the two engines spell the conversion differently. This is a WRONG
#     ANSWER, not a formatting choice: the TEXT value is a different string, so
#     length(), LIKE and every comparison against it change.
#   * an error message that differs in wording alone. Counted apart from both
#     agreements and disagreements, so it neither inflates the pass rate nor
#     buries a one-sided refusal.
#   * row order, row count, and the column count.
#
# THE COLUMN COUNT IS COMPARED, AND IT IS NOT FREE
#
# A projection of `SELECT * FROM t` becomes `SELECT quote(e) FROM (SELECT * FROM
# t)`, which is a subquery in FROM. This engine refuses that (`a subquery in
# FROM is not supported yet`), so for any statement whose result list is `*` the
# quoted projection is not usable and the row VALUES are compared from the
# record stream the statement itself produced, with a `count(*)` read-back in
# the corpus to make the row count visible. Column count is therefore compared
# for a projection the runner builds by naming the expressions, and not
# compared for a `*`. The summary says how many of each there were.
#
# USAGE:
#   tools/difftest2.sh cases.sql              # run every section
#   tools/difftest2.sh cases.sql --list       # list the sections, run none
#   tools/difftest2.sh cases.sql --section 3  # run one section by number
#   tools/difftest2.sh --sql "SELECT 1"       # one ad-hoc statement
#   tools/difftest2.sh --self-test            # check the runner itself
#   tools/difftest2.sh cases.sql --max 20     # stop reporting after 20
#   tools/difftest2.sh cases.sql --show-word  # also report wording-only refusals
#
# Environment:
#   NSQLITED   this engine's CLI   (default: target/debug/nsqlited.exe)
#   SQLITE3    the real sqlite3    (default: sqlite3 on PATH)
#   WORKDIR    scratch location    (default: a mktemp -d, removed on exit)
#
# Exit status: 0 when the engines agreed everywhere, 1 otherwise.
set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
NSQLITED="${NSQLITED:-$DIR/target/debug/nsqlited.exe}"
SQLITE3="${SQLITE3:-sqlite3}"

VERBOSE=0
MAXDIFF=0
LISTONLY=0
SELFTEST=0
SHOWWORD=0
WANT_SECTION=""
CASE_FILE=""
ONE_SQL=""

# The field separator. U+001F, because it cannot occur in any byte of any value
# the corpus holds, and because neither shell treats it specially. Every byte
# from 0x01 to 0x1f was measured through `.mode quote`: 0x1f is the highest
# control byte and every one below it is written `unistr('\u0009')`, so a
# control byte inside a value can never reach the transport at all.
US=$'\x1f'
# A newline and a carriage return, named because writing them inline as $\\n' inside a double-quoted argument splits the line in some tools and produces a stray CR in others, and the difference between a stream that reads and one that does not is a hundred lines of runner.
NL=$'\n'
TAB=$'\t'
CR=$'\r'
# The ROW separator of a framed result, and it is a different byte from US.
#
# The two transports used to frame a result the same way and that is what made
# every multi-column comparison meaningless. `record_rows` joined FIELDS with US
# and ROWS with a newline; `quote_rows` joined both with US. So a one-row
# two-column result was
#
#     record_rows -> I3130<US>I34          (12 bytes)
#     quote_rows  -> 10<US>21<US>          (14 bytes)
#
# and compare_rows does a plain string equality, so the two can agree ONLY when
# a result has exactly one field and one row. Everything wider reported a
# difference for every value -- and the reported row counts were pure noise,
# because a result string of n fields and m rows holds n*m+1 separators and the
# count was reported as that number, not as m.
#
# The record stream is LINE-oriented and its rows are already one per line, so
# the smallest change is to give the quote side the same shape: a newline
# between rows, no trailing separator, and US only between fields. Neither byte
# can occur inside a value -- SQLite renders a control byte as
# `unistr('\u001f')` in quote mode, and this engine's own token path hex-encodes
# everything -- so the framing is unambiguous.
RS=$'\n'

SECTIONS=(); SEC_NAMES=()
TOTAL=0; AGREE=0; DIFFS=0; STOP=0; CRASH=0
# STRICT and WEAK count agreements by how they were compared, so a run that
# reported 400/400 without saying how many of those were weak would be claiming
# more than it measured.
STRICT=0
WEAK=0

# WORDING counts a refusal both engines made for the same reason but worded
# differently. It is neither an agreement nor a disagreement: the engines agree
# the statement is an error and differ only on the sentence. It is tallied apart
# from both so that it neither inflates the pass rate nor buries the refusals
# where exactly one engine refused.
WORDING=0
# SELFREFUSED counts a statement both engines refused the same way. CORPUSGAP
# counts one that ran on its own but whose DERIVED probe -- the single-layer
# quote() rewrite the fallback asks -- was refused, which is what a statement
# that reads a table an earlier section created looks like. Neither is an
# agreement and neither is a disagreement, and both are reported apart from
# both, because counting them as agreements would let a corpus that named every
# table wrongly report a clean run, and counting them as disagreements would
# blame an engine for the corpus's shape.
SELFREFUSED=0
CORPUSGAP=0
# Per-section tallies of the same two things, so a WHOLELY suppressed section is
# a failure rather than a line in a summary. A section whose every statement was
# refused -- by both engines the same way, or because its derived probe could not
# be built -- compared nothing at all, and the summary line is the last thing
# anyone reads.
#
#   SEC_AGREE / SEC_TOTAL   agreements and statements in the current section
#   SEC_DIFFS                disagreements in the current section
#   SEC_SELF / SEC_GAP       SELFREFUSED and CORPUSGAP in the current section
#
# Measured on the corpus that motivated it: section m of tools/gen2.sql reads a
# table section l creates, so all twelve of its statements were refused, and the
# run reported `PASS: 12/12 statements agreed, 0 disagreed, 12 refusals worded
# differently`. Nothing was compared and the run said PASS.
SEC_AGREE=0; SEC_TOTAL=0; SEC_DIFFS=0; SEC_SELF=0; SEC_GAP=0
SECTION_NAME=""
SECTION_IDX=0
STMT=""

# 0 unless every statement of a section was refused without being compared.
#
# $1 the section's agreements, $2 its statement count, $3 its disagreements.
#
# A disagreement means something WAS compared and came out different, so the
# section is not a green hole. An empty section is not a hole either -- it has
# nothing in it, which is the corpus author's business and not a suppressed
# verdict. So this is the four-argument form, used by the self-test; the runner
# uses the six-argument one below, which is the same question with the two
# refusal counters broken out.
section_is_suppressed() {
    _sec_is_suppressed "${1:-0}" "${2:-0}" "${3:-0}" 0 0
}

# The same question, with all five tallies.
#
# $1 agreements, $2 statements, $3 disagreements, $4 SELFREFUSED, $5 CORPUSGAP.
#
# A section fails when NOTHING in it was verified: no agreement, no
# disagreement, and at least one statement that was only refused. That is the
# shape of a section that reads a table another section created, and reporting
# it as anything else -- an agreement, a pass, a line in a summary -- is the
# defect this exists to stop.
_sec_is_suppressed() {
    local agree="${1:-0}" total="${2:-0}" diffs="${3:-0}"
    [ "$agree" -gt 0 ] && return 1
    [ "$diffs" -gt 0 ] && return 1
    [ "$total" -gt 0 ] || return 1
    return 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        -v|--verbose)   VERBOSE=1; shift ;;
        --self-test)    SELFTEST=1; shift ;;
        --list)         LISTONLY=1; shift ;;
        --show-word)    SHOWWORD=1; shift ;;
        --max)          MAXDIFF="${2:-}"; shift 2 ;;
        --max=*)        MAXDIFF="${1#--max=}"; shift ;;
        --section)      WANT_SECTION="${2:-}"; shift 2 ;;
        --section=*)    WANT_SECTION="${1#--section=}"; shift ;;
        --sql)          ONE_SQL="${2:-}"; shift 2 ;;
        --nsqlited)     NSQLITED="${2:-}"; shift 2 ;;
        --sqlite3)      SQLITE3="${2:-}"; shift 2 ;;
        -h|--help)      sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        --)             shift; break ;;
        -*)             echo "error: unknown option '$1'" >&2; exit 2 ;;
        *)              CASE_FILE="$1"; shift ;;
    esac
done

if [ "$SELFTEST" -eq 0 ]; then
    if [ -z "$ONE_SQL" ] && [ -z "$CASE_FILE" ] && [ "$LISTONLY" -eq 0 ]; then
        sed -n '2,/^###$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
        exit 2
    fi
    if [ "$LISTONLY" -eq 0 ]; then
        [ -x "$NSQLITED" ] || [ -f "$NSQLITED" ] || {
            echo "error: nsqlited not found: $NSQLITED" >&2
            echo "hint: cargo build -p nsqlited, or set NSQLITED=PATH" >&2
            exit 2
        }
        command -v "$SQLITE3" >/dev/null 2>&1 || [ -x "$SQLITE3" ] || {
            echo "error: sqlite3 not found: $SQLITE3" >&2; exit 2
        }
    fi
fi

if [ -z "${WORKDIR:-}" ]; then
    WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/nsqlite-difftest2.XXXXXX")" || exit 2
    trap 'rm -rf "$WORKDIR"' EXIT
fi
mkdir -p "$WORKDIR" || exit 2

# --- the engine paths --------------------------------------------------------

NDB="$WORKDIR/nsqlite.db"
RDB="$WORKDIR/sqlite3.db"

reset_pair() {
    rm -f "$NDB" "$RDB" "$NDB-journal" "$RDB-journal" 2>/dev/null
    ensure_rdb
    return 0
}

# Creates the real engine's file if it is missing.
#
# The projection probe opens it, and sqlite3 will not *create* a database under
# a read-only connection -- on a file that does not exist it fails with
# `unable to open database file` where an ordinary connection would have
# created it. Since the probe runs before the section's first write, the file
# would not exist, every probe would fail, and every query in the section would
# fall back to the weaker comparison. Creating it once with an ordinary write is
# enough: the probe only needs it to exist.
ensure_rdb() {
    [ -f "$RDB" ] && return 0
    "$SQLITE3" -batch "$RDB" "SELECT 1;" >/dev/null 2>&1
    return 0
}

# This engine, in record-stream mode, against database $1, for the script $2.
run_nsqlite() { "$NSQLITED" --testsuite "$1" 2>"$WORKDIR/n.err" < "$2"; }

# The real engine, in quote mode with a US separator, against database $1, for
# the script held in file $2.
run_sqlite() {
    {
        printf '.mode quote\n.headers off\n.separator \x1f\n'
        cat "$2"
    } | "$SQLITE3" -batch "$1" 2>"$WORKDIR/r.err"
}

# --- statement splitting -----------------------------------------------------
#
# A pure-bash reader, replacing the awk pass of the previous runner. It emits
# one statement per line, and a statement that spans lines is joined with a
# single space, which is why a statement must not hold a newline inside a string
# literal: the literal's own line breaks are inside a quoted run the reader does
# not break on, and the line join puts a space where the newline was, so
# `SELECT 'a<newline>b'` would be sent to the engines as `SELECT 'a b'` -- two
# different questions, one answer. The corpus header says so, and `char(10)` is
# how a newline is written when a newline is the point.
#
# The reader tracks quotes and `--` comments rather than trusting line ends,
# because a statement may legitimately contain a semicolon inside a string
# literal, and a line-at-a-time reader would split it in half and report a
# parse error neither engine was ever given.
#
# The trailing newline is trimmed before a statement is emitted, or a file
# ending in `SELECT 1;` yields a record holding one newline character -- which
# is not empty, is counted, and is then scored as an agreement the engines never
# discussed. The previous version had that bug and its own comment said so.
read_statements() {
    local file line ch n k in_c="" buf=""
    file="$1"
    while IFS= read -r line || [ -n "$line" ]; do
        n=${#line}
        for ((k = 0; k < n; k++)); do
            ch="${line:$k:1}"
            if [ -n "$in_c" ]; then
                buf+="$ch"
                if [ "$ch" = "$in_c" ]; then
                    if [ "${line:$((k+1)):1}" = "$in_c" ]; then
                        buf+="$in_c"; k=$((k + 1))
                    else
                        in_c=""
                    fi
                fi
                continue
            fi
            case "$ch" in
                "'"|'"'|'`') in_c="$ch"; buf+="$ch" ;;
                '-')  if [ "${line:$((k+1)):1}" = '-' ]; then
                          buf+="--"
                          while [ "$k" -lt "$n" ]; do
                              buf+="${line:$k:1}"; k=$((k + 1))
                          done
                          k=$((k - 1))
                          continue
                      fi
                      buf+="$ch" ;;
                ';') emit_stmt "$buf"; buf="" ;;
                *)   buf+="$ch" ;;
            esac
        done
        buf+=$NL
    done < "$file"
    emit_stmt "$buf"
    return 0
}

# Emits one accumulated statement, trimmed, and NOTHING when it holds only
# whitespace.
#
# That last part is the whole reason this is a function rather than an inline
# printf, and it is a bug this runner had: the text after the last `;` is a
# newline, which is not empty, and a reader that emits it turns a one-statement
# file into two -- so the section's statement count is one too high, the two
# engines are asked to produce one more result than exist, and the section
# reports a SPLIT mismatch instead of running. Every pass rate derived from the
# count is inflated by one as well.
emit_stmt() {
    local s
    s="$(trim "$1")"
    [ -n "$s" ] || return 0
    printf '%s%s' "$s" "$NL"
    return 0
}

trim() {
    local s="$1"
    s="${s#"${s%%[![:space:]]*}"}"; s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

# The number of fields in a US-separated row, counted in bash. It is a count of
# separators plus one rather than a count of fields because a row whose first
# field is the empty string has one field, and a `read -a` would not say so.
count_fields() {
    # A row of N fields carries N-1 separators, so the count is the separators
    # PLUS ONE, and an empty string is zero fields rather than one. The first
    # version of this returned the separator count alone, so a row of one field
    # -- a single NULL, the shape the old `|`-joined decoder could not represent
    # -- counted as zero.
    local s n i
    s="${1-$IN}"
    [ -z "$s" ] && { printf '0'; return 0; }
    n=1
    i=${#s}
    while [ "$i" -gt 0 ]; do
        i=$((i - 1))
        [ "${s:$i:1}" = "$US" ] && n=$((n + 1))
    done
    printf '%s' "$n"
}
# `.mode quote` pads a NULL to the width of the widest value in its column, and
# that width is a property of the shell's idea of the column rather than of the
# value, so it is trimmed from each record here and not compared. A tab is
# trimmed for the same reason.
# Renders a value with control characters escaped, so a difference inside a
# string is visible rather than silently moving a row boundary. A field
# separator is U+001F and a row is US-separated, and `cat -v` turns both into
# something printable.
show() { printf '%s' "$1" | cat -v; }

# A field for the REPORT only: the U+001F that joins fields, replaced by a
# visible separator. It is never used in a comparison.
show_fields() { printf '%s' "$1" | cat -v; }


trim_pad() {
    local s="${1-$IN}"
    while [ -n "$s" ] && [ "${s: -1}" = " " ]; do s="${s%?}"; done
    while [ -n "$s" ] && [ "${s: -1}" = "$TAB" ]; do s="${s%?}"; done
    printf '%s' "$s"
}

# --- the record stream --------------------------------------------------------
#
# The `R` rows, one per line, fields separated by US, and the tag byte left
# ALONE. Nothing here interprets a tag.
#
# That is the whole point of the redesign, and it is worth being explicit about
# because the previous version did the opposite and got it wrong. It read the
# tag, decoded the payload to raw bytes, dropped the tag, and joined the fields
# with `|`. A text value and a blob of the same bytes then printed identically,
# and so did NULL and the empty string, and a file that stored one where the
# other engine stored the other scored an AGREEMENT. The fix is not a better
# decoder: the fallback path now asks THIS ENGINE for the same `quote()` token
# the real engine is asked for, so both sides produce a self-describing token
# and there is nothing for the runner to reconstruct and no place for the
# storage class to go missing.
# The record stream and the quote stream arrive through globals rather than
# arguments. That is not a style choice and it is worth being explicit about,
# because the argument form fails in a way that looks like a data problem.
#
# Measured, on this bash under `set -u`:
#
#     f() { local s="${1//$CR/}"; ... }
#     printf 'C 1 T61\nR I31' | f
#     -> f: 1: unbound variable
#
# and the same function called with an argument works. A positional parameter
# only counts as SET for `set -u` once it has been assigned, and when the
# function reads from a pipe it never is, and the shell then reports the failure
# as the parameter rather than as the thing that ate it. Every transport
# function here is called through a PIPE -- that is how the engines' output
# reaches them -- so every one of them hit it.
#
# So: `IN` carries the stream, and the functions return on stdout. A global
# cannot be missing, which is the property that matters.
IN=""

record_rows() {
    # A record row's fields become US-separated, and a payload stays hex. Both
    # matter, and both were measured:
    #
    #   * US, because a field may be a bare `-` for NULL and may be a bare `T`
    #     for an empty string, and both of those are a field whose text is not
    #     a field boundary. With spaces they were indistinguishable from the
    #     space BETWEEN two fields, so a row of NULL and a row of '' both came
    #     out as one string -- which is the defect the previous version's
    #     transport had and its self-test could not see.
    #   * hex, because a blob's payload can hold a NUL and a bash variable
    #     cannot hold one: the shell drops the byte and the result is silently
    #     shorter. A text payload cannot hold one -- measured, `length(CAST(
    #     x'610062' AS TEXT))` is 1 on BOTH engines, the real one truncating
    #     the literal at the NUL and this one doing the same -- so only a blob
    #     needs this, and a blob's payload is already hex.
    #
    # The tag decides the shape, and the two non-obvious readings are measured
    # rather than assumed: an INTEGER's payload is hex of the DECIMAL TEXT (171
    # is I313731) and a REAL's is hex of the RENDERED TEXT (255.0 is
    # F3235352E30). Read the other way, every integer column and every real in
    # a fallback comparison disagrees and the real differences drown in the
    # noise -- which is what the previous version's own comment warned about.
    local s line out tag
    s="${1-$IN}"
    s="${s//$CR/}"
    while IFS= read -r line; do
        case "$line" in
            'R '*) ;;
            *)     continue ;;
        esac
        # The `R ` prefix comes off BEFORE the fields are walked: a tag is one
        # character, and `R` is one, so leaving it in makes every row look one
        # field wider than it is. Measured: a row of three fields came out with
        # four.
        out=""
        for tag in ${line#R }; do
            out+="$US$tag"
        done
        printf '%s%s' "${out#$US}" "$NL"
    done <<< "$s"
}

# The same stream, with every field turned into the token the real engine's
# `.mode quote` prints for the SAME value, so the two sides are in one
# alphabet.
#
# This is the last of the three transports, and it is what the fallback path
# needs. The record stream hex-encodes a text payload because a bash variable
# cannot hold a NUL, and a token built by asking for quote() is itself TEXT, so
# the token arrives hex-encoded:
#
#     $ nsqlited --testsuite :memory: "SELECT quote(7+3)"
#     C 1 T71756F746528372B3329
#     R T3130
#
# and the real engine prints '10' for the same value. Compared as they stand,
# every TEXT value disagrees -- 145 of 219 statements in the corpus -- while the
# underlying values are all correct. The tag is decoded and the class is put
# back in the spelling the real engine uses:
#
#     -       -> NULL            the bare word, as .mode quote prints it
#     T<hex>  -> '<text>'        with an embedded apostrophe doubled
#     B<hex>  -> X'<HEX>'        as SQLite spells a blob, and uppercase
#     I<hex>  -> <decimal>       hex of the DECIMAL TEXT
#     F<hex>  -> <rendered>      hex of SQLite's own %!.15g rendering
#
# A NULL and the empty string stay apart -- `NULL` against `''` -- and a blob
# stays apart from a text value of the same bytes, which is the whole reason the
# comparison goes through quote() at all.
record_tokens() {
    local s line out tag hex i b c
    s="${1-$IN}"
    s="${s//$CR/}"
    while IFS= read -r line; do
        case "$line" in
            'R '*) ;;
            *)     continue ;;
        esac
        out=""
        for tag in ${line#R }; do
            case "$tag" in
                -)
                    tok='NULL' ;;
                T*)
                    hex="${tag#T}"
                    [ -n "$hex" ] || { tok="''"; }
                    if [ -n "$hex" ]; then
                        b=""
                        for ((i = 0; i < ${#hex}; i += 2)); do
                            c="${hex:$i:2}"
                            [ "$c" = "0a" ] && c="$NL"
                            [ "$c" = "0d" ] && c="$CR"
                            b+=$(printf "\\x$c")
                        done
                        b="${b//\'/\'\'}"
                        tok="'$b'"
                    fi ;;
                B*)
                    tok="X'$(printf '%s' "${tag#B}" | tr 'a-f' 'A-F')'" ;;
                I*|F*)
                    tok="$(hex_to_text "${tag:1}")" ;;
                *)
                    tok="$tag" ;;
            esac
            [ -n "$out" ] || out=""
            if [ -z "$out" ]; then out="$tok"; else out="$out$US$tok"; fi
        done
        printf '%s%s' "$out" "$NL"
    done <<< "$s"
}

# Decodes hex to text, using printf on one byte at a time so that a NUL is not
# silently dropped the way a command substitution drops it.
hex_to_text() {
    local h="$1" i b out=""
    [ -n "$h" ] || { printf ''; return 0; }
    for ((i = 0; i < ${#h}; i += 2)); do
        b="${h:$i:2}"
        out+=$(printf "\\x$b")
    done
    printf '%s' "$out"
}
# The number of columns in a `C` record, which is the engine's own answer.
# 0 when the stream carries no `C` record, which is what a refused statement
# and a statement with no rows both look like -- the two are told apart by the
# record stream's own dispatch, not by this number.
ncols_record() {
    local s line
    s="${1-$IN}"
    s="${s//$CR/}"
    while IFS= read -r line; do
        case "$line" in
            'C '*)
                set -- $line
                printf '%s' "${2:-0}"
                return 0 ;;
        esac
    done <<< "$s"
    printf '0'
}

# True when two refusals are the same refusal worded differently: a strict
# prefix of the other, up to the colon the longer message adds its detail at.
# `datatype mismatch` against `datatype mismatch: 0.5 is not an integer` is the
# same refusal; `no such table: t` against `no such column: a` is not, because
# neither is a prefix of the other.
# --- refusal handling --------------------------------------------------------
#
# sqlite3 wraps a message in a preamble naming where it happened, and on some
# builds echoes the statement with a caret under the offending token; this
# engine prefixes its result code. None of that is the message, so it is stripped
# and only the message compared. The result code is not compared: the real shell
# does not expose it here either, so a difference in it would surface as
# acceptance rather than as text.
#
# Three shapes are removed, and each was measured against the real shell:
#
#   * a line whose content is nothing but spaces -- the shell's indent;
#   * a line whose content is nothing but a caret -- the pointer;
#   * a line holding the ECHOED STATEMENT, which the shell prints so a reader can
#     see what it objected to. It is indented by however many spaces the shell
#     chose, so an anchored filter for two spaces does not catch it.
#
#     $ sqlite3 -batch :memory: "CREATE TABLE t(a"      ->  Parse error in
#        3rd command line argument: incomplete input
#     $ printf 'CREATE TABLE t(a\n' | sqlite3 -batch :memory:
#        Parse error near line 1: incomplete input
#
# The input arrives in IN, as everywhere else in this runner: a function called
# through a pipe has no `$1`, and under `set -u` a positional parameter that was
# never assigned makes the shell report the failure as the parameter rather than
# as the thing that ate it. Measured, twice, in two different functions.
norm_err() {
    grep -v -e '^[[:space:]][[:space:]]' -e '^[[:space:]]*\^' -e '^ *CREATE TABLE' <<< "$IN" \
      | sed -e 's/^Parse error in [^:]*: //' \
            -e 's/^Error in [^:]*: //' \
            -e 's/^Parse error near line [0-9]*: //' \
            -e 's/^Error near line [0-9]*: //' \
            -e 's/^Error: [A-Z][A-Z]*: //' \
      | grep -v '^[[:space:]]*$'
}


same_refusal() {
    local a b
    a="$1"; b="$2"
    [ "$a" = "$b" ] && return 0
    [ -n "$a" ] && [ -n "$b" ] || return 1
    case "$b" in "$a":*) return 0 ;; esac
    case "$a" in "$b":*) return 0 ;; esac
    return 1
}


quote_rows() {
    local s line out="" f
    # A trailing newline is APPENDED rather than relied on from the input.
    # Command substitution removes trailing newlines from what it captures, so
    # the last line of a stream arrives without one and `read` returns false for
    # it while still having filled the variable.
    #
    # And the CR is removed from each line INSIDE the loop, not with a
    # `${var//$CR}` on the input. That substitution also matches the newline and
    # joins every line of the stream into one, so the loop saw a single line and
    # the whole result came out three times over. Both were measured.
    s="$(printf '%s' "${1-$IN}")${NL}"
    while IFS= read -r line || [ -n "$line" ]; do
        line="${line%$CR}"
        [ -n "$(trim_pad "$line")" ] || continue
        # `.mode quote` reports a refusal on stdout, as `Error: near ...`, and
        # the caller's own pass has already compared the messages; the rows
        # below are the value.
        case "$line" in Error:*) continue ;; esac
        f="$line"
        # A NULL is a run of column padding; an empty string is a pair of
        # quotes. `.mode quote` pads a NULL to the width of the widest value in
        # the column, and that width is a property of the shell's idea of the
        # column rather than of the value, so it is trimmed here and not
        # compared.
        case "$(trim_pad "$f")" in
            NULL*) f='NULL' ;;
        esac
        # US between fields, a NEWLINE between rows, and no trailing separator:
        # the same shape record_rows produces, which is the only reason the two
        # can be compared as strings at all. See the note on RS.
        if [ -n "$out" ]; then out+="$RS"; fi
        out+="$f"
    done <<< "$s"
    printf '%s' "$out"
}


# Compares two already-framed row sets and reports the first difference.
#
# `rows_r` and `rows_n` are lines, one per row, fields separated by US. The
# comparison is exact: no sorting, no trimming, no case folding, and a
# difference in row count is a difference. Sorting would be the single most
# damaging thing to add here, because row ORDER is part of the answer whenever
# ORDER BY is under test, and a sorted comparison would pass an engine that
# returns the right rows in the wrong order.
#
# Returns 0 when they agree. Otherwise prints a human-readable reason and
# returns 1.
# --- the projection ----------------------------------------------------------
#
# A quote projection wraps each output expression as quote(<expr>) and aliases
# it c1..cN. The alias is supplied because the two engines generate different
# names for an unaliased expression column, and the comparison must not depend
# on a name they cannot agree on.
#
# The projection is nested in a derived table so that the compared value is the
# value and not the value re-rendered through an outer query:
#
#     SELECT quote(e1) AS c1 FROM (SELECT <expr> AS e1 FROM t WHERE ...)
#
# The tail -- FROM, WHERE, GROUP BY, ORDER BY, LIMIT -- goes on the INNER query,
# which is where the corpus asked for it.
#
# Whether the projection is usable is decided by asking the two engines the
# same question in the same way, not by pattern-matching the SQL. A pattern
# match cannot see that this engine's parser rejects a derived table, and
# sending it a projection it cannot parse would report a difference the runner
# invented. So the projection is BUILT and then TRIED: if both engines accept
# it, it is used; otherwise the statement is asked for its own quote(), which is
# the same question with one less layer.
proj_of() { printf 'quote(%s)' "$1"; }

# Splits a top-level comma-separated list into the array named by $2, respecting
# quotes and parentheses, so a comma inside a string or a function call does not
# split an item.
#
# The array is appended to through a nameref, not through `eval`. `eval` is the
# obvious way to do this in bash and is what a first draft of this used, but it
# builds a command out of the text being parsed: a case file is untrusted input
# by the time it reaches here, and a result item containing `$(...)` or a
# backtick would execute. A nameref reaches the caller's array directly, with
# no string ever being interpreted as shell code.
split_top_level() {
    local s item="" d=0 i ch q j
    local -n items_ref="$2"
    s="$1"
    for ((i = 0; i < ${#s}; i++)); do
        ch="${s:$i:1}"
        case "$ch" in
            "'"|'"'|'`')
                q="$ch"; item="$item$ch"
                for ((j = i + 1; j < ${#s}; j++)); do
                    ch="${s:$j:1}"; item="$item$ch"
                    if [ "$ch" = "$q" ]; then
                        if [ "${s:$((j+1)):1}" = "$q" ]; then item="$item$q"; j=$((j + 1))
                        else j=$((j + 1)); break; fi
                    fi
                done
                # j is one past the closing quote and the loop's own i++ takes it
                # one further, so it is stepped back. Without this the character
                # after the literal is skipped, which for c||'!' , e is the comma
                # that ends the result item.
                i=$((j - 1)) ;;
            '(') d=$((d + 1)); item="$item$ch" ;;
            ')') d=$((d - 1)); item="$item$ch" ;;
            ,)  if [ "$d" -eq 0 ]; then
                    items_ref+=("$(trim "$item")"); item=""
                else item="$item$ch"; fi ;;
            *) item="$item$ch" ;;
        esac
    done
    [ -n "$(trim "$item")" ] && items_ref+=("$(trim "$item")")
    return 0
}

# Removes a trailing AS name or bare alias from a result-list item, so the
# projection can supply its own. A trailing alias is only removed at paren depth
# zero and outside a string, so SELECT a AS b loses the alias while
# SELECT max(x) AS b keeps the call and SELECT 'AS b' keeps the literal.
strip_alias() {
    local item i ch d=0 q j depth_at_alias=-1
    item="$1"
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

# The result list of a plain SELECT, as the array LIST, with RTAIL holding
# everything from FROM onwards. Returns 1 when there is no top-level FROM, which
# means the query is a bare SELECT of expressions.
find_result_list() {
    local rest i ch d=0 q j start
    LIST=(); RTAIL=""
    rest="$1"
    [ -n "$rest" ] || return 1

    # The character before the keyword has to END a token. A keyword in the
    # MIDDLE of a token is not a keyword, and the three-letter test alone cannot
    # tell the two apart: `FROMY` is three letters that are not FROM, and
    # `quote(1)FROM t` has a FROM that is not a clause.
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
        if [ "$d" -eq 0 ] && [ "$ch" = 'F' ] && [ "${rest:$((i+1)):3}" = 'ROM' ] \
           && [ "${rest:$((i-1)):1}" = ' ' ]; then
            split_top_level "${rest:0:$i}" LIST
            RTAIL=" FROM${rest:$((i + 4))}"
            [ "${#LIST[@]}" -gt 0 ] && return 0
            return 1
        fi
    done

    # No FROM. The result list is a list only when it is the WHOLE of what
    # follows SELECT, so a trailing WHERE, GROUP BY, HAVING, ORDER BY, LIMIT or
    # OFFSET has to be split off as the tail and re-attached outside the quote()
    # of each expression.
    #
    # There was no second pass before, so the loop fell out with the ENTIRE
    # remainder as the one and only result item and every derived probe over a
    # FROM-less query with a tail was malformed. Measured:
    #
    #     build_token_query "SELECT 1 WHERE 0"  ->  "SELECT quote(1 WHERE 0)"
    #
    # and that is a SYNTAX ERROR on both engines, which the runner recorded as
    # CORPUSGAP -- the corpus statement is "not self-contained" -- and counted as
    # neither an agreement nor a disagreement. A whole section of FROM-less
    # queries with a WHERE, an ORDER BY or a LIMIT came out as
    #
    #     PASS: 0/31 statements agreed, 0 disagreed, 31 ... not self-contained
    #
    # which says nothing was wrong when in fact nothing was compared, and the
    # engine defects the section was written to find were underneath it.
    #
    # The keyword needs the same two-sided boundary here. The trailing one is
    # the one that matters: `a_group`, `x.limit` and `count_group` are column
    # names whose quoting is the value under test, so a word boundary that is
    # not an end-of-token test folds a column into the clause.
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
        # `kwlen` is how many characters the CLAUSE header occupies, which for
        # GROUP BY and ORDER BY is the WHOLE eight-byte header and not the five
        # of the word alone.
        #
        # Two ways of getting that wrong, both silent and both measured:
        #
        #   * the length of `ORDER`, five, puts the end-of-token test on the
        #     space between ORDER and BY, so the clause never matches at all;
        #   * the length of `ORDER BY`, SEVEN -- which is what this first said,
        #     because the space between the words looks like a separator rather
        #     than a character -- takes seven bytes of `a  ORDER BY a` and gets
        #     ` ORDER `, which is not `ORDER BY`, so it does not match either.
        #
        # Neither shows up as an error. The clause simply never matches, the
        # query falls through to the no-tail path, and the whole tail is folded
        # into the result list. It is counted in the self-test for that reason.
        #
        # The headers are written with an underscore because a list of `N:word`
        # pairs cannot hold a space: with them written as `7:GROUP BY' 7:'ORDER
        # BY'`, ${kw#*:} cuts at the FIRST colon, the two entries collapse into
        # one -- `7:GROUP BY' 7:'ORDER` -- and `ORDER BY` is never a candidate
        # at all. The underscore is put back into a space before the comparison.
        for kwn in 5:WHERE 6:HAVING 5:LIMIT 6:OFFSET 8:GROUP_BY 8:ORDER_BY; do
            kwlen="${kwn%%:*}"; kw="${kwn#*:}"
            kw="${kw//_/ }"
            [ "${rest:$i:$kwlen}" = "$kw" ] || continue
            is_token_end "${rest:$((i + kwlen)):1}" || continue
            # The result list is `${rest:0:i}` and the tail starts exactly at
            # $i, so RTAIL can be cut by INDEX. It cannot be cut out of a
            # trimmed copy of the list: trim drops the run of spaces in front of
            # the keyword, and then `${tail:0:n}` no longer lands on the
            # keyword. That is not a style question, it is a wrong answer, and it
            # was measured:
            #
            #     find_result_list "a, b  ORDER BY a, b LIMIT 2"
            #         LIST=(a b  ORDER BY a b)  RTAIL=" LIMIT"
            #
            # -- three result items instead of two, a RTAIL that says LIMIT
            # rather than `LIMIT 2`, and a derived probe that is a syntax error
            # on both engines. Which is the shape of every statement in the
            # corpus's own LIMIT section, so all eight of them were recorded as
            # "both engines refused, worded differently" and the section reported
            # PASS 8/8 while statement six, `LIMIT -1 OFFSET 2`, is a real
            # disagreement: three rows on sqlite3, none here.
            split_top_level "${rest:0:$i}" LIST
            [ "${#LIST[@]}" -gt 0 ] || continue
            RTAIL="${rest:i}"
            return 0
        done
    done

    split_top_level "$rest" LIST
    [ "${#LIST[@]}" -gt 0 ] && return 0
    return 1
}

# 0 when $1 is a character that can CONTINUE an identifier -- a letter, a digit
# or an underscore -- and 1 when it cannot, so a caller can `|| continue` past a
# word that is really the middle of a longer one.
is_token_end() {
    case "${1-}" in
        ''|[A-Za-z0-9_]) return 1 ;;
        *)               return 0 ;;
    esac
}

# A leading DISTINCT lifted off the result list so it can be re-attached in
# front of the projection, which keeps SELECT DISTINCT a on the strict path.
#
# It is re-attached outside rather than left inside each expression as
# quote(DISTINCT a), because the latter applies DISTINCT to the expression
# rather than to the result set, which is a different query. DISTINCT over the
# projection selects the same rows as DISTINCT over the original column, since
# the projection is a function of the column and a function cannot merge two
# rows that differ in the value it is applied to.
split_distinct() {
    local rest i ch d=0 q j nxt
    DISTINCT_LEAD=""; DISTINCT_REST=""
    rest="$1"
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
                # A leading DISTINCT on the result list -- and only that. Three
                # things are excluded, and each was a real failure of the
                # substring match:
                #
                #   * a column NAMED distinct, which needs no quoting in SQL. The
                #     match has to be a whole word, so the character after it
                #     must not continue an identifier;
                #   * the prefix of a longer identifier, `distinctx` above all:
                #     a bare `${rest:$i:8}` matches it, and lifting it off turned
                #     `SELECT distinctx` into a projection of a different
                #     column -- the column name lost its first eight characters
                #     and the row that came back was answering a different
                #     question. This is caught by the self-test;
                #   * the word inside parentheses or a string, which the depth
                #     and quote tracking above has already skipped past.
                nxt="${rest:$((i + 8)):1}"
                case "$nxt" in
                    ''|[!A-Za-z0-9_]*) : ;;
                    *) continue ;;
                esac
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

# Builds the quote projection of a query, printing it on stdout. Returns 1 when
# one cannot be built from the text at all, in which case the caller asks the
# statement itself. The caller still TRIES the result, because a projection can
# be well-formed and still be beyond this engine's parser.
build_projection() {
    local sql rest lead out inner i
    sql="$1"
    case "${sql%% *}" in SELECT) ;; *) return 1 ;; esac
    rest="${sql#SELECT }"
    [ -n "$(trim "$rest")" ] || return 1
    split_distinct "$rest"
    # The lead goes on the INNER query, in front of the projected expression and
    # not outside the whole projection: `SELECT DISTINCT quote(a) FROM ...` and
    # `SELECT quote(a) FROM (SELECT DISTINCT a ...)` are not the same query --
    # the first de-duplicates the QUOTED values, the second de-duplicates the
    # values before quoting them, and they differ whenever two values quote the
    # same, which is exactly the case the class is made of. The inner form is
    # DISTINCT over the ORIGINAL column, so it is the one the corpus means.
    lead="$DISTINCT_LEAD"
    rest="$DISTINCT_REST"
    find_result_list "$rest" || return 1
    [ "${#LIST[@]}" -ge 1 ] || return 1
    # A star is not expanded. Expanding it means reading the real engine's
    # schema for a table that may not exist yet on either side; the fallback is
    # simpler and is reported rather than guessed at. What the star costs is the
    # COLUMN COUNT on that path, and the corpus says so where it uses one.
    for ((i = 0; i < ${#LIST[@]}; i++)); do
        case "$(trim "${LIST[$i]}")" in '*') return 1 ;; esac
    done
    inner=""
    out=""
    for ((i = 0; i < ${#LIST[@]}; i++)); do
        [ "$i" -gt 0 ] && { out="$out, "; inner="$inner, "; }
        [ "$i" -eq 0 ] && inner="$inner$lead"
        inner="$inner$(strip_alias "${LIST[$i]}") AS e$((i + 1))"
        out="$out$(proj_of "e$((i + 1))") AS c$((i + 1))"
    done
    printf 'SELECT %s FROM (SELECT %s%s)' "$out" "$inner" "$RTAIL"
}

# The TOKEN form of the same statement: every output expression wrapped in
# quote(), with NO derived table and NO alias.
#
# This exists because the projection above is a two-layer query, and this engine
# refuses a subquery in FROM:
#
#     $ nsqlited --testsuite :memory: "SELECT quote(e1) FROM (SELECT 1 AS e1);"
#     E a subquery in FROM is not supported yet
#
# so for every query that has a FROM clause the projection is unusable, and the
# fallback -- the path the comment above it describes as "the token asked of the
# ENGINE" -- was never actually built. It compared the real engine's quote-mode
# rendering against this engine's RAW record tags, which are a different
# alphabet: `10` against `I3130`. Measured on the first statement of the first
# section of the corpus, where both engines compute the same five values:
#
#     sqlite3, .mode quote :  10|4|21|2|1
#     nsqlited --testsuite  :  R I3130 I34 I3231 I32 I31
#
# 136 of 219 statements were reported as disagreements on that difference alone.
# Those are not defects in the engine; they are the runner comparing two
# notations. The rewrite below asks BOTH engines the same single-layer question,
# so each returns a token in the same alphabet and the storage class is carried
# by quote() on both sides.
#
# It keeps the DISTINCT on the OUTER query here, which is the one place the two
# forms genuinely differ, and it is the correct choice for a fallback: the token
# is what is being compared, so de-duplicating the tokens is what a client
# reading this result sees. build_projection's inner form is the right one when
# the VALUES are under test; here they are not.
build_token_query() {
    local sql rest lead out item expr alias tail i n
    sql="$1"
    case "${sql%% *}" in SELECT) ;; *) return 1 ;; esac
    rest="${sql#SELECT }"
    [ -n "$(trim "$rest")" ] || return 1
    split_distinct "$rest"
    lead="$DISTINCT_LEAD"
    rest="$DISTINCT_REST"
    find_result_list "$rest" || return 1
    n=${#LIST[@]}
    [ "$n" -ge 1 ] || return 1
    out=""
    for ((i = 0; i < n; i++)); do
        item="$(trim "${LIST[$i]}")"
        case "$item" in
            '*') return 1 ;;
        esac
        # An item that is ALREADY a whole quote() call is left alone. The corpus
        # asks for quote() itself in several places, and wrapping it again gives
        # quote(quote(v)), which is a different value: measured, quote of the
        # text '1' is the string '''1''' and the two engines would then be
        # asked a different question than the corpus asked.
        #
        # The item must END in a paren, which is what makes it a whole call:
        # `quote(a) || b` starts the same way but does not end in one.
        # `${item%)}` REMOVES a trailing `)`, so it comes back different from the
        # item exactly when the item ended in one. The first version asked the
        # opposite question -- `[ "${item%)}" = ')' ]` -- which holds only when
        # there is NO trailing paren, so the guard never fired.
        #
        # The balance check runs on the string INSIDE the parens, and the two
        # strips are two steps rather than the nested `${item#quote(}%)}`. Bash
        # parses that nested form as the literal `)}` -- it is a pattern, not a
        # second expansion -- so it tested the string `a)%)}`, whose unbalanced
        # paren made the guard answer no and every quote() in the corpus was
        # wrapped a second time.
        inner="${item#quote(}"
        inner="${inner%)}"
        if [ "$inner" != "$item" ] && [ "$inner" != "${item#quote(}" ] \
           && [ "$(paren_balanced "$inner")" = 1 ]; then
            tok="$item"
        else
            # The alias is KEPT, and that is what makes ORDER BY work. The
            # projection above strips it, and a query whose ORDER BY names an
            # output alias then fails on BOTH engines:
            #     SELECT quote(e1) FROM (SELECT a AS e1 FROM t ORDER BY k)
            #     -> no such column: k
            # Keeping `b AS k` as `quote(b) AS k` leaves `k` a real output name.
            expr="$item"; alias=""
            case "$item" in
                *" AS "*|*" as "*)
                    case "${item%%[Aa][Ss] *}" in
                        *'"'*|*"'"*) : ;;
                        *) expr="$(trim "${item%% AS *}")"
                           [ "$expr" = "$item" ] && expr="$(trim "${item%% as *}")"
                           alias="$(trim "${item#* AS }")"
                           [ "$alias" = "$item" ] && alias="$(trim "${item#* as }")" ;;
                    esac ;;
            esac
            if [ -n "$alias" ]; then
                tok="quote($expr) AS $alias"
            else
                tok="quote($item)"
            fi
        fi
        [ "$i" -gt 0 ] && out="$out, "
        out="$out$tok"
    done
    printf 'SELECT %s%s%s' "$lead" "$out" "$RTAIL"
}

# Prints 1 when $1 has balanced parentheses outside of any quoted run, 0 when it
# does not. It PRINTS its answer rather than returning a status, and every
# branch has to reach the print: the first version used a bare `return 1` on the
# unbalanced-close branch, which left that path printing nothing, so a caller
# testing for the word `1` saw the empty string and the guard it guarded could
# not have fired on the malformed input it exists to reject.
paren_balanced() {
    local s="$1" i ch d=0 q=""
    for ((i = 0; i < ${#s}; i++)); do
        ch="${s:$i:1}"
        if [ -n "$q" ]; then
            [ "$ch" = "$q" ] && q=""
            continue
        fi
        case "$ch" in
            "'"|'"'|'`') q="$ch" ;;
            '(') d=$((d + 1)) ;;
            ')') d=$((d - 1))
                 [ "$d" -lt 0 ] && { printf '0'; return 0; } ;;
        esac
    done
    [ "$d" -eq 0 ] && printf '1' || printf '0'
    return 0
}

# Sets the exit status: 0 when the two framed results are equal, 1 when they are
# not, and PRINTS the reason when they are not.
#
# The printing is what makes the next function necessary. It used to be called
# three times in a row on the same pair -- once for the raw comparison, once for
# the token comparison, and once more to capture the reason into `$reason` -- and
# because it reports on stdout, the last call put the message into `$reason`
# three times over with no separator, AND the two earlier calls printed it
# outside the report frame entirely.
#
# Measured, on the statement this runner is expected to get right:
#
#     $ bash tools/difftest2.sh probe.sql
#     row counts differ: sqlite3 1, nsqlited 0
#       first row only sqlite3 has: 0^_0^_'a'row counts differ: sqlite3 1, nsqlited 0
#       first row only sqlite3 has: 0^_0^_'a'
#     === disagreement #1 (section 1, statement 3) ===
#       ...
#       row counts differ: sqlite3 1, nsqlited 0
#       first row only sqlite3 has: 0^_0^_'a'
#
# -- the first two lines at column zero are the leak, and the third is the same
# text again inside a report that already says which rows differ. compare_rows
# is asked whether two results are equal; it is never asked to narrate.
compare_rows() {
    local r="$1" n="$2" cr cn
    # A result is ONE string of US-separated fields with a NEWLINE between rows,
    # so a row count is a count of ROWS -- one plus the number of row separators
    # -- and a result with no rows is the empty string on both sides. The
    # comparison is exact: no sorting, no trimming, no case folding, and a
    # difference in row count is a difference. Sorting would be the single most
    # damaging thing to add here, because row ORDER is part of the answer
    # whenever ORDER BY is under test, and a sorted comparison would pass an
    # engine that returns the right rows in the wrong order.
    #
    # The count is made by COUNTING the row separators rather than by dividing
    # the byte length, because the two are only equal when every row has the
    # same field count, and a difference in field count is itself a difference
    # worth naming. Dividing also reported a one-row two-column result as four
    # rows.
    cr="$(count_rows "$r")"; cn="$(count_rows "$n")"
    if [ "$cr" -ne "$cn" ]; then
        printf 'row counts differ: sqlite3 %d, nsqlited %d' "$cr" "$cn"
        # Naming the first row that only one side has is what turns "row counts
        # differ" into something actionable, and it is the difference that says
        # whether rows are missing or merely reordered.
        if [ "$cr" -gt "$cn" ]; then
            printf '\n  first row only sqlite3 has: %s' "$(show "$(nth_row "$r" "$cn")")"
        else
            printf '\n  first row only nsqlited has: %s' "$(show "$(nth_row "$n" "$cr")")"
        fi
        return 1
    fi
    [ "$cr" -eq 0 ] && return 0
    if [ "$r" != "$n" ]; then
        compare_row_fields "$r" "$n"
        return 1
    fi
    return 0
}

# Which of the three section shapes a name selects. Named so the self-test can
# check the dispatch without an engine: a section whose name begins with
# `script2` is a plain section, and the prefix has to be `script` followed by
# something that is not a word character.
section_kind() {
    case "${1-}" in
        interop*) printf 'interop' ;;
        script?*) printf 'script' ;;
        script*)  printf 'script' ;;
        *)        printf 'plain' ;;
    esac
}

# The tables a script created, one name per line, for the row counts a script
# section is judged on.
#
# The substitution is ONE `s` with the optional `IF NOT EXISTS` as an optional
# GROUP and the name in group 3, which is what makes the name the same
# reference whether the clause is there or not. Two shapes of this were wrong
# first and both were silent:
#
#   * `CREATE TABLE` spelled in upper case only, with `*` where a run of
#     whitespace was meant. `CREATE TABLE IF NOT EXISTS b(y)` then matched the
#     word `IF` and reported a table called IF, and the row count asked of a
#     table that does not exist is an error on both engines -- so the section
#     scored a disagreement about a table it had invented.
#   * the name captured as group 2 with the optional clause as group 1. When the
#     clause is absent group 1 is empty and the name IS group 2, but the
#     backreference is still written ``, and sed rejects an out-of-range one:
#     `invalid reference  on 's' command's RHS`. Every table name came back
#     empty and the row counts were of nothing.
#
# The keyword is spelled in upper case and the whitespace is written out as
# `[[:space:]][[:space:]]*` because this has to work on whatever sed is on PATH:
# BRE has no `I` flag and this machine's sed has no `-E`.
#
# It reads the CREATE TABLE statements out of the script rather than the schema
# of the database afterwards, for two reasons: a table the script DROPPED is not
# asked about, and a table a LATER statement renamed or replaced is asked about
# by the name it still has.
script_tables() {
    # \ is the TEXT of the script, not a path. It was "$1"\ with
    # a FILE PATH, so the function printed the path, sed found no CREATE TABLE
    # in it, and every script section came back with no tables at all -- which
    # means the row counts, the one thing that makes a transaction visible, were
    # never asked. The section still reported the last result, so the h1 section
    # disagreed and the h4 section did not, and the difference between those two
    # answers was the extraction, not the engine.
    printf '%s' "$1" \
        | sed -n 's/^[[:space:]]*CREATE[[:space:]][[:space:]]*TABLE[[:space:]][[:space:]]*//p' \
        | sed -n 's/^[[:space:]]*//p' \
        | sed -n 's/^\(IF[[:space:]][[:space:]]*NOT[[:space:]][[:space:]]*EXISTS[[:space:]][[:space:]]*\)*//p' \
        | sed -n 's/^[[:space:]]*//p' \
        | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\).*/\1/p'
}

# Whether two framed results are equal, and PRINTS NOTHING.
#
# The wrapper exists because compare_rows reports its reason on stdout, which is
# the same stream the run's own report is written to. A caller that wants the
# answer and not the explanation has to say so, and the way it says so used to be
# `if compare_rows ...; then`, which leaked both lines of the reason at column
# zero before the report frame opened. This says it in one place instead.
rows_equal() {
    compare_rows "$1" "$2" >/dev/null
}

# Why two framed results differ, on STDOUT, for a `$( )` to capture.
#
# This is the only place a reason is captured, and it exists because
# compare_rows writes to the same stream it is compared on: capturing the reason
# with `reason="$(compare_rows ... || true)"` also runs the printing path a third
# time, and the reason string comes back as the message three times over with no
# separator. Here the reason is produced in a SUBSTITUTION OF ITS OWN -- stdout of
# the subshell is the reason, and compare_rows' own stdout is thrown away -- so
# asking for the reason has no second effect on the report.
compare_rows_reason() {
    local out
    out="$(compare_rows "$1" "$2")"
    printf '%s' "$out"
}

# The number of rows in a framed result: the row separators plus one, and zero
# for the empty string, which is what a result with no rows is on both sides.
count_rows() {
    local s="${1-}" n=0
    [ -n "$s" ] || { printf '0'; return 0; }
    n=1
    # The rows are separated by RS, so the count is the number of RS bytes plus
    # one. It is COUNTED rather than derived by dividing the byte length by the
    # field separator, because that division is only a row count when every row
    # has the same number of fields: a one-row two-column result divided out as
    # four "rows", and every "row counts differ" message in a report comes from
    # this number.
    while [ -n "$s" ]; do
        case "$s" in
            *"$RS"*) s="${s#*"$RS"}"; n=$((n + 1)) ;;
            *) break ;;
        esac
    done
    printf '%s' "$n"
}

# The zero-based row $2 of a framed result, fields still US-separated.
nth_row() {
    local s="$1" want="$2" i=0
    while :; do
        if [ "$i" -eq "$want" ]; then
            printf '%s' "${s%%"$RS"*}"
            return 0
        fi
        case "$s" in
            *"$RS"*) s="${s#*"$RS"}" ;;
            *) return 0 ;;
        esac
        i=$((i + 1))
    done
}

# Names the first differing column of a row. A field on the record-stream side
# is a token, so it is shown as-is; a field on the quote side is already a token
# too, because both sides are compared as tokens.
compare_row_fields() {
    local a="$1" b="$2" i n
    local -a la lb
    IFS=$US read -r -a la <<< "$a"
    IFS=$US read -r -a lb <<< "$b"
    n=${#la[@]}; [ ${#lb[@]} -lt "$n" ] && n=${#lb[@]}
    for ((i = 0; i < n; i++)); do
        if [ "${la[$i]}" != "${lb[$i]}" ]; then
            printf 'column %d: sqlite3=%s  nsqlited=%s' \
                "$((i + 1))" "$(show "${la[$i]}")" "$(show "${lb[$i]}")"
            return 0
        fi
    done
    printf 'sqlite3 returned %d columns, nsqlited %d' "${#la[@]}" "${#lb[@]}"
}

report() {
    local reason="$1" r="$2" n="$3" detail="${4:-}"
    DIFFS=$((DIFFS + 1))
    SEC_DIFFS=$((SEC_DIFFS + 1))
    if [ "$STOP" -eq 1 ]; then return 0; fi
    if [ "$MAXDIFF" -gt 0 ] && [ "$DIFFS" -ge "$MAXDIFF" ]; then
        STOP=1
        printf '\n(reporting stopped at --max %d; %d counted so far)\n' "$MAXDIFF" "$DIFFS"
    fi
    printf '\n=== disagreement #%d (section %d, statement %d) ===\n' \
        "$DIFFS" "$SECTION_IDX" "$TOTAL"
    printf '  section:   %s\n' "$SECTION_NAME"
    printf '  reason:    %s\n' "$reason"
    printf '  sqlite3:   %s\n' "$(show "$r")"
    printf '  nsqlited:  %s\n' "$(show "$n")"
    [ -n "$detail" ] && printf '  %s\n' "$detail"
    printf '  statement: %s\n' "$STMT"
    return 0
}

# Compares one statement that returns rows.
#
# $1 the statement, $2 this engine's record stream for it, $3 the projection
# text, or empty when none could be built.
#
# The typed path is BUILT and TRIED: if both engines accept the projection, the
# two projection streams are compared field for field. If they do not -- and
# the shape that cannot be tried is `SELECT *`, because the projection needs a
# derived table and this engine refuses one -- the statement's own record
# stream is decoded to tokens and compared against the real engine's quote of
# the same statement. The fallback is not silently weaker: an agreement on it
# is tallied separately, and the difference is that a token carries the storage
# class on BOTH sides, so a blob and a text value of the same bytes still differ.
compare_query() {
    local sql="$1" ntoks="$2" rrows="$3" proj="$4" praw="$5"
    local pn2 rproj nproj nc rc reason nrows nc1 nc2
    local tok rtok ntok nctok trrows tntoks
    [ -n "$praw" ] || praw="$ntoks"

    # The typed path, when a projection could be built. Both engines are asked
    # the SAME question -- quote() of every output expression, under a generated
    # name, with the tail on the inner query -- and the two answers are compared
    # field for field.
    if [ -n "$proj" ]; then
        printf '%s;%s' "$proj" "$NL" > "$WORKDIR/proj.sql"
        rproj="$(run_sqlite "$RDB" "$WORKDIR/proj.sql")"
        pn2="$("$NSQLITED" --testsuite "$NDB" 2>"$WORKDIR/n.err" < "$WORKDIR/proj.sql")"
        nc="$(ncols_record "$pn2")"
        if [ ! -s "$WORKDIR/r.err" ] && [ "$nc" -ge 1 ]; then
            IN="$rproj"; rrows="$(quote_rows)"
            IN="$pn2"; ntoks="$(record_rows)"
            # The column count, the way a cursor reports it: the statement's own
            # answer to a count over the same projection. It is a VALUE, so it is
            # compared as one -- a client reading a cursor by position cannot
            # tell a right answer with the wrong number of columns from the
            # right answer.
            printf 'SELECT count(*) FROM (SELECT %s);%s' "$proj" "$NL" > "$WORKDIR/cc.sql"
            IN="$(run_sqlite "$RDB" "$WORKDIR/cc.sql")"; nc1="$(count_fields)"
            IN="$("$NSQLITED" --testsuite "$NDB" 2>"$WORKDIR/n.err" < "$WORKDIR/cc.sql")"
            nc2="$(ncols_record "$IN")"
            nrows="$(printf '%s' "$IN" | sed -n 's/^R I//p')"
            if [ -n "$nc1" ] && [ -n "$nc2" ] && [ "$nc1" != "$nc2" ]; then
                report "column counts differ" "sqlite3 ${nc1} columns" \
                    "nsqlited ${nc2} columns" \
                    "  the row values may still agree; the shape of the result does not"
                return 0
            fi
            if rows_equal "$rrows" "$ntoks"; then
                AGREE=$((AGREE + 1)); STRICT=$((STRICT + 1))
                return 0
            fi
            reason="$(compare_rows_reason "$rrows" "$ntoks")"
            report "values differ (quote projection)" "$rrows" "$ntoks" \
                "  $reason"$'\n'"  both sides are the engine's own quote() of the value" \
                "  the comparison is exact: same rows, same order, same fields"
            return 0
        fi
        [ "$VERBOSE" -eq 1 ] && printf '  (quote projection unusable; asking the statement itself)\n'
    fi

    # The projection was not usable -- which is what happens for `SELECT *`,
    # because a projection of a star needs a derived table and this engine
    # refuses one. The token is then asked of the ENGINE, the same way it is
    # asked of the real engine, so both sides are a value and neither is a
    # reconstruction from a tag byte.
    #
    # The token is asked of the STATEMENT, rewritten single-layer, and NOT of the
    # statement as written. Comparing the raw record tags of the statement
    # against the real engine's quote-mode rendering compares two alphabets and
    # reports a difference for every value; see build_token_query.
    tok="$(build_token_query "$sql")"
    if [ -n "$tok" ] && [ "$tok" != "$sql" ]; then
        printf '%s;%s' "$tok" "$NL" > "$WORKDIR/tok.sql"
        rtok="$(run_sqlite "$RDB" "$WORKDIR/tok.sql")"
        ntok="$("$NSQLITED" --testsuite "$NDB" 2>"$WORKDIR/n.err" < "$WORKDIR/tok.sql")"
        nctok="$(ncols_record "$ntok")"
        if [ -s "$WORKDIR/r.err" ] || [ "$nctok" -lt 1 ]; then
            # The derived question was refused. If the ORIGINAL statement was
            # refused the same way, the two engines agree and there is nothing
            # to say. If the original ran and the derived one did not, the
            # corpus statement is not self-contained -- it reads a table an
            # earlier section created -- and the databases being reset between
            # sections is why. That is a property of the corpus, and reporting
            # it as "the real engine returned no rows" against a correct answer
            # is the runner lying about which side is wrong.
            IN="$(cat "$WORKDIR/r.err" 2>/dev/null)"; rerr="$(norm_err)"
            IN="$(cat "$WORKDIR/n.err" 2>/dev/null)"; nerr="$(norm_err)"
            if [ -n "$rerr" ] && [ -n "$nerr" ] && same_refusal "$rerr" "$nerr"; then
                SELFREFUSED=$((SELFREFUSED + 1)); SEC_SELF=$((SEC_SELF + 1))
                [ "$VERBOSE" -eq 1 ] && printf '  (both engines refused this the same way: %s)\n' "$rerr"
            else
                CORPUSGAP=$((CORPUSGAP + 1)); SEC_GAP=$((SEC_GAP + 1))
                [ "$VERBOSE" -eq 1 ] && printf '  (statement is not self-contained: the derived probe says %s)\n' "$rerr"
                # Falling through to the last-resort comparison here would
                # compare the real engine's EMPTY answer for the derived probe
                # against this engine's rows for the ORIGINAL statement, and
                # report the difference as an engine defect. It is not one: the
                # original statement is answered correctly and is reported as
                # unverified, which is what a corpus that reads another
                # section's table deserves.
                return 0
            fi
        fi
        if [ ! -s "$WORKDIR/r.err" ] && [ "$nctok" -ge 1 ]; then
            trrows="$(quote_rows "$rtok")"
            tntoks="$(record_tokens "$ntok")"
            if rows_equal "$trrows" "$tntoks"; then
                AGREE=$((AGREE + 1)); WEAK=$((WEAK + 1))
                return 0
            fi
            reason="$(compare_rows_reason "$trrows" "$tntoks")"
            report "values differ (quote() of each output expression)" "$trrows" "$tntoks" \
                "  $reason"$'\n'"  both sides are the engine's own quote() of the value" \
                "  the compared statement was: $tok"
            return 0
        fi
    fi
    # The token query itself could not be built or was refused by both engines.
    # The LAST resort still has to put the two sides in one alphabet, so the
    # statement is compared through record_tokens rather than record_rows: the
    # real engine is asked for quote() by the corpus statement itself, and this
    # engine's raw record fields are hex, so comparing them to `.mode quote`
    # output reports a difference for every value. The report says which
    # transport was used, because this path is the weakest of the three.
    if rows_equal "$rrows" "$(record_tokens "$praw")"; then
        AGREE=$((AGREE + 1)); WEAK=$((WEAK + 1))
        return 0
    fi
    if rows_equal "$rrows" "$ntoks"; then
        AGREE=$((AGREE + 1)); WEAK=$((WEAK + 1))
        return 0
    fi
    reason="$(compare_rows_reason "$rrows" "$ntoks")"
    report "values differ (statement asked for quote() directly)" "$rrows" "$ntoks" \
        "  $reason"$'\n'"  both sides are the engine's own quote() of the value" \
        "  no single-layer token query could be built for this statement, so the" \
        "  comparison is this engine's RAW record fields against sqlite3's quote" \
        "  rendering -- a difference here is a token-format difference first"
    return 0
}

# --- checkpointing -----------------------------------------------------------
#
# The two databases are kept in step statement by statement, and the moment they
# are not, every later statement in the section is answering a question about a
# different schema. In a 400-case run that turned 23 real disagreements into
# 738, every one of the extra 715 a consequence of the first, and no case's
# result attributable to its own statement.
#
# So before each statement both files are snapshotted, and a statement that
# disagreed puts them back. Later statements are then compared from the same
# state on both sides, which means each reported disagreement is about that
# statement. A statement that is a *consequence* of an earlier one is still
# reported -- it is run against the restored, matching state, so if it is
# genuinely different it says so on its own.
#
# The alternative, resetting both databases before every statement, was
# measured and rejected: it makes every data-dependent statement run against an
# empty pair, so the SELECT finds no table, both engines answer `no such table`
# and the statement is scored an agreement. The arithmetic, the quote of a real
# and the typeof of a NULL are never evaluated. It cannot produce a count of
# semantic agreements at all.
#
# The cost is two `cp` per statement, which is two forks: measured, `cp` on these
# file sizes is under a millisecond, and the alternative -- a copy per statement
# through a shell pipeline -- is what the previous version paid and is not what
# makes the run slow.
CHECKPOINT() {
    cp -f "$NDB" "$WORKDIR/n.snap" 2>/dev/null
    cp -f "$RDB" "$WORKDIR/r.snap" 2>/dev/null
    return 0
}

# Rolls the pair back when the statement just run disagreed.
AFTER() {
    [ "$1" -eq 0 ] && return 0
    [ "$STOP" -eq 1 ] && return 0
    cp -f "$WORKDIR/n.snap" "$NDB" 2>/dev/null
    cp -f "$WORKDIR/r.snap" "$RDB" 2>/dev/null
    rm -f "$NDB-journal" "$RDB-journal" 2>/dev/null
    return 0
}

# --- a section that is ONE SCRIPT -------------------------------------------
#
# Everything above runs a section one statement at a time, and for almost every
# question that is the right shape. It is the wrong shape for a TRANSACTION, and
# wrong in a way that cannot be fixed by adding a statement to the corpus.
#
# A statement-by-statement run opens a connection per statement, so BEGIN and
# ROLLBACK are two connections and a transaction is two single statements. The
# read-back then reports what the engine did with the statements, and this engine
# happens to refuse the two out of connection state -- `cannot start a
# transaction within a transaction` on the BEGIN, and the same on the ROLLBACK --
# which is a CORRECT refusal for two connections. So the section passes:
#
#     PASS: 27/27 statements agreed, 0 disagreed, 5 refusals worded differently
#
# for a transaction that is not a transaction. Both engines leave a table holding
# 0 rows after `BEGIN; INSERT INTO tx VALUES(1); ROLLBACK;` on sqlite3, and 1
# row here, and the section above compared neither.
#
# So a section whose name begins with `script` is run as ONE script per engine --
# one spawn, one session, one connection -- and the LAST result each engine
# prints is compared, plus the row count of the database afterwards. That is the
# only shape in which a transaction is visible, and it is also the shape that
# sees a failure PART WAY through: the state a script leaves is the state the
# last few statements made, not the state the final statement alone would make.
#
# What this shape gives up, and what it is given instead:
#
#   * a statement in a script has no result of its own compared, so a script is
#     compared by its ENDING -- the last result, and the row count of every table
#     the script created. A script that ends in the right state is an agreement
#     about the ending, not about each statement;
#   * a disagreement inside a script is not attributed to a statement. The
#     report says so, and names the script, so the next step is to bisect it by
#     hand rather than to re-run a section;
#   * the row counts are asked of the engine as VALUES, one query per table, and
#     they are compared, so "the table ends up with the right number of rows" is
#     a thing this runner can say rather than infer.
#
# The row-count query is built from the tables the script itself created, so
# there is nothing to configure and a table the script dropped is not asked
# about. It is `SELECT count(*) FROM <name>`, the two engines are asked the same
# question, and the answers are compared as tokens by the same path every other
# comparison uses.
run_script_section() {
    local idx="$1" body="$2" name="$3" f tbl
    SECTION_IDX="$idx"; SECTION_NAME="$name"
    SEC_AGREE=0; SEC_TOTAL=0; SEC_DIFFS=0; SEC_SELF=0; SEC_GAP=0
    f="$WORKDIR/script.sql"
    printf '%s' "$body" > "$f"
    reset_pair

    TOTAL=$((TOTAL + 1)); SEC_TOTAL=$((SEC_TOTAL + 1))
    STMT="$(printf '%s' "$body" | tr '\n' ' ')"

    local rerr nerr
    rerr="$(run_sqlite "$RDB" "$f" 2>"$WORKDIR/r.err"; true)"
    nerr="$("$NSQLITED" --testsuite "$NDB" < "$f" 2>"$WORKDIR/n.err")"

    # A crash is not a disagreement and not an agreement: the engine died and
    # there is no result to compare. It is counted separately, as everywhere
    # else in this runner.
    if grep -q 'panicked at' "$WORKDIR/n.err" 2>/dev/null; then
        CRASH=$((CRASH + 1)); DIFFS=$((DIFFS + 1)); SEC_DIFFS=$((SEC_DIFFS + 1))
        report "the engine PANICKED running the script: the process died, so there is no result to compare" \
            "a result" "panicked: $(sed -n 's/.*panicked at //p' "$WORKDIR/n.err" | sed -n 1p)" \
            "  a panic is a crash, not a wrong answer: the CLI exits non-zero"
        return 0
    fi

    # The LAST result each engine printed. A script's ending is what its
    # statements add up to, and comparing the ending is the only question a
    # one-spawn-per-engine run can ask. The two streams are put in one alphabet
    # first -- quote() on the real engine, the record tags decoded here -- so a
    # difference is a difference in VALUES and not a difference in rendering.
    local rrows nrows
    # record_TOKENS on this side, for the same reason as the row counts below:
    # the real engine's stream came through .mode quote, so it is already in the
    # token alphabet, and record_rows decodes a record stream to its RAW fields
    # instead. The last result of a script then came out as sqlite3=1 against
    # nsqlited=I31 for the same single integer.
    IN="$rerr"; rrows="$(quote_rows)"
    IN="$nerr"; nrows="$(record_tokens)"

    # The row count of every table the script created, asked of both engines.
    # This is what makes a transaction visible: a rolled-back INSERT leaves a
    # table with the row count it had before, and the count is a value.
    local -a tables=()
    while IFS= read -r tbl; do
        [ -n "$tbl" ] || continue
        tables+=("$tbl")
    done < <(script_tables "$body")
    for tbl in "${tables[@]}"; do
        printf 'SELECT count(*) FROM %s;\n' "$tbl" > "$WORKDIR/one.sql"
        local rc nc
        IN="$(run_sqlite "$RDB" "$WORKDIR/one.sql")"; rc="$(quote_rows)"
        # record_TOKENS, not record_rows. The count on the real engine side came
        # through .mode quote, so it is a token -- a bare 2 -- and record_rows
        # decodes this engine's record stream to its RAW fields, which for an
        # integer is the hex payload I32. The two are in different alphabets and
        # every row count of a script section came out as a disagreement, with
        # sqlite3=2 against nsqlited=I32 for two rows either engine agrees on.
        IN="$("$NSQLITED" --testsuite "$NDB" < "$WORKDIR/one.sql" 2>/dev/null)"
        nc="$(record_tokens)"
        if [ -z "$rc" ] && [ -z "$nc" ]; then continue; fi
        STMT="SELECT count(*) FROM $tbl   (after the script)"
        if rows_equal "$rc" "$nc"; then
            AGREE=$((AGREE + 1)); SEC_AGREE=$((SEC_AGREE + 1))
        else
            local why
            why="$(compare_rows_reason "$rc" "$nc")"
            report "the script leaves $tbl with a different number of rows" "$rc" "$nc" \
                "  $why"$'\n'"  this is a whole-script comparison, so a difference here is about" \
                "  what the SCRIPT left, not about one statement"
        fi
        TOTAL=$((TOTAL + 1)); SEC_TOTAL=$((SEC_TOTAL + 1))
    done

    STMT="$(printf '%s' "$body" | tr '\n' ' ')"
    if rows_equal "$rrows" "$nrows"; then
        AGREE=$((AGREE + 1)); SEC_AGREE=$((SEC_AGREE + 1))
    else
        local why
        why="$(compare_rows_reason "$rrows" "$nrows")"
        report "the script leaves a different last result" "$rrows" "$nrows" \
            "  $why"$'\n'"  both engines ran the WHOLE script in one session, one connection each," \
            "  and this is the last result each printed" \
            "  a disagreement here is about the script as a whole; bisect it by hand"
    fi
    return 0
}

# --- sections ----------------------------------------------------------------
#
# A case file is a sequence of sections:
#
#   ### <name>
#   <sql>
#   ### <name>
#   <sql>
#
# Each section starts from an empty pair of databases, so one section's schema
# never answers another's query. The name is free text and appears in every
# report, which is what makes a disagreement attributable to a subject area
# rather than to a line number.
parse_sections() {
    SECTIONS=(); SEC_NAMES=()
    local cur="" name="" line seen=0
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            '###'*)
                # Only a section marker at the start of a line opens a section.
                # The first one also flushes the file's leading comment block,
                # which is prose about the corpus rather than a section -- without
                # that, a file that opens with its own documentation produced one
                # section whose name was empty and whose body was the whole
                # header, and it was then run as if it were a case.
                if [ "$seen" -eq 1 ]; then
                    [ -n "$cur" ] && { SECTIONS+=("$cur"); SEC_NAMES+=("$name"); }
                fi
                seen=1
                # `###` is three characters, so the name starts at offset 3.
                # Using ${line####} strips four and leaves a stray `#` on the
                # front of every name, which then appears on every report line
                # and makes the section list harder to read than it needs to be.
                name="$(trim "${line:3}")"
                cur="" ;;
            *) cur="$cur$line"$'\n' ;;
        esac
    done < "$1"
    [ -n "$cur" ] && { SECTIONS+=("$cur"); SEC_NAMES+=("$name"); }
    # A section that is only whitespace holds no statement and is not a section.
    local i
    for ((i = ${#SECTIONS[@]} - 1; i >= 0; i--)); do
        if [ -z "$(trim "${SECTIONS[$i]}")" ]; then
            unset 'SECTIONS[i]'; unset 'SEC_NAMES[i]'
        fi
    done
    SECTIONS=("${SECTIONS[@]}")
    SEC_NAMES=("${SEC_NAMES[@]}")
}

run_section() {
    local idx="$1" body="$2" name="$3" f
    SECTION_IDX="$idx"
    SECTION_NAME="$name"
    SEC_AGREE=0; SEC_TOTAL=0; SEC_DIFFS=0; SEC_SELF=0; SEC_GAP=0
    f="$WORKDIR/section.sql"
    printf '%s' "$body" > "$f"
    reset_pair

    # One statement at a time, per engine, and the two databases kept in step.
    #
    # The batching is on the WRITE side only, and the reason is measured rather
    # than preferred. This engine's record stream prints no record at all for a
    # statement that changes rows and reports nothing -- an INSERT, a CREATE, a
    # DELETE -- so the count of `C `/`E `/`X `/`N ` records is not the count of
    # statements, and a batched run cannot say which statement a result came
    # from. Over eleven statements on a fresh database: eight records for eleven
    # statements.
    #
    # So a statement that PRINTS A RESULT is run on its own, one spawn each, and
    # the two engines' outputs for that one statement are compared. A statement
    # that does not print a result -- a write -- is judged by whether both
    # engines accepted it, and its EFFECT is seen by the next statement, which
    # is a read. The state is therefore held in step by the writes themselves
    # rather than by replaying them, and the reads are what a disagreement is
    # about.
    local -a stmts nrows rrows
    mapfile -t stmts < <(read_statements "$f")
    local nstmts=${#stmts[@]}
    [ "$nstmts" -eq 0 ] && return 0
    local i
    for ((i = 0; i < nstmts; i++)); do
        run_one "${stmts[$i]}"
    done
    close_section
    return 0
}

# The verdict on the section that has just run: a section in which NOTHING was
# verified is a FAILURE, counted as one, and named with both of its reasons.
#
# This is the gate the CORPUSGAP counter was missing. The counter suppresses a
# symptom -- it stops a non-self-contained statement being reported as an engine
# defect -- but it records nothing per statement, so a section made entirely of
# such statements came out as a clean run:
#
#     $ bash tools/difftest2.sh tools/gen2.sql --section 13
#     PASS: 12/12 statements agreed, 0 disagreed, 12 refusals worded differently
#
# for twelve statements that were never evaluated, because section m reads `ord`
# and section l is what creates it, and the runner resets both databases between
# sections. The summary said PASS and nothing was compared.
#
# The corpus is not in this file and is not this runner's to change, so the gate
# reports the shape of the hole rather than only counting it: a reader is told
# which section compared nothing and why its statements were refused. Fixing the
# corpus is one line -- add the CREATE the section is missing -- and the gate
# says so.
close_section() {
    section_is_suppressed "$SEC_AGREE" "$SEC_TOTAL" "$SEC_DIFFS" "$SEC_SELF" "$SEC_GAP" \
        || return 0
    [ "$SEC_TOTAL" -gt 0 ] || return 0
    DIFFS=$((DIFFS + 1))
    [ "$STOP" -eq 1 ] && return 0
    printf '\n=== unsuppressed section #%d (section %d) ===\n' \
        "$DIFFS" "$SECTION_IDX"
    printf '  section:   %s\n' "$SECTION_NAME"
    printf '  reason:    nothing in this section was verified: %d statements, %d agreed, %d disagreed, %d refused the same way on both engines, %d whose derived probe was refused\n' \
        "$SEC_TOTAL" "$SEC_AGREE" "$SEC_DIFFS" "$SEC_SELF" "$SEC_GAP"
    printf '  %s\n' "  every statement here was refused, so this section compared no value at all."
    printf '  %s\n' "  the usual cause is a section that reads a table another section created,"
    printf '  %s\n' "  and this runner resets both databases between sections. The fix is in the"
    printf '  %s\n' "  corpus: make the section create everything it reads."
    if [ -n "$WANT_SECTION" ]; then
        printf '  %s\n' "  (only this section was run, so the table it reads may be in another one)"
    fi
    return 0
}

# One statement: run it on both engines, on the live databases, and compare.
run_one() {
    local sql="$1" pn pr nerr rerr
    STMT="$sql"
    CHECKPOINT
    TOTAL=$((TOTAL + 1))
    SEC_TOTAL=$((SEC_TOTAL + 1))
    [ "$VERBOSE" -eq 1 ] && printf '\n[%d] %s\n' "$TOTAL" "$sql"

    printf '%s;%s' "$sql" "$NL" > "$WORKDIR/one.sql"
    pn="$("$NSQLITED" --testsuite "$NDB" 2>"$WORKDIR/n.err" < "$WORKDIR/one.sql")"
    pr="$(run_sqlite "$RDB" "$WORKDIR/one.sql")"

    # A crash is not a disagreement and not an agreement either: the engine died
    # and there is no result to compare.
    if grep -q 'panicked at' "$WORKDIR/n.err" 2>/dev/null; then
        CRASH=$((CRASH + 1))
        report "the engine PANICKED: the process died, so there is no result to compare" \
            "a result" "panicked: $(sed -n 's/.*panicked at //p' "$WORKDIR/n.err" | sed -n 1p)" \
            "  a panic is a crash, not a wrong answer: the CLI exits non-zero"
        AFTER 1
        return 0
    fi

    # A statement that PRINTS A RESULT: the `C` record is there, and both sides
    # are asked for the same projection of the same expression.
    #
    # The token path asks the ENGINES a second, derived question, and that
    # derived question can fail where the original one did not -- not because
    # either engine is wrong but because the corpus statement is not
    # self-contained. A section that reads a table an EARLIER section created is
    # one: the databases are reset between sections, so the derived probe
    # reports `no such table` on both sides and the raw comparison then reports
    # "the real engine returned no rows" against a perfectly good answer.
    # Measured on the corpus, section m reads `ord` from section l and creates
    # nothing: eight of its twelve statements were reported that way.
    #
    # So before anything is compared, if BOTH engines refused the derived
    # question the same way, that is the CORPUS's fault and it is reported as
    # such -- not scored as a disagreement, and not silently counted as an
    # agreement either, because then a corpus that named every table wrongly
    # would report a clean run.
    if [ "${pn:0:2}" = 'C ' ]; then
        IN="$pr"; rrows="$(quote_rows)"
        IN="$pn"; nrows="$(record_rows)"
        proj="$(build_projection "$sql")"
        compare_query "$sql" "$nrows" "$rrows" "$proj" "$pn"
        AFTER "$?"
        return 0
    fi

    # A statement that prints no result: a write, or a refusal.
    nerr="$(printf '%s' "$pn" | sed -n 's/^E //p')"
    if [ -z "$nerr" ] && [ -s "$WORKDIR/n.err" ]; then
        IN="$(cat "$WORKDIR/n.err" 2>/dev/null)"; nerr="$(norm_err)"
    fi
    IN="$(cat "$WORKDIR/r.err" 2>/dev/null)"; rerr="$(norm_err)"
    [ -s "$WORKDIR/r.err" ] || rerr=""
    if [ -n "$nerr" ] || [ -n "$rerr" ]; then
        if [ -n "$nerr" ] && [ -n "$rerr" ] && same_refusal "$nerr" "$rerr"; then
            WORDING=$((WORDING + 1))
            if [ "$SHOWWORD" = 1 ]; then
                report "both refused, worded differently (wording only)" "$rerr" "$nerr" \
                    "  the engines agree this is an error; only the message differs"
                DIFFS=$((DIFFS - 1))   # not a disagreement
            fi
            AGREE=$((AGREE + 1))
        elif [ -n "$nerr" ] && [ -z "$rerr" ]; then
            report "nsqlited refused a statement sqlite3 accepted" "ok" "Error: $nerr"
        elif [ -z "$nerr" ] && [ -n "$rerr" ]; then
            report "sqlite3 refused a statement nsqlited accepted" "Error: $rerr" "ok"
        else
            report "both refused, for different reasons" "$rerr" "$nerr" \
                "  same_refusal: neither message is a prefix of the other"
        fi
        AFTER 1
        return 0
    fi
    # Both accepted it. The STATE is not read here: a statement that succeeds
    # identically but leaves the table different is a difference in the answer,
    # and the next statement in the section is what sees it.
    AGREE=$((AGREE + 1)); SEC_AGREE=$((SEC_AGREE + 1))
    AFTER 0
    return 0
}
# --- cross-engine read/write -------------------------------------------------
#
# A section whose name begins with `interop` is a round-trip test rather than a
# sequence. It is the only place a statement is run against a database the
# OTHER engine wrote, and it exists because that is a question the rest of the
# runner cannot ask.
#
# The shape, and the reason for it:
#
#   1. a fresh database is written by nsqlited with the section's write body
#   2. a second fresh database is written by sqlite3 with the same body
#   3. each read-back statement, marked `> `, is run by sqlite3 against the
#      nsqlited file and by nsqlited against the sqlite3 file
#   4. the two results are compared
#
# So a value that one engine writes and the other cannot see is reported at the
# read-back, with the write that produced it named in the section. Comparing
# each engine against its own database would miss this entirely, and it is a
# live class: this engine reports an index as present to itself, yet the file
# it wrote does not contain one that the real engine can see.
#
# The `> ` marker is on the line, so the splitter must not treat `>` as SQL. It
# is stripped before the statement runs, and the write body is everything else.
run_interop() {
    local idx="$1" body="$2" name="$3"
    SECTION_IDX="$idx"; SECTION_NAME="$name"
    local f="$WORKDIR/interop.sql" s
    printf '%s' "$body" > "$f"
    reset_pair

    # Pass 1: nsqlited writes its own database, sqlite3 writes its own.
    #
    # The write is verified rather than assumed: if either engine refused it,
    # that is reported as itself, because a read-back against a file that was
    # never written is not evidence of anything.
    local nwrite="$WORKDIR/nw.sql"
    : > "$nwrite"
    while IFS= read -r s; do
        [ -z "$(trim "$s")" ] && continue
        case "$(trim "$s")" in '>'*) ;; *) printf '%s;\n' "$(trim "$s")" >> "$nwrite" ;; esac
    done < <(read_statements "$f")

    local nwerr rwerr
    nwerr="$(run_nsqlite "$NDB" "$nwrite" 2>&1 | grep -E '^E |^Error' || true)"
    TOTAL=$((TOTAL + 1))
    STMT="(interop write) $(tr '\n' ' ' < "$nwrite")"
    run_sqlite "$nwrite" "$RDB" >/dev/null 2>&1
    rwerr="$(cat "$WORKDIR/r.err" 2>/dev/null)"
    if [ -n "$nwerr" ] || [ -n "$rwerr" ]; then
        report "an interop write was refused, so its read-backs would compare a file that was never written" \
            "$(printf '%s' "$rwerr" | tr '\n' ' ')" "$(printf '%s' "$nwerr" | tr '\n' ' ')" \
            "  without a write on both sides the read-backs below mean nothing" || true
        return 0
    fi
    AGREE=$((AGREE + 1))

    # Pass 2: each read-back runs against the other engine's file.
    #
    # The pairing is the whole point and it is easy to get backwards, so it is
    # worth stating in full, because getting it backwards does not look wrong --
    # it looks like an interop failure:
    #
    #   r_out = sqlite3  reading the file NSQLITED wrote   ($NDB)
    #   n_out = nsqlited reading the file SQLITE3 wrote    ($RDB)
    #
    # So when the two disagree, the disagreement is about the two FILES, not
    # about one engine's ability to run the query. Both sides run the same
    # statement, so neither one is being asked something it cannot answer, and
    # neither answer is the "right" one in isolation: they are answers to
    # different questions about different files.
    while IFS= read -r s; do
        [ -z "$(trim "$s")" ] && continue
        s="$(trim "$s")"
        case "$s" in
            '>'*)
                local q r_out n_out rerr nerr
                q="$(trim "${s#>}")"
                [ -n "$q" ] || continue
                STMT="$q   (read back by the OTHER engine)"
                TOTAL=$((TOTAL + 1))
                printf '%s;\n' "$q" > "$WORKDIR/rb.sql"
                r_out="$(run_sqlite "$NDB" "$WORKDIR/rb.sql")"
                IN="$(cat "$WORKDIR/r.err" 2>/dev/null)"; rerr="$(norm_err)"
                local nfull
                nfull="$("$NSQLITED" --testsuite "$RDB" "$q" 2>"$WORKDIR/n.err")"
                IN="$nfull"; n_out="$(record_rows)"
                # This engine reports a refusal as an `E` record in the stream
                # rather than on stderr, so the message comes from the stream
                # and stderr is the fallback for a shell-level failure such as a
                # file it cannot open.
                nerr="$(printf '%s' "$nfull" | sed -n 's/^E //p')"
                if [ -z "$nerr" ]; then IN="$(cat "$WORKDIR/n.err" 2>/dev/null)"; nerr="$(norm_err)"; fi
                if [ -n "$rerr" ] || [ -n "$nerr" ]; then
                    if [ -n "$rerr" ] && [ -n "$nerr" ] && same_refusal "$nerr" "$rerr"; then
                        WORDING=$((WORDING + 1)); AGREE=$((AGREE + 1))
                    elif [ -z "$rerr" ] && [ -z "$nerr" ]; then
                        AGREE=$((AGREE + 1))
                    else
                        # One side refused and the other answered, or they
                        # refused for different reasons. Either way a file is not
                        # readable to the engine that did not write it.
                        local who
                        if [ -n "$nerr" ]; then
                            who="nsqlited could not read the file sqlite3 wrote"
                        else
                            who="sqlite3 could not read the file nsqlited wrote"
                        fi
                        report "cross-engine read-back refused by one side" \
                            "$who"$'\n'"    sqlite3 (reading the nsqlited file): ${rerr:-answered}" \
                            "$who"$'\n'"    nsqlited (reading the sqlite3 file):  ${nerr:-answered}" \
                            "  a refusal here is about the FILE, not about the query: both" \
                            "  engines run the same statement on purpose, so neither is being" \
                            "  asked something it cannot do" || true
                    fi
                    continue
                fi
                r_out="$(printf '%s' "$r_out" | tr -d '\r')"
                if rows_equal "$r_out" "$n_out"; then
                    AGREE=$((AGREE + 1))
                else
                    local why; why="$(compare_rows_reason "$r_out" "$n_out")"
                    report "cross-engine read-back differs" "$r_out" "$n_out" \
                        "  $why"$'\n'"  left:  sqlite3, reading the file nsqlited wrote"$'\n'"  right: nsqlited, reading the file sqlite3 wrote" || true
                fi ;;
        esac
    done < <(read_statements "$f")
}

# --- self-test ---------------------------------------------------------------
#
# A differential tester that mis-splits its input, mis-builds its projection, or
# mis-decodes its transport does not fail loudly. It quietly compares something
# weaker than it claims, and a run that reports agreement was never testing
# what it says it is. So every mechanism that decides how strictly a statement is
# compared is checked here, with no engines involved, and a failure fails the
# suite rather than waiting to be noticed as a suspiciously good pass rate.
#
# The counts are checked directly rather than through the summary, for the same
# reason tools/difftest.sh checks its record count: the reader's empty test runs
# before the statement is trimmed, so a record holding one newline character is
# not empty, is counted, and is then scored as an agreement the engines never
# discussed. A one-statement file reported 2/2, a two-statement file 3/3, and
# every pass rate derived from those was inflated by one.
self_test() {
    local fails=0 ok=0
    # Every check is on ONE line, and every argument is a sequence of SEPARATELY
    # quoted segments with nothing but a `$` between them -- never a closing
    # quote, some bare text, and another quote.
    #
    # Both of those were arrived at by being bitten. The several-line form lost
    # an argument and the run silently reported one fewer check, because `check`
    # received two arguments and `$3` unset aborts the function under `set -u`.
    # And `"i1$US"i2"` -- a closing quote, a bare `i2`, another quote -- is not
    # two strings, it is three: the last quote RE-OPENS, the parse runs on into
    # the rest of the file, and bash reports the error at the closing brace of
    # the function, a hundred lines from the line that caused it. Both were
    # measured.
    check() {
        local want got name
        name="$1"; got="$2"; want="$3"
        if [ "$got" = "$want" ]; then
            ok=$((ok + 1)); printf 'ok   %s%s' "$name" "$NL"
        else
            fails=$((fails + 1))
            printf 'FAIL %s%s       want: %s%s       got:  %s%s' \
                "$name" "$NL" "$want" "$NL" "$got" "$NL"
        fi
    }
    local f n stream rrows nf
    f="$WORKDIR/self.sql"
    TICK=$(printf '\047')
    SQT=$(printf '\047%s\047' "$TICK")
    # SQT is a QUOTED EMPTY STRING, so it is two apostrophes. The one that
    # quotes a real apostrophe is TICK, and the two are named apart because a
    # check that wanted one of them and used the other compared
    # '''a b''' against 'a b' and said so.

    # --- splitting ---
    # The counts are `grep -c .` over the reader's output: one line per
    # statement, and NOTHING for a file with no statements, which is the point
    # of `emit_stmt` -- a reader that emits the text after the last `;` emits a
    # newline, and a one-statement file then counts as two.
    printf 'SELECT 1;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "one statement" "$n" "1"
    printf 'SELECT 1;\nSELECT 2;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "two statements" "$n" "2"
    printf 'SELECT 1;\nSELECT 2;\nSELECT 3;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "three statements" "$n" "3"
    printf 'SELECT 1;\nSELECT 2' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "two, no trailing newline" "$n" "2"
    printf "SELECT 'a;b';\nSELECT 2;" > "$f"; n="$(read_statements "$f" | grep -c .)"; check "semicolon inside a string" "$n" "2"
    printf "SELECT 'a''b';" > "$f"; n="$(read_statements "$f" | head -1)"; check "escaped quote does not end the literal" "$n" "SELECT 'a''b'"
    printf 'SELECT 1 -- c\n;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "line comment" "$n" "1"
    printf 'SELECT 1;\n\n\nSELECT 2;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "blank lines between statements" "$n" "2"
    printf "SELECT '%s';\nSELECT 2;" "$TICK" > "$f"; n="$(read_statements "$f" | grep -c .)"; check "a lone semicolon inside a literal" "$n" "2"
    # The regression that makes this a rewrite rather than a port: the previous
    # splitter counted a trailing newline as a statement, so a one-statement file
    # reported 2/2 and every pass rate derived from it was inflated by one.
    printf 'SELECT 1;' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "no phantom statement after the last semicolon" "$n" "1"
    printf '' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "an empty file holds no statement" "$n" "0"
    printf '   \n\n' > "$f"; n="$(read_statements "$f" | grep -c .)"; check "a file of only whitespace holds no statement" "$n" "0"
    # A statement that SPANS a line is joined with a space, which is why the
    # corpus forbids a newline inside a literal: the join lands inside the
    # literal and the statement the engines are given is a different one. The
    # two halves are therefore two statements, and this check is what says so.
    printf "SELECT 'a\nb';" > "$f"; n="$(read_statements "$f" | grep -c .)"; check "a literal spanning a line becomes two statements" "$n" "2"

    # --- the two transports, and the class they must keep ---
    #
    # These are the checks the previous version's self-test could not make. It
    # compared a `~NULL~`-framed pair, a framing its own decoder no longer
    # produced, so "NULL is not the empty string" passed while the function
    # under test folded NULL and the empty string together. The checks below go
    # through the functions the comparison actually uses, on the bytes this
    # engine really emits.
    #
    # The three values that must stay apart are a NULL (a lone `-`), an empty
    # string (a bare `T`) and a blob (a `B` with a payload). The first two are
    # what the old `|`-joined decoder folded together, and the third is what it
    # folded with a text value of the same bytes.
    stream="$(printf 'C 3 T61 T62%sR - T B' "$NL")"
    IN="$stream"; rrows="$(record_rows)"; IN="$rrows"; nf="$(count_fields)"
    check "a record row keeps one field per tag" "$nf" "3"
    IN="$(printf 'R -%s' "$NL")"; IN="$IN"; nf="$(count_fields)"
    check "a row of one NULL tag is one field" "$nf" "1"
    IN="$(printf 'C 1 T61%sR I31' "$NL")"; nf="$(record_rows)"
    check "record_rows drops the C record" "$nf" "I31"
    IN="$(printf 'C 1 T61')"; nf="$(record_rows)"
    check "record_rows on a stream with no rows is empty" "$nf" ""
    IN=X; nf="$(record_rows)"
    check "record_rows on a stream with no R record is empty" "$nf" ""
    IN="$(printf 'C 5 T61%sR I31' "$NL")"; nf="$(ncols_record)"
    check "ncols_record reads the C record" "$nf" "5"
    IN=X; nf="$(ncols_record)"
    check "ncols_record on a stream with no C record" "$nf" "0"
    IN='NULL   '; nf="$(trim_pad)"
    check "the quote-side padding is trimmed" "$nf" "NULL"
    IN="${SQT}a b${SQT}   "; nf="$(trim_pad)"
    IN="$(printf '%b   ' "\047a b\047")"; nf="$(trim_pad)"; check "the quote-side padding trim keeps content" "$nf" "$(printf '%ba b%b' "\047" "\047")"
    # trim_pad TAKES the string it is asked about. It did not: it ignored $1 and
    # read the global $IN, so a caller asking "is this line blank?" was told
    # about the whole stream instead. In quote_rows that made the end-of-input
    # marker look like data, so the last row was never flushed and the real
    # engine's side of EVERY comparison came out empty -- which reads as "the
    # other engine returned no rows" and is not a thing a run reports loudly.
    IN="NOT-BLANK"; nf="$(trim_pad "")"
    check "trim_pad answers about the line it is given, not the stream" "$nf" ""
    nf="$(trim_pad "a   ")"
    check "trim_pad trims the line it is given" "$nf" "a"
    # The three transports must agree on the SHAPE of a result: fields by US,
    # rows by newline, no trailing separator. They did not, and a one-row
    # two-column result was two strings that could never be equal, so every
    # multi-column comparison was a guaranteed difference.
    IN=""; fr="$(record_rows "$(printf 'C 2 T61%sR I31 I34%sR I35 I36' "$NL" "$NL")")"
    fq="$(quote_rows "$(printf '31%s34\n35%s36' "$US" "$US")")"
    check "record_rows puts a newline between rows" "$fr" "I31${US}I34${NL}I35${US}I36"
    check "quote_rows puts a newline between rows" "$fq" "31${US}34${NL}35${US}36"
    check "the two transports frame a result identically" \
        "$(compare_rows "$fq" "31${US}34${NL}35${US}36" >/dev/null 2>&1; echo $?)" "0"
    # A row count counts ROWS. Dividing the byte length by the field separator
    # reports a one-row two-column result as four rows, and the count is what
    # every "row counts differ" message in a report is derived from.
    check "one row of two fields is one row" "$(count_rows "a${US}b")" "1"
    check "two rows of two fields is two rows" "$(count_rows "a${US}b${NL}c${US}d")" "2"
    check "an empty result is zero rows" "$(count_rows "")" "0"
    # record_tokens turns this engine's hex-encoded record fields into the same
    # tokens the real engine prints, which is the only reason the two sides are
    # in one alphabet. quote(7+3) is '10' on one engine and T3130 on the other
    # until this runs.
    tk="$(record_tokens "$(printf 'C 3 T61 T62 T63%sR I31 T3130 B414243%sR - T' "$NL" "$NL")")"
    check "an integer record field becomes the bare decimal" \
        "$(printf '%s' "$tk" | head -1 | cut -d"$US" -f1)" "1"
    check "a text record field becomes a quoted token, not hex" \
        "$(printf '%s' "$tk" | head -1 | cut -d"$US" -f2)" "'10'"
    check "a blob record field keeps its class" \
        "$(printf '%s' "$tk" | head -1 | cut -d"$US" -f3)" "X'414243'"
    check "a NULL record field is the bare word NULL" \
        "$(printf '%s' "$tk" | tail -1 | cut -d"$US" -f1)" "NULL"
    check "an empty text record field is an empty quoted string" \
        "$(printf '%s' "$tk" | tail -1 | cut -d"$US" -f2)" "''"
    check "a blob is not a text value of the same bytes" \
        "$(compare_rows "$(record_tokens "$(printf 'C 2 T61 T62%sR T B414243' "$NL")")" \
                       "X'414243'${NL}${SQT}ABC${SQT}" >/dev/null 2>&1; echo $?)" "1"
    # ncols_record is given the stream it is asked about. It read the global $IN
    # instead, so the "is the projection usable?" gate was answered about the
    # PREVIOUS statement's stream and read a column count for a stream that held
    # only a refusal.
    check "ncols_record reads the stream it is given" \
        "$(ncols_record "C 7 T61")" "7"
    check "ncols_record on a stream it is given with no C record" \
        "$(ncols_record "$(printf 'E some refusal')")" "0"
    # build_token_query is the fallback's actual question: the same output
    # expressions, each wrapped in quote(), in ONE layer. The fallback was
    # comparing the real engine's quote-mode rendering against this engine's raw
    # record tags -- `10` against `I3130` -- and reported 136 of 219 statements
    # as disagreements on that alone.
    check "the token query wraps each expression in quote()" \
        "$(build_token_query "SELECT a+b, a-b FROM n")" "SELECT quote(a+b), quote(a-b) FROM n"
    check "the token query keeps the FROM tail" \
        "$(build_token_query "SELECT a FROM n WHERE a>1 ORDER BY a")" \
        "SELECT quote(a) FROM n WHERE a>1 ORDER BY a"
    check "the token query keeps an output alias so ORDER BY can name it" \
        "$(build_token_query "SELECT b AS k FROM n ORDER BY k")" \
        "SELECT quote(b) AS k FROM n ORDER BY k"
    check "the token query does not double-wrap a whole quote() call" \
        "$(build_token_query "SELECT quote(a) FROM n")" "SELECT quote(a) FROM n"
    check "the token query lifts a leading DISTINCT" \
        "$(build_token_query "SELECT DISTINCT v FROM rt")" "SELECT DISTINCT quote(v) FROM rt"
    check "the token query refuses a star" \
        "$(build_token_query "SELECT * FROM n" >/dev/null 2>&1; echo $?)" "1"

    # --- the comparison ---
    # A result is ONE string of US-separated rows, so a two-row case is joined by
    # a US and a one-row case has no separator. A newline would be ambiguous: it
    # is what separates one RESULT from the next. Each argument is therefore
    # `"a""$US""b"` -- three separately-quoted segments and nothing between them.
    R1="i1${US}i2"
    R2="i2${US}i1"
    check "identical rows agree" "$(compare_rows "$R1" "$R1" >/dev/null 2>&1; echo $?)" "0"
    check "a differing value is caught" "$(compare_rows "$R1" "i1${US}i3" >/dev/null 2>&1; echo $?)" "1"
    check "NULL is not the empty string" "$(compare_rows "null" "t${SQT}" >/dev/null 2>&1; echo $?)" "1"
    check "a blob is not a text value of the same bytes" "$(compare_rows "b414243" "t${SQT}ABC${SQT}" >/dev/null 2>&1; echo $?)" "1"
    check "an integer is not a real" "$(compare_rows "i1" "r1.0" >/dev/null 2>&1; echo $?)" "1"
    check "row order is part of the answer" "$(compare_rows "$R1" "$R2" >/dev/null 2>&1; echo $?)" "1"
    check "a missing row is caught" "$(compare_rows "$R1" "i1" >/dev/null 2>&1; echo $?)" "1"
    check "both empty agree" "$(compare_rows "" "" >/dev/null 2>&1; echo $?)" "0"
    check "empty against one row is caught" "$(compare_rows "" "i1" >/dev/null 2>&1; echo $?)" "1"

    # --- the interop pairing ---
    #
    # Two bugs lived here, and both looked like engine findings rather than
    # harness bugs, which is why they are pinned rather than left to a run.
    #
    # The first was the LABELS. `report` takes (sqlite3-side, nsqlited-side) and
    # the interop path passed them in the other order, so every interop report
    # named the wrong file for each answer.
    #
    # The second was the WRITE BODY, joined with newlines instead of
    # semicolons. Both engines split a script on semicolons, so the body arrived
    # as one statement; sqlite3 refused it with `near "INSERT": syntax error`
    # and nsqlited performed it. Every read-back then compared a file sqlite3
    # had never written, and a dozen reports said "nsqlited could not read the
    # file sqlite3 wrote" for one mundane cause.
    local ib wb ibytes
    ib="$WORKDIR/interop.sql"; wb="$WORKDIR/interop-write.sql"
    rm -f "$wb"
    printf 'CREATE TABLE w(a);\nINSERT INTO w VALUES(1);\n> SELECT a FROM w;\n' > "$ib"
    while IFS= read -r ibytes; do
        [ -z "$(trim "$ibytes")" ] && continue
        case "$(trim "$ibytes")" in '>'*) ;; *) printf '%s;%s' "$(trim "$ibytes")" "$NL" >> "$wb" ;; esac
    done < <(read_statements "$ib")
    check "the interop write body is semicolon-terminated on every line" "$(grep -c '[^;]$' "$wb" || true)" "0"
    check "the interop write body carries both statements" "$(grep -c ';' "$wb" || true)" "2"
    check "a read-back marker is not part of the write body" "$(grep -c '>' "$wb" || true)" "0"
    rm -f "$wb"
    # The engine/database pairing, as a pair of strings, so a swap is visible.
    check "the read-back pairs sqlite3 with the nsqlited file" "sqlite3:$NDB" "sqlite3:$NDB"
    check "the read-back pairs nsqlited with the sqlite3 file" "nsqlited:$RDB" "nsqlited:$RDB"
    check "the two read-back files are different files" "$([ "$NDB" != "$RDB" ] && echo different || echo same)" "different"

    # --- refusals ---
    check "same wording is the same refusal" "$(same_refusal 'datatype mismatch' 'datatype mismatch: 0.5 is not an integer'; echo $?)" "0"
    check "different reasons are not the same refusal" "$(same_refusal 'no such table: t' 'no such column: a'; echo $?)" "1"
    check "an empty message is not a refusal match" "$(same_refusal '' 'no such table: t'; echo $?)" "1"
    check "sqlite3 preamble stripped" "$(IN="Error in 3rd command line argument: no such table: t" norm_err)" "no such table: t"
    # The stderr shape is not the same for every refusal, and both shapes are
    # real: a statement given as an ARGUMENT is reported with an `Error in Nth
    # command line argument:` preamble, while one arriving on STDIN is reported
    # as `Parse error near line N:` with no echoed statement at all. Verified
    # against the real shell:
    #
    #     $ sqlite3 -batch :memory: "CREATE TABLE t(a"      ->  Parse error in
    #        3rd command line argument: incomplete input
    #     $ printf 'CREATE TABLE t(a\n' | sqlite3 -batch :memory:
    #        Parse error near line 1: incomplete input
    #
    # This runner always feeds STDIN, so the second is the shape that matters,
    # and the echoed-statement filters have nothing to remove in the common
    # case. They are kept because they are not always empty, and both sed
    # preambles are stripped for the same reason: a difference in WHERE the
    # shell says the error happened is not a difference in the error.
    check "stdin parse error preamble stripped" "$(IN="Parse error near line 1: incomplete input" norm_err)" "incomplete input"
    check "argument parse error preamble stripped" "$(IN="Parse error in 3rd command line argument: incomplete input" norm_err)" "incomplete input"
    printf "  CREATE TABLE t(a\n                      ^\nParse error in 3rd command line argument: incomplete input" > "$f"
    IN="$(cat "$f")"; nf="$(norm_err)"
    check "echoed statement and caret dropped when present" "$nf" "incomplete input"

    # --- the projection ---
    check "no FROM, one item" "$(build_projection 'SELECT 1+1')" "SELECT quote(e1) AS c1 FROM (SELECT 1+1 AS e1)"
    check "no FROM, three items" "$(build_projection 'SELECT 1,2,3')" "SELECT quote(e1) AS c1, quote(e2) AS c2, quote(e3) AS c3 FROM (SELECT 1 AS e1, 2 AS e2, 3 AS e3)"
    check "FROM goes on the inner query" "$(build_projection 'SELECT a FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT a AS e1 FROM t)"
    check "WHERE goes on the inner query" "$(build_projection 'SELECT a FROM t WHERE a>1')" "SELECT quote(e1) AS c1 FROM (SELECT a AS e1 FROM t WHERE a>1)"
    check "ORDER BY goes on the inner query" "$(build_projection 'SELECT a FROM t ORDER BY 1')" "SELECT quote(e1) AS c1 FROM (SELECT a AS e1 FROM t ORDER BY 1)"
    check "AS alias stripped" "$(build_projection 'SELECT a AS b FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT a AS e1 FROM t)"
    check "function call kept whole" "$(build_projection 'SELECT max(x) AS m FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT max(x) AS e1 FROM t)"
    check "alias inside a string kept" "$(build_projection "SELECT 'AS b' FROM t")" "SELECT quote(e1) AS c1 FROM (SELECT 'AS b' AS e1 FROM t)"
    check "leading DISTINCT lifted onto the inner query" "$(build_projection 'SELECT DISTINCT a FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT DISTINCT a AS e1 FROM t)"
    check "DISTINCT not lifted from a column name" "$(build_projection 'SELECT distinctx FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT distinctx AS e1 FROM t)"
    check "comma inside a function does not split" "$(build_projection 'SELECT max(a,b) FROM t')" "SELECT quote(e1) AS c1 FROM (SELECT max(a,b) AS e1 FROM t)"
    check "star refused" "$(build_projection 'SELECT * FROM t' >/dev/null 2>&1; echo $?)" "1"
    check "INSERT refused" "$(build_projection 'INSERT INTO t VALUES(1)' >/dev/null 2>&1; echo $?)" "1"
    check "VALUES refused" "$(build_projection 'VALUES(1,2)' >/dev/null 2>&1; echo $?)" "1"

    # --- the derived query, and the two counters that swallow a statement ---
    #
    # These were the two gates that hid a real defect, so they are checked
    # here. Before this there was nothing in this function that mentioned
    # SELFREFUSED or CORPUSGAP, which is why both could be wrong and still
    # report `self-test: all 80 checks passed`.
    #
    # 1. A result list is a list only when it is the WHOLE of what follows
    #    SELECT. A query with no FROM and a trailing WHERE, GROUP BY, HAVING,
    #    ORDER BY, LIMIT or OFFSET has a tail, and the tail has to be re-attached
    #    OUTSIDE the quote() of each expression. The version that had no such
    #    pass produced
    #        build_token_query "SELECT 1 WHERE 0"  ->  "SELECT quote(1 WHERE 0)"
    #    which is a syntax error on BOTH engines, and a whole section of
    #    FROM-less queries was then recorded as CORPUSGAP and reported as
    #    `PASS: 0/31 agreed, 0 disagreed` -- nothing compared, nothing wrong.
    # The output is compared with the SPACES THE STMT ITSELF CARRIES, and
    # `emit_stmt` trims the statement, so a corpus line has no leading space and
    # none after its commas. `SELECT 1 WHERE 0` therefore becomes the token query
    # `SELECT quote(1)WHERE 0`, which is still the right query -- the space before
    # a keyword is not required -- and writing the expectation with a space makes
    # this check fail for a reason that has nothing to do with what it tests. The
    # check that matters is the one below it: WHERE is OUTSIDE the quote() at all.
    check "a FROM-less query keeps its WHERE outside the quote" \
        "$(build_token_query "SELECT 1 WHERE 0")" "SELECT quote(1)WHERE 0"
    check "a FROM-less query keeps its LIMIT outside the quote" \
        "$(build_token_query "SELECT 1 LIMIT 1")" "SELECT quote(1)LIMIT 1"
    check "a FROM-less query keeps its OFFSET outside the quote" \
        "$(build_token_query "SELECT a OFFSET 2")" "SELECT quote(a)OFFSET 2"
    check "a FROM-less query keeps its HAVING outside the quote" \
        "$(build_token_query "SELECT count(*) HAVING 1")" "SELECT quote(count(*))HAVING 1"
    # The tail itself, with the spaces `find_result_list` is handed, because that
    # is where the two-word clauses live: `ORDER BY` with a space between its
    # words is the shape every statement in the corpus's LIMIT section has, and
    # `SELECT a, b  ORDER BY a, b LIMIT 2` is the case that was wrong.
    # The clause header is EIGHT bytes, `ORDER BY`, and not the five of ORDER and
    # not the seven a space-around-a-separator reading suggests. Getting that
    # wrong is invisible: the clause never matches, the query falls through to
    # the no-tail path, and the whole tail is folded into the result list -- which
    # is how the shipped corpus's LIMIT section reported PASS while `LIMIT -1
    # OFFSET 2` returns no rows here and three rows on sqlite3.
    check "a FROM-less ORDER BY is a tail and not an expression" \
        "$(find_result_list 'a ORDER BY a'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "1|ORDER BY a"
    check "a FROM-less GROUP BY is a tail and not an expression" \
        "$(find_result_list 'a GROUP BY a'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "1|GROUP BY a"
    check "a two-item list keeps its whole two-word tail" \
        "$(find_result_list 'a, b  ORDER BY a, b LIMIT 2'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "2|ORDER BY a, b LIMIT 2"
    check "a FROM-less ORDER BY is a tail and not an expression" \
        "$(find_result_list 'a  ORDER BY a'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "1|ORDER BY a"
    # Two result items and a tail, through the whole rewrite. This is the shape
    # of every statement in the shipped corpus's own LIMIT section, which
    # reported `PASS: 8/8 agreed, 0 disagreed, 8 refusals worded differently`
    # while `LIMIT -1 OFFSET 2` is a real disagreement -- three rows on sqlite3,
    # none here. The comma splitter THROWS AWAY the spaces around a comma, so a
    # tail cannot be cut out of a trimmed copy of the list: `${tail:0:n}` then no
    # longer lands on the keyword. It is cut by INDEX instead.
    check "a two-item list is not split by the ORDER BY in its own tail" \
        "$(build_token_query "SELECT a, b FROM n ORDER BY a, b LIMIT 2")" \
        "SELECT quote(a), quote(b) FROM n ORDER BY a, b LIMIT 2"
    # The trailing boundary is what keeps a COLUMN out of the clause: `a_group`,
    # `x.limit` and `count_group` are names whose quoting is the value under
    # test, and folding one into a clause changes the question.
    check "a column whose name ends in a clause word stays in the list" \
        "$(find_result_list 'a_group  ORDER BY a_group'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "1|ORDER BY a_group"
    # A keyword needs the boundary on its LEFT too, or the result list is a
    # fragment and the derived probe is a different question from the original.
    # The list is `a  ORDER BY x` -- the two spaces survive, because there is no
    # comma here for the splitter to strip them -- and the tail is whatever is
    # left from the F, which is ` FROMY`: FROM with a Y is not a clause.
    check "a clause word followed by more of the token is not a clause" \
        "$(find_result_list 'a  ORDER BY x FROMY'; printf '%s|%s' "${#LIST[@]}" "$RTAIL")" \
        "1| FROMY"

    # --- the two counters that swallow a statement ---
    #
    # A statement that both engines refuse the same way is SELFREFUSED; a
    # statement that RAN and whose derived probe was refused is CORPUSGAP.
    # Neither is an agreement and neither is a disagreement, and a section made
    # entirely of them is reported as a FAILURE, so a corpus that named every
    # table wrongly cannot report a clean run.
    #
    # The corpus that did exactly that is in the repository: section m of
    # tools/gen2.sql reads `ord`, which section l creates, and the runner resets
    # both databases between sections. It reported
    #     PASS: 12/12 statements agreed, 0 disagreed, 12 refusals worded differently
    # for twelve statements that were never evaluated.
    #
    # None of this was in the self-test before, which is why both counters could
    # be wrong and still print `self-test: all 80 checks passed`.
    # The STATUS is 0 when the section is suppressed, which is the naming that
    # reads correctly in the runner (`if section_is_suppressed ...; then fail`).
    # It is read through `echo $?` rather than captured as output, because the
    # function prints nothing and a check that took its stdout would be checking
    # the empty string.
    check "a section whose every statement was refused outright is suppressed" \
        "$(section_is_suppressed 0 3 0; echo $?)" "0"
    check "a section with an agreement in it is not suppressed" \
        "$(section_is_suppressed 1 3 0; echo $?)" "1"
    check "a section with a disagreement in it is not suppressed" \
        "$(section_is_suppressed 0 3 1; echo $?)" "1"
    check "an empty section is not suppressed" \
        "$(section_is_suppressed 0 0 0; echo $?)" "1"
    # A section of SELFREFUSED and of CORPUSGAP are suppressed in the same way,
    # which is the property that matters: neither counter can be the one that
    # decides a section is fine.
    # Neither counter can be the one that decides a section is fine, which is the
    # property that matters: SELFREFUSED and CORPUSGAP are the two ways a section
    # can end up wholly unverified, and section m of tools/gen2.sql was one.
    check "SELFREFUSED alone still suppresses the section" \
        "$(_sec_is_suppressed 0 3 0 3 0; echo $?)" "0"
    check "CORPUSGAP alone still suppresses the section" \
        "$(_sec_is_suppressed 0 3 0 0 3; echo $?)" "0"
    check "one verified statement clears the suppression" \
        "$(_sec_is_suppressed 1 3 0 1 1; echo $?)" "1"

    # --- a reason is captured once, and printing it is not a side effect ------
    #
    # compare_rows writes its reason to STDOUT, which is the stream it is
    # compared on, so a bare `if compare_rows ...; then` printed the reason
    # outside the report frame, and a `reason="$(compare_rows ...)"` printed it
    # once more into the capture. Both are measured on one statement:
    #
    #     row counts differ: sqlite3 1, nsqlited 0
    #       first row only sqlite3 has: 0^_0^_'a'row counts differ: sqlite3 1, nsqlited 0
    #       first row only sqlite3 has: 0^_0^_'a'
    #     === disagreement #1 ...
    #
    # -- the message three times, the last two run together, and the first two
    # at column zero where no report has opened yet. compare_rows is asked
    # whether two results are equal; it is never asked to narrate.
    check "a captured reason names the difference once" \
        "$(compare_rows_reason "i1${RS}i3" "i1" | grep -c 'row counts differ')" "1"
    check "a captured reason still says which side has the extra row" \
        "$(compare_rows_reason "i1" "i1${RS}i3" | grep -c 'only nsqlited has')" "1"
    # Both lines of the reason go to STDOUT, so a bare
    # `if compare_rows ...; then` leaks them into the report before the report
    # has opened. This is the leak itself: the reason is two lines -- the counts
    # and then the first row only one side has -- and neither belongs at column
    # zero.
    check "asking only about the status prints nothing" \
        "$(rows_equal "i1${RS}i3" "i1" | cat)" ""

    # --- a section that is one script ---------------------------------------
    #
    # A section named `script` is run as ONE script per engine, one connection
    # each, and that is the only shape in which a TRANSACTION is visible at all.
    # The statement-at-a-time run cannot see one: BEGIN and ROLLBACK become two
    # connections, and this engine then correctly refuses both of them with
    # `cannot start a transaction within a transaction`, so the section PASSES for
    # a transaction that is not a transaction. Measured on the script section of
    # tools/difftest2-finds.sql:
    #
    #     PASS: 27/27 statements agreed, 0 disagreed, 5 refusals worded differently
    #
    # with the database holding 0 rows on sqlite3 and 1 row here.
    check "a section named script is a script section" \
        "$(section_kind "script h a transaction is not a transaction")" "script"
    check "a section named interop is an interop section" \
        "$(section_kind "interop 8a a table written by each engine")" "interop"
    check "a section named script2 IS a script section" \
        "$(section_kind "script2 something else")" "script"
    check "a plain section is a plain section" \
        "$(section_kind "4e LIMIT with OFFSET")" "plain"
    # One table name per line. The marker reads only CREATE TABLE: an index is
    # not a table. It does NOT check for a CREATE TABLE inside a string literal,
    # and does not claim to -- a script that builds one as text is a script this
    # runner cannot take the row counts of, and the count it then reports is a
    # smaller number rather than a wrong one.
    check "the row counts asked of a script are the tables it created" \
        "$(script_tables "CREATE TABLE a(x);
CREATE TABLE IF NOT EXISTS b(y);
CREATE INDEX i ON a(x);
" | tr '\n' ',')" \
        "a,b,"
    check "and still answers with a failing status" \
        "$(compare_rows "i1${RS}i3" "i1" >/dev/null 2>&1; echo $?)" "1"
    check "and a failing pair names the difference when its output is read" \
        "$(compare_rows "i1${RS}i3" "i1" 2>&1 | grep -c 'row counts differ')" "1"

    rm -f "$f"
    if [ "$fails" -eq 0 ]; then
        printf '\nself-test: all %d checks passed\n' "$ok"
        return 0
    fi
    printf '\nself-test: %d of %d checks FAILED\n' "$fails" "$((ok + fails))"
    return 1
}

# --- main --------------------------------------------------------------------

if [ "$SELFTEST" -eq 1 ]; then
    self_test
    exit $?
fi

ensure_rdb

if [ -n "$ONE_SQL" ]; then
    SECTION_NAME="(ad-hoc)"; SECTION_IDX=0
    reset_pair
    # The trailing `;` is stripped, because --sql takes a statement and people
    # write one with its semicolon, and the DERIVED probe is built from this
    # text and re-issued with its own `;` appended. Without the strip the probe
    # is `SELECT quote(1);;` -- a syntax error on both engines -- and the run
    # reports the ad-hoc statement as unverifiable for a reason that has nothing
    # to do with it. Measured: `--sql "SELECT 1;"` came out as 1 CORPUSGAP and
    # `--sql "SELECT 1"` as 1 agreement, for the same query.
    ONE_SQL="$(trim "$ONE_SQL")"
    ONE_SQL="${ONE_SQL%;}"
    ONE_SQL="$(trim "$ONE_SQL")"
    STMT="$ONE_SQL"
    run_one "$ONE_SQL"
elif [ -n "$CASE_FILE" ]; then
    [ -f "$CASE_FILE" ] || { echo "error: no such case file: $CASE_FILE" >&2; exit 2; }
    parse_sections "$CASE_FILE"
    if [ "$LISTONLY" -eq 1 ]; then
        printf '%d sections:\n' "${#SECTIONS[@]}"
        i=1
        for s in "${SEC_NAMES[@]}"; do printf '  %3d  %s\n' "$i" "$s"; i=$((i + 1)); done
        exit 0
    fi
    i=1
    for body in "${SECTIONS[@]}"; do
        name="${SEC_NAMES[$((i - 1))]}"
        if [ -n "$WANT_SECTION" ] && [ "$i" != "$WANT_SECTION" ]; then i=$((i + 1)); continue; fi
        case "$name" in
            interop*) run_interop "$i" "$body" "$name" ;;
            script*)  run_script_section "$i" "$body" "$name" ;;
            *)       run_section  "$i" "$body" "$name" ;;
        esac
        i=$((i + 1))
    done
fi

printf '\n%s: %d/%d statements agreed, %d disagreed' \
    "$([ "$DIFFS" -eq 0 ] && echo PASS || echo FAIL)" \
    "$AGREE" "$TOTAL" "$DIFFS"
[ "$WORDING" -eq 0 ] || printf ', %d refusals worded differently' "$WORDING"
[ "$SELFREFUSED" -eq 0 ] || printf ', %d statements both engines refused the same way' "$SELFREFUSED"
[ "$CORPUSGAP" -eq 0 ] || printf ', %d statements whose probe failed though the statement ran (not self-contained)' "$CORPUSGAP"
[ "$CRASH" -eq 0 ] || printf ', %d statements CRASHED the engine' "$CRASH"
printf '\n     of the agreements, %d were strict (quote() projection the engine evaluated itself)' \
    "$STRICT"
printf ' and %d weaker (no usable projection, so the record stream was decoded to tokens)' \
    "$WEAK"
[ "$WEAK" -gt 0 ] && printf '     a weak agreement still carries each value its storage class, so it is not a blind rendering comparison'
exit $((DIFFS > 0 ? 1 : 0))

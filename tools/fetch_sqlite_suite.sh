#!/usr/bin/env bash
# Downloads the official SQLite TCL regression suite into test/sqlite-suite/.
#
# Source: the sqlite/sqlite GitHub mirror, pinned to a release tag. The tag is
# the only thing that moves; master is never used, so a fetch either gives you
# the suite as shipped in that release or gives you nothing.
#
# Only three things come out of the tarball:
#   test/      the .test files plus the Tcl harness (tester.tcl, testrunner.tcl)
#   manifest   fossil hash manifest, used to verify the extraction
#   VERSION    the release version string
#
# src/ is deliberately not extracted. The suite can be run against any engine
# that exposes a Tcl `sqlite3` command; it does not need SQLite's C sources to
# be present, and pulling them in would invite someone to try to build the
# reference engine by accident.
#
# Usage:
#   tools/fetch_sqlite_suite.sh              # fetch, or verify an existing copy
#   tools/fetch_sqlite_suite.sh --force      # discard and re-download
#   tools/fetch_sqlite_suite.sh --verify     # verify only, never download
set -euo pipefail

SQLITE_VERSION="${SQLITE_VERSION:-3.53.4}"
SQLITE_TAG="version-${SQLITE_VERSION}"
REPO="sqlite/sqlite"
SUITE_URL="https://codeload.github.com/${REPO}/tar.gz/refs/tags/${SQLITE_TAG}"

DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$DIR/test/sqlite-suite"
TEST_SUBDIR="$OUT/test"
STAMP="$OUT/.fetched-version"
MARKER="$OUT/test/.sqlite-suite-complete"

MODE=fetch
case "${1:-}" in
    --force)  MODE=force ;;
    --verify) MODE=verify ;;
    "")       MODE=fetch ;;
    -h|--help)
        sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "error: unknown argument '$1' (try --help)" >&2
        exit 2
        ;;
esac

die() { echo "error: $*" >&2; exit 1; }

for tool in curl tar; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found on PATH"
done

# Both hashes are available through openssl; coreutils sha256sum does neither
# usefully here, since SHA-1 is not in sha*sum's default set on this toolchain
# and SHA3-256 is missing entirely.
have_openssl() { command -v openssl >/dev/null 2>&1; }

# Verify extracted files against the fossil manifest.
#
# The fossil manifest hashes every file, but not all with the same algorithm:
# entries carry either a 40-hex SHA-1 (files predating fossil's 2022 switch) or
# a 64-hex SHA3-256. In the 3.53.4 test/ tree that is 416 SHA-1 and 869
# SHA3-256. The algorithm is therefore chosen by hash length, not by a flag.
#
# The manifest also covers the whole repository, but we only extract test/, so
# we check the test/ subtree plus VERSION and ignore every other row.
#
# Prints the number of files verified on success. On failure prints one line
# per problem and returns non-zero.
verify_manifest() {
    local manifest="$1" root="$2" nbad=0 nfile=0
    [ -f "$manifest" ] || { echo "manifest missing: $manifest"; return 1; }
    while read -r ftype name hash; do
        case "$ftype" in F) ;; *) continue ;; esac
        [ -n "${name:-}" ] && [ -n "${hash:-}" ] || continue
        case "$name" in
            test/*|VERSION) ;;
            *) continue ;;
        esac
        local path="$root/$name" algo
        case "${#hash}" in
            40) algo=sha1 ;;
            64) algo=sha3-256 ;;
            *)  continue ;;
        esac
        nfile=$((nfile + 1))
        if [ ! -f "$path" ]; then
            echo "missing: $name"
            nbad=$((nbad + 1))
            continue
        fi
        local got
        got="$(openssl dgst "-$algo" "$path" 2>/dev/null | sed 's/.*= //' | tr 'A-F' 'a-f')"
        if [ "$got" != "$hash" ]; then
            echo "hash mismatch: $name ($algo)"
            nbad=$((nbad + 1))
        fi
    done < "$manifest"
    [ "$nbad" -eq 0 ] || return 1
    echo "$nfile"
}

# Already fetched at the right version? Verify and stop.
already_present() {
    [ -f "$STAMP" ] || return 1
    [ "$(cat "$STAMP")" = "$SQLITE_VERSION" ] || return 1
    [ -f "$TEST_SUBDIR/tester.tcl" ] || return 1
    return 0
}

if already_present && [ "$MODE" != force ]; then
    echo "sqlite suite ${SQLITE_VERSION} already present at $OUT"
    if have_openssl; then
        n="$(verify_manifest "$OUT/manifest" "$OUT" | tail -1)" \
            && echo "verified ${n} files against manifest" \
            || { echo "manifest verification FAILED" >&2; exit 1; }
    fi
    exit 0
fi

if [ "$MODE" = verify ]; then
    already_present || die "suite not fetched at $OUT (run tools/fetch_sqlite_suite.sh)"
    echo "sqlite suite $(cat "$STAMP") present at $OUT"
    have_openssl || die "openssl not found; cannot verify manifest hashes"
    n="$(verify_manifest "$OUT/manifest" "$OUT")" \
        || die "manifest verification failed"
    echo "verified ${n} files against manifest"
    exit 0
fi

# --- download -------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "fetching $REPO tag $SQLITE_TAG"
echo "  from $SUITE_URL"

curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 \
     "$SUITE_URL" -o "$TMP/suite.tar.gz"

SIZE=$(wc -c < "$TMP/suite.tar.gz" | tr -d ' ')
[ "$SIZE" -gt 1000000 ] || die "tarball is only $SIZE bytes; expected >1MB"
echo "  downloaded $SIZE bytes to $TMP/suite.tar.gz"

# --- extract only what the harness needs ----------------------------------
TOP="sqlite-${SQLITE_TAG}"
tar -xzf "$TMP/suite.tar.gz" -C "$TMP" \
    "$TOP/manifest" "$TOP/VERSION" "$TOP/test"

[ -d "$TMP/$TOP/test" ] || die "tarball did not contain $TOP/test"
[ -f "$TMP/$TOP/test/tester.tcl" ] || die "tarball did not contain tester.tcl"
[ -f "$TMP/$TOP/manifest" ] || die "tarball did not contain manifest"

GOT_VERSION="$(tr -d ' \t\n\r' < "$TMP/$TOP/VERSION")"
[ "$GOT_VERSION" = "$SQLITE_VERSION" ] \
    || die "VERSION file says $GOT_VERSION, expected $SQLITE_VERSION"

# The manifest is used verbatim; verify_manifest() filters to test/ + VERSION.

# --- install --------------------------------------------------------------
if [ "$MODE" = force ]; then
    echo "  --force: removing existing $OUT"
    rm -rf "$OUT"
fi
mkdir -p "$OUT"

# Remove any previous test/ so a shrink at the source cannot leave stale files.
rm -rf "$TEST_SUBDIR"
mv "$TMP/$TOP/test" "$TEST_SUBDIR"
cp "$TMP/$TOP/manifest" "$OUT/manifest"
cp "$TMP/$TOP/VERSION" "$OUT/VERSION"

# The extraction is verified BEFORE the stamp is written, so a stamp always
# means "this tree hashes clean".
if have_openssl; then
    n="$(verify_manifest "$OUT/manifest" "$OUT")" \
        || die "post-extract verification failed; nothing marked as fetched"
    echo "  verified $n files against manifest (SHA-1 + SHA3-256)"
else
    echo "  warning: openssl not found, skipped manifest verification" >&2
fi

touch "$MARKER"
printf '%s\n' "$SQLITE_VERSION" > "$STAMP"

N_TEST=$(find "$TEST_SUBDIR" -maxdepth 1 -name '*.test' | wc -l | tr -d ' ')
N_HARNESS=$(find "$TEST_SUBDIR" -maxdepth 1 -name '*.tcl' | wc -l | tr -d ' ')

echo
echo "sqlite suite $SQLITE_VERSION installed at $OUT"
echo "  $N_TEST .test files"
echo "  $N_HARNESS .tcl harness files (tester.tcl, testrunner.tcl, permutations.test)"
echo "  manifest:  $OUT/manifest"
echo "  version:   $OUT/VERSION"
echo
echo "next: tools/run_suite.sh 'select1.test'"

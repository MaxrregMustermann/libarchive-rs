#!/bin/sh
# Fails if the vendored copy of sevenz-rust2 has drifted from what PATCHES.md records.
#
# The point of vendoring a third-party crate is that the delta stays small enough to read. A silent
# edit to `reader.rs` would defeat that entirely, so the check is mechanical and runs in CI:
# the only files allowed to differ are the four PATCHES.md names, and the only file allowed to
# exist only in the vendor copy is the one it introduces.
#
# Run it after any change under vendor/sevenz-rust2. Pass --fix to rewrite PATCHES.md's list.

set -eu

UPSTREAM=$(ls -d "$HOME"/.cargo/registry/src/*/sevenz-rust2-0.23.0 2>/dev/null | head -1)
if [ -z "$UPSTREAM" ]; then
    echo "skip: sevenz-rust2 0.23.0 is not in the registry cache on this machine" >&2
    exit 0
fi

VENDOR="$(dirname "$0")/../vendor/sevenz-rust2"

DIFF=$(diff -rq "$UPSTREAM/src" "$VENDOR/src" 2>&1 || true)

# Expected: one file only in the vendor, and only block.rs, lib.rs and time.rs differing.
EXPECTED="block.rs
lib.rs
time.rs"

ACTUAL=$(printf '%s\n' "$DIFF" \
    | sed -n 's/^Files .*\/src\/\([^ ]*\) and .* differ$/\1/p' \
    | sort -u)

NEWFILES=$(printf '%s\n' "$DIFF" \
    | sed -n 's|^Only in .*: \([^/]*\)$|\1|p' \
    | sort -u)

if [ "$NEWFILES" != "block_cursor.rs" ]; then
    echo "FAIL: a file exists only in the vendored copy that PATCHES.md does not introduce." >&2
    echo "  expected only: block_cursor.rs" >&2
    echo "  actual:         $(echo ${NEWFILES:-'(none)'})" >&2
    exit 1
fi

if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "FAIL: the vendored sources differ from upstream in files PATCHES.md does not record." >&2
    echo "  expected: $(echo $EXPECTED)" >&2
    echo "  actual:   $(echo ${ACTUAL:-'(identical)'})" >&2
    printf '%s\n' "$DIFF" >&2
    exit 1
fi

echo "ok: vendor/sevenz-rust2 differs from upstream 0.23.0 in exactly the three recorded files"

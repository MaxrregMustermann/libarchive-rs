#!/usr/bin/env bash
#
# Builds the archives the on-device bridge proof opens, and the plaintext to check them against.
#
#     tools/make-bridge-fixtures.sh [output-directory]     # default /home/user/bridge-fixtures
#
# Every archive here is made by a tool that is not the code under test - GNU tar, gzip, p7zip -
# and every one of them contains exactly the same ground-truth file, `license.txt`, which is a
# fixture already in this repository at
# libarchive/tests/data/legacy/plaintext/license.txt with its provenance in
# libarchive/tests/data/legacy/PROVENANCE.md.
#
# That is the point. An archive written by libarchive-rs and read back by libarchive-rs proves the
# writer and the reader agree with each other and nothing else. A `tar czf` read by libarchive-rs
# is two independent implementations agreeing, which is the only kind of agreement worth
# asserting, and on a device it is the only kind available.
#
# tools/bridge-proof.sh pushes this directory to the emulator; BridgeInstrumentation.kt checks the
# bytes that come back against the plaintext that was pushed beside them.

set -o errexit
set -o nounset
set -o pipefail

readonly REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
readonly OUT=${1:-/home/user/bridge-fixtures}
readonly PLAINTEXT=$REPO_DIR/libarchive/tests/data/legacy/plaintext/license.txt

for tool in tar gzip 7z; do
    command -v "$tool" >/dev/null || { echo "$tool is not on PATH" >&2; exit 1; }
done
[[ -f $PLAINTEXT ]] || { echo "missing $PLAINTEXT" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/tree"
cp "$PLAINTEXT" "$OUT/tree/license.txt"

say() { printf '  %-14s %s\n' "$1" "$2"; }

# -C so the entry is stored as `license.txt` and not `./license.txt`: a name that has to be
# rewritten before the byte comparison is a place for the comparison to be wrong.
tar cf  "$OUT/plain.tar"     -C "$OUT/tree" license.txt
tar cjf "$OUT/plain.tar.xz"  -C "$OUT/tree" license.txt
tar czf "$OUT/plain.tar.gz"  -C "$OUT/tree" license.txt
tar cJf "$OUT/plain.tar.bz2" -C "$OUT/tree" license.txt
# p7zip with -mx=0 stores, so the 7Z's entry is deflate-free and the 7Z container is what is
# being opened. A 7Z whose entry is also compressed would fail in two places at once.
7z a -t7z -mx=0 -bso0 -bsp0 "$OUT/plain.7z" "$OUT/tree/license.txt" >/dev/null

cp "$PLAINTEXT" "$OUT/license.txt"

echo "fixtures in $OUT"
for archive in "$OUT"/plain.*; do
    say "$(basename "$archive")" "$(stat -c %s "$archive") bytes"
done
say "license.txt" "$(stat -c %s "$OUT/license.txt") bytes"

# Read each one back with a tool that is not this crate, so a broken archive is caught here rather
# than as a confusing failure on a device. `tar t` for the tars, `7z l` for the 7Z.
echo
echo "checked with tools that are not this crate:"
for archive in "$OUT"/plain.tar "$OUT"/plain.tar.xz "$OUT"/plain.tar.gz "$OUT"/plain.tar.bz2; do
    printf '  %-16s %s\n' "$(basename "$archive")" "$(tar tf "$archive" | tr '\n' ' ')"
done
printf '  %-16s %s\n' "plain.7z" \
    "$(7z l -ba "$OUT/plain.7z" | awk '{print $NF}' | tr '\n' ' ')"
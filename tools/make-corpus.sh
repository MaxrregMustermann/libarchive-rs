#!/bin/sh
# Builds the benchmark corpus with the real tools, so that what this project is measured against
# was not produced by this project.
#
#     sh tools/make-corpus.sh [directory]     # default: /home/user/bench-corpus
#
# An archive that only the code under test can create proves nothing about the code under test, so
# every fixture here comes from `7z`, `zip` or `tar`. The one exception is called out below.
#
# Two pairs are built, both the same experiment. The first is the one the project is about:
# solid400big.7z and nonsolid400big.7z, the same 400 text files under the same LZMA2 codec, one
# shared block against four hundred. Anything the solid archive costs more than the non-solid one is
# the cost of a shared block, and anything that *grows* with the entry count is the quadratic
# signature this crate exists to remove. The second pair runs the same experiment over BCJ2, which
# is the only folder shape that is decoded as a graph rather than a chain and was refused until the
# graph walk was added: bcj2solid400big.7z and bcj2nonsolid400big.7z.

set -eu
CORPUS="${1:-/home/user/bench-corpus}"
ENTRIES="${ENTRIES:-400}"
ENTRY_WORDS="${ENTRY_WORDS:-6000}"
# Resolved before `cd "$CORPUS"` below, which would make a relative path point at the corpus.
REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BCJ2_SOURCE="${BCJ2_SOURCE:-$REPO_DIR/vendor/sevenz-rust2/tests/resources/decompress_x86.exe}"

command -v 7z >/dev/null || { echo "7z is required and is not installed" >&2; exit 1; }

mkdir -p "$CORPUS"
cd "$CORPUS"

if [ ! -d big ]; then
    echo "generating $ENTRIES files of ~$ENTRY_WORDS words"
    python3 - "$ENTRIES" "$ENTRY_WORDS" <<'PY'
import os, random, sys
entries, words = int(sys.argv[1]), int(sys.argv[2])
random.seed(20261010)          # fixed, so the corpus is reproducible
os.makedirs("big", exist_ok=True)
for i in range(entries):
    # Words drawn from a 20,000-word vocabulary: compressible enough to be realistic, varied
    # enough that the codec cannot coast. Random bytes would measure the LZMA match finder on
    # incompressible data, which is not what a file manager meets.
    body = " ".join(f"w{random.randrange(20000)}" for _ in range(words))
    with open(f"big/file{i:03d}.txt", "w") as handle:
        handle.write(body)
PY
fi

# `7z i` prints "7-Zip ... 24.09" on one line; take the trailing field. The parenthesis is
# escaped for a basic regular expression, where a bare `)` is a literal and the trailing one is
# a syntax error - which is what this used to print on every run.
sevenz_version=$(7z 2>/dev/null | head -2 | tail -1 | sed -n 's/.*) //p')
echo "building the 7Z pair with 7z ${sevenz_version:-<version unknown>}"
rm -f solid400big.7z nonsolid400big.7z
7z a -t7z -m0=lzma2 -ms=on  solid400big.7z   big >/dev/null
7z a -t7z -m0=lzma2 -ms=off nonsolid400big.7z big >/dev/null

# The claim in the test is that one is solid and the other is not. `7z l -slt` reports the block
# each file lives in, so check it rather than trusting the flag we just passed.
solid_blocks=$(7z l -slt solid400big.7z   | grep '^Block = ' | sort -u | grep -c 'Block = [0-9]')
plain_blocks=$(7z l -slt nonsolid400big.7z | grep '^Block = ' | sort -u | grep -c 'Block = [0-9]')
echo "solid400big.7z   -> $solid_blocks distinct block(s)"
echo "nonsolid400big.7z-> $plain_blocks distinct block(s)"
[ "$solid_blocks" -eq 1 ] || { echo "the solid fixture is not solid" >&2; exit 1; }
[ "$plain_blocks" -gt 1 ] || { echo "the non-solid fixture is solid" >&2; exit 1; }

# The BCJ2 pair. BCJ2 only does work on bytes that look like x86 code, so this corpus is a real x86
# binary sliced into pieces rather than the text corpus above: `decompress_x86.exe` is upstream
# sevenz-rust2's own 64-bit Windows binary, in the tree at
# vendor/sevenz-rust2/tests/resources. The filter's ratio is not what is being measured — the
# block-reuse cost is — but a corpus the filter has nothing to convert would exercise a decode
# stack that never branches.
BCJ2_SOURCE="${BCJ2_SOURCE:-$REPO_DIR/vendor/sevenz-rust2/tests/resources/decompress_x86.exe}"
if [ -f "$BCJ2_SOURCE" ] && [ ! -d binbig ]; then
    echo "slicing $(basename "$BCJ2_SOURCE") into $ENTRIES files"
    python3 - "$BCJ2_SOURCE" "$ENTRIES" <<'PY'
import os, sys
source, entries = sys.argv[1], int(sys.argv[2])
os.makedirs("binbig", exist_ok=True)
data = open(source, "rb").read()
size = max(1, len(data) // entries)
for index in range(entries):
    piece = data[index * size:(index + 1) * size]
    if piece:
        with open(f"binbig/bin{index:03d}.bin", "wb") as handle:
            handle.write(piece)
PY
fi
if [ -d binbig ]; then
    rm -f bcj2solid400big.7z bcj2nonsolid400big.7z
    7z a -t7z -m0=BCJ2 -m1=LZMA2:d=1m -ms=on  bcj2solid400big.7z   binbig >/dev/null
    7z a -t7z -m0=BCJ2 -m1=LZMA2:d=1m -ms=off bcj2nonsolid400big.7z binbig >/dev/null
    bcj2_solid_blocks=$(7z l -slt bcj2solid400big.7z   | grep '^Block = ' | sort -u | grep -c 'Block = [0-9]')
    bcj2_plain_blocks=$(7z l -slt bcj2nonsolid400big.7z | grep '^Block = ' | sort -u | grep -c 'Block = [0-9]')
    echo "bcj2solid400big.7z   -> $bcj2_solid_blocks distinct block(s)"
    echo "bcj2nonsolid400big.7z-> $bcj2_plain_blocks distinct block(s)"
    [ "$bcj2_solid_blocks" -eq 1 ] || { echo "the solid BCJ2 fixture is not solid" >&2; exit 1; }
    [ "$bcj2_plain_blocks" -gt 1 ] || { echo "the non-solid BCJ2 fixture is solid" >&2; exit 1; }
    # And it has to be a branched folder, or the pair measures nothing new.
    if ! 7z l -slt bcj2solid400big.7z | grep -q 'Method = BCJ2'; then
        echo "the solid BCJ2 fixture is not a BCJ2 archive" >&2
        exit 1
    fi
else
    echo "skipping the BCJ2 pair: no BCJ2_SOURCE binary (set BCJ2_SOURCE=/path/to/an/x86/binary)"
fi

echo
echo "corpus in $CORPUS:"
ls -la "$CORPUS"/*.7z
echo
echo "run the assertions with:"
echo "  LIBARCHIVE_BENCH_CORPUS=$CORPUS cargo test --release --test sevenz_solid -- --ignored --nocapture"

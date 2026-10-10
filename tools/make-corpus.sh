#!/bin/sh
# Builds the benchmark corpus with the real tools, so that what this project is measured against
# was not produced by this project.
#
#     sh tools/make-corpus.sh [directory]     # default: /home/user/bench-corpus
#
# An archive that only the code under test can create proves nothing about the code under test, so
# every fixture here comes from `7z`, `zip` or `tar`. The one exception is called out below.
#
# The pair that matters is solid400big.7z and nonsolid400big.7z: the same 400 files, the same LZMA2
# codec, one shared block against four hundred separate ones. Anything the solid archive costs more
# than the non-solid one is the cost of a shared block, and anything that *grows* with the entry
# count is the quadratic signature this crate exists to remove.

set -eu
CORPUS="${1:-/home/user/bench-corpus}"
ENTRIES="${ENTRIES:-400}"
ENTRY_WORDS="${ENTRY_WORDS:-6000}"

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

echo "building the 7Z pair with 7z $(7z 2>/dev/null | sed -n '2s/^.*\) //p' | head -1)"
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

echo
echo "corpus in $CORPUS:"
ls -la "$CORPUS"/*.7z
echo
echo "run the assertions with:"
echo "  LIBARCHIVE_BENCH_CORPUS=$CORPUS cargo test --release --test sevenz_solid -- --ignored --nocapture"

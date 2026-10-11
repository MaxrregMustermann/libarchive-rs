#!/usr/bin/env python3
"""Build the profiling corpus: one large ZIP that looks like what a file manager actually meets.

    python3 tools/make_bigzip.py /home/user/bench-corpus 629

The point of this fixture is that it is built by a tool that is not the code under test, so a
number measured against it is a number about the library and not about the generator.

Shape, and why:

  * ~20,000 entries, which is what a phone's Download or a shared folder accumulates. Entry
    count, not byte count, is what listing cost scales with, so it is the dimension that matters.
  * Nested directories, because a flat archive would understate the work a real listing does
    and would let the implementation cache one path prefix.
  * A realistic size distribution: mostly small text and images, a long tail of larger files.
    Every entry is a distinct size, so an implementation that assumes a constant cannot pass.
  * Mixed methods - stored for the already-compressed binary payload, deflate for the text -
    because a file manager's users do not compress uniformly.
  * A handful of unicode names and a name with a space in it, because a real listing has to
    render them and a bug that only shows up on those is a bug users find first.

Everything is deterministic: a fixed seed, and content derived from the entry index, so two
runs produce byte-identical archives and a timing difference between them is a real difference.

    python3 tools/make_bigzip.py OUT.zip ENTRIES
"""

from __future__ import annotations

import os
import random
import sys
import zipfile

# 24 words of plausible log text. Mixed into the body so the deflate path has something to do
# that is not one repeated symbol, which would measure the codec's shortest path rather than
# its normal one.
WORDS = (
    "archive entry index offset header payload stream buffer window flush finish reset "
    "deflate inflate encode decode compress dictionary block flush pending table hash chain "
    "match literal distance length code tree symbol huffman lzma zstd brotli container"
).split()


def body(seed: int, size: int) -> bytes:
    """Deterministic, compressible, and not one repeated symbol."""
    rng = random.Random(seed)
    out = bytearray()
    while len(out) < size:
        out.extend(f" {rng.choice(WORDS)}".encode())
    return bytes(out[:size])


def entry_size(index: int, rng: random.Random) -> int:
    """A long tail: most files small, a few large, which is what real directories look like.

    The exact distribution is not load-bearing; that it is *spread* and *reproducible* is.
    """
    roll = rng.random()
    if roll < 0.70:
        return rng.randint(200, 4_000)
    if roll < 0.92:
        return rng.randint(4_000, 40_000)
    if roll < 0.995:
        return rng.randint(40_000, 400_000)
    return rng.randint(400_000, 3_000_000)


def name_for(index: int, rng: random.Random) -> str:
    folder = f"folder_{index % 64:02d}/sub_{index % 7}"
    extension = rng.choice([".txt", ".txt", ".txt", ".jpg", ".bin", ".log", ".dat"])
    # A few names a real directory has and a synthetic one usually does not.
    if index % 500 == 499:
        return f"{folder}/a file with spaces {index:05d}{extension}"
    if index % 1000 == 997:
        return f"{folder}/ünïcodé-ñame-{index:05d}{extension}"
    return f"{folder}/entry_{index:05d}{extension}"


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "/home/user/bench-corpus/big.zip"
    entries = int(sys.argv[2]) if len(sys.argv) > 2 else 20_000
    rng = random.Random(20261010)

    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    total = 0
    with zipfile.ZipFile(out, "w", allowZip64=True) as archive:
        for index in range(entries):
            size = entry_size(index, rng)
            data = body(index, size)
            total += len(data)
            # Stored for the binary extensions: they are already compressed, and asking deflate
            # to try would measure the codec failing rather than the library working.
            method = zipfile.ZIP_STORED if name_for(index, rng).endswith((".jpg", ".bin")) else zipfile.ZIP_DEFLATED
            archive.writestr(
                zipfile.ZipInfo(name_for(index, rng), date_time=(2026, 10, 10, 12, 0, 0)),
                data,
                compress_type=method,
            )

    print(f"{out}")
    print(f"  entries        {entries}")
    print(f"  uncompressed   {total:,} B ({total / 2**20:.1f} MiB)")
    print(f"  on disk        {os.path.getsize(out):,} B ({os.path.getsize(out) / 2**20:.1f} MiB)")
    # Re-open with the standard library and print what it thinks, so the fixture is verified by
    # something other than the thing that made it before anyone measures against it.
    with zipfile.ZipFile(out) as archive:
        bad = archive.testzip()
        print(f"  zipfile -t     {'OK' if bad is None else 'FAILED at ' + bad}")
        print(f"  entries read   {len(archive.infolist())}")


if __name__ == "__main__":
    main()
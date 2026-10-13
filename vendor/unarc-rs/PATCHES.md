# Patches applied to the vendored `unarc-rs`

`unarc-rs` 0.7.2 is vendored here and used as a path dependency, the same arrangement
`sevenz-rust2` uses. This file records every difference from upstream, so that a re-sync does not
silently undo a fix. See `vendor/sevenz-rust2/PATCHES.md` for the same role over there.

## Why it is vendored at all

`unarc-rs` provides the readers for the long tail — ARC, ARJ, Ha, Hyp, Zoo, SQ, ACE, LZH, ICE, RAR
and the JAR/SQZ variants. Hostile input found real defects in it: a corrupted `.ha`, `.sq` or `.arc`
file could panic the decoder, and a panic at the JNI boundary aborts the process on Android rather
than raising a Java exception. A panic reachable from "the user opened a slightly corrupt archive" is
an app crash, so the decoders had to be fixed, and a crates.io dependency cannot be patched in place.

The two fixes below are the minimum that makes "open an archive" survivable. They are deliberately
small; anything larger should be argued for on its own.

## The functional patches

### 1. `src/ha/asc.rs` — bound the decode loop

`decompress()` drove its Huffman decode with an unbounded `while` loop. A corrupted stream can make
that loop never terminate, which showed up as the hostile harness's HANG verdicts: eleven of them,
all Ha, no verdict within 15 seconds. A wedged decoder is a frozen UI thread on a phone.

    + const MAX_DECODE_ITERATIONS: usize = 100_000;
    + let mut decode_iterations = 0usize;
      ... in the decode loop:
    +     decode_iterations += 1;
    +     if decode_iterations > MAX_DECODE_ITERATIONS { return Err(...); }

100,000 is deliberately far above any legitimate file in the corpus; the largest Ha fixture decodes in
a few hundred iterations. The bound is a backstop against corrupt input, not a performance knob.

### 2. `src/ha/ha_archive.rs` — reject entries whose header cannot be true

A `Dir` or `Special` entry carries no data, so a non-zero `original_size` in its header is
impossible. Upstream read those entries without looking, which let a corrupted header produce an
entry that claims to have data and does not.

    + if header.original_size != 0 { return Err(...) }   for CompressionMethod::Dir
    + if header.original_size != 0 { return Err(...) }   for CompressionMethod::Special

## What is *not* a patch: the `acoder.rs` comment

An earlier version of the commit message for this work claimed `src/ha/acoder.rs` was changed so that
"read_bit feeds zeros at EOF instead of underflowing". **That is not true and the diff does not do
it.** Upstream 0.7.2 already feeds zeros on EOF:

    upstream src/ha/acoder.rs:67   // On EOF, feed zeros (standard practice for trailing bits)

and already carries the 1000-iteration renormalisation bound. The only change to that file is a
comment that was reflowed onto more lines. It is recorded here so the next reader does not go looking
for a fix that was never applied.

## The formatting churn, and why it must stay

Fifty-seven files under `src/` differ from upstream **only** by `rustfmt` reflow — line joins, splits
and indentation, no semantic change. Verify with:

    diff -uw vendor/unarc-rs/src <registry>/unarc-rs-0.7.2/src    # for these files: no output

That is not accident. `unarc-rs` is a workspace member, so `cargo fmt` at the repository root formats
it, and upstream is not rustfmt-clean under this project's configuration. Without the reflow,
`cargo fmt --check` fails — and the project treats that as a merge blocker. So the churn is the price
of the crate joining the workspace, and it should stay.

It does make `diff -r` against upstream unreadable, which is the real cost. `diff -uw` is the way
through it, and it is how the two functional patches above were isolated from the other fifty-five
files.

## Re-syncing to a new upstream version

1. Replace the tree from the registry copy.
2. Re-apply the two patches above; both are small and both are justified in this file.
3. Run `cargo fmt`, or the workspace bar will fail.
4. Re-run the hostile harness, which is the gate for exactly this:

        cargo test --offline --test hostile -- --nocapture

   It must report `0 bad`. Before the fixes it reported 100: 10 panics, 11 hangs and 79 cases that
   returned `Ok` with something other than the entry's contents.
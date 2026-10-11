# Provenance of the long-tail fixtures

Every archive here is a **real archive of its format**, copied in rather than generated, so that
`libarchive/tests/long_tail.rs` runs offline from files that are in the repository and not from a
crate cache that a future machine may not have. Nothing in this directory was produced by this
crate: a test that checks our decoder against our own writer proves only that the two agree.

Run the table with:

```
CARGO_TARGET_DIR=/home/user/cargo-target-wren cargo test -p libarchive --offline --test long_tail -- --nocapture
```

## Where the archives came from

**`rar/`, `ace/`, `arc/`, `zoo/`, `arj/`, `sq/`, `uc2/`, `ha/`, `hyp/`, `ice/`** —
`unarc-rs` 0.7.2, `tests/<format>/`, verbatim, with the file names kept. The source tree is

```
~/.cargo/registry/src/index.crates.io-*/unarc-rs-0.7.2/tests/
```

`unarc-rs` is the crate this repository carries these readers from, so these are the archives its
own test suite uses, and the ones a regression in our adapter would have to break.

`unarc-rs` 0.7.2 ships **no LZH fixture and no UCB fixture at all**. UCB has no fixture anywhere on
this machine, which is why the table has a row for the format and no row of measurements for it.

**`lzh/`** — `oxiarc-lzhuf` 0.4.2, `tests/data/`, verbatim:

```
~/.cargo/registry/src/index.crates.io-*/oxiarc-lzhuf-0.4.2/tests/data/
```

These are genuine LHA archives produced by historical LHA implementations, not by the crate that
carries them. Per that crate's own `tests/data/README.md`, they come from the
[`fragglet/lhasa`](https://github.com/fragglet/lhasa) test-archive collection at commit
`75ed83559f23e9538e0045c62f53f77ab03d03d6`, and were verified upstream with **Lhasa v0.6.0**
(`lha t`, which checks the stored CRC-16). `lha255e_lh5` was made by the Japanese LHA 2.55e;
`lha_unix114i_*` by LHa for UNIX 1.14i. They carry a small SFX stub in front of the LZH header,
which is why `detect` does not see them as LZH (see the detection column in the table).

## Where the plaintexts came from

`plaintext/` holds the bytes every extraction is checked against. The test computes a CRC-32 over
the bytes streamed out of the reader and a CRC-32 over the file here, and compares the two; the
expected CRC is never written down as a constant, so swapping a plaintext for a different one
fails the test rather than silently redefining "correct".

| File | Bytes | What it is |
|------|-------|------------|
| `license.txt` | 11,357 | `unarc-rs-0.7.2/LICENSE` verbatim (Apache-2.0). Nearly every fixture in the table holds this file. |
| `DDTZ.COM`, `READ.COM` | 9,984 / 128 | `unarc-rs-0.7.2/tests/arc/DDTZ.COM` and `.../READ.COM` verbatim: the two payloads `arc/cpm.arc` holds. |
| `hyp-a.txt`, `hyp-b.txt`, `hyp-c.txt` | 3 / 14 / 4 | The three entries of `hyp/stored.hyp`: `a\r\n`, eleven `a`s then `\r\n`, `aa\r\n`. `unarc-rs-0.7.2/tests/hyp_decompression.rs` asserts the first and third byte for byte; the middle one is the same fixture's third line. |
| `lzh-GPL-2.txt` | 18,092 | Extracted with **oxiarc-lzhuf 0.4.2** from `lzh/lha255e_lh5.lzh`. `unarc-rs` 0.7.2 decodes the same archive to the same CRC-32 (`4e46f4a1`), and upstream Lhasa 0.6.0 extracted it from the original archive, so two independent decoders agree on these bytes. |
| `lzh-gpl-2.gz` | 6,829 | The same, from `lzh/lha_unix114i_h0_lh0.lzh` (the `-lh0-` store variant, whose member is the gzipped GPL-2). Both decoders agree: CRC-32 `e4690583`. |

No LZH fixture ships with its plaintext, which is why the two LZH plaintexts are the only files
here that were produced by a decoder. They are not checked against *this* crate's output — they are
the input to the comparison — and they agree with a second, unrelated decoder.

## Checksums

SHA-256, first 16 hex digits, over each file as vendored. Enough to notice a truncated or mangled
copy; `sha256sum` over this directory reproduces the list.

```
c457c207063a9194  ace/license1.ace
40f3d9421290b4a5  ace/license2.ace
74ace2ed2c97bb17  ace/multi/unarc.ace
903055ca9c5fb301  ace/multi/unarc.c00
830cd25677c5e1cd  ace/multi/unarc.c01
66b2958f8b518a3c  arc/cpm.arc
fc92cf1ac975e058  arc/crunch2.arc
bdd0fdc6500d0717  arc/crunch.arc
838d6c67e805f310  arc/license_cypted.arc
7daa2c3ead2d9e4a  arc/squashed.arc
3e3272f20848b95e  arc/store.arc
229b14408024305d  arc/wrongcrc16.arc
29b1c579452f5b6e  arj/license_crypted.arj
a610180da1a37246  arj/method1.arj
f5585d01f2534892  arj/method2.arj
7186a2d7a0121a3b  arj/method3.arj
3a1e1ebcf4cdc3dd  arj/method4.arj
80c1a51a509eddc9  arj/multi/test_file.a01
caf74286c0b424e3  arj/multi/test_file.a02
f33c9bea36e8b6fd  arj/multi/test_file.arj
3108cf45a88a657c  arj/stored.arj
13b78dc78f838d91  arj/wrongcrc32.arj
c6a8a075559f457b  ha/asc.ha
3655b396502df3d4  ha/copy.ha
cf860a56029e89a1  ha/hsc.ha
ba22adcb207d47f7  hyp/invalid.hyp
82d9980aad1fcc81  hyp/license.hyp
5c2ab9ae603dd0c5  hyp/stored.hyp
833616ba837b2413  ice/license_lha.ice
7f10d0be69536733  lzh/lha255e_lh5.lzh
568edb96b0cf40d2  lzh/lha_unix114i_h0_lh0.lzh
cf709905a6d89fb4  lzh/lha_unix114i_h0_lh5.lzh
511e1d679cbedcb2  lzh/lha_unix114i_h1_lh5.lzh
67fa06a23f8b8537  lzh/lha_unix114i_h2_lh5.lzh
fc2769fe9c0c473e  plaintext/DDTZ.COM
8e4621379786ef42  plaintext/hyp-a.txt
9879d73fcad64c62  plaintext/hyp-b.txt
77a1e8e39473ba9a  plaintext/hyp-c.txt
c71d239df91726fc  plaintext/license.txt
5c423e9bdf915d23  plaintext/lzh-gpl-2.gz
8177f97513213526  plaintext/lzh-GPL-2.txt
25784f644057784a  plaintext/READ.COM
3fe0979ba5c252ec  rar/multi/unarc.part1.rar
639ba12ef0810724  rar/multi/unarc.part2.rar
939c79dbc11a384c  rar/multi/unarc.part3.rar
462ae8d1f91ece5a  sq/license.sq
4c6ba4cbafb082e5  sq/license.sq2
a0d0169138d59fcd  uc2/fast.uc2
ad341656c3447f94  uc2/normal.uc2
7ba0ee0a90f46f5e  uc2/stight.uc2
19d34d9a60f75af5  uc2/tight.uc2
ae45e8abe3e36bc0  zoo/default.zoo
ce35965239be786b  zoo/high_per.zoo
27cd5b07007167ee  zoo/store.zoo
efa11d81d2ebd49f  zoo/wrongcrc16.zoo
```

## What is deliberately not here

`pak/`, `pi9/`, `sqz/`, `ue2/` and `acid-70a.rar` also exist in the `unarc-rs` test tree. They are
not in `Format`, so a row in the long-tail table for them would be a claim about a format this
crate does not have. `ue2/license.ue2` is the interesting one: `unarc-rs` rejects it as
`UnsupportedMethod { format: "UC2", method: "UltraCrypt (UE2)" }`, so if UCB-style encrypted UC2
ever reaches this crate it will fail there, not silently produce bytes.
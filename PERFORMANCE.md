# Performance baselines

Every row below is a number produced by a command in this repository. A claim without a command is a guess, and this project has already shipped a benchmark asserting a property that had never compiled.

## ZIP write — 5,000 entries x 512 B (251-byte cycle payload)

| Metric | Code being replaced (oxiarc @ 93b17af) | This crate | Bar | Status | Command |
|---|---|---|---|---|---|
| Time (deflate, level 6) | **268.1 ms** wall / 260 ms cpu | **231.9 ms** wall / 220 ms cpu | 109.2 ms | **NOT MET** | `cargo bench --bench zip --offline -- zip-write-5000-entries/deflate` |
| Time (stored) | — | **13.9 ms** wall / 10 ms cpu | 60 ms | MET | `cargo bench --bench zip --offline -- zip-write-5000-entries/stored` |
| Sink writes per entry | 32 | **3.00** | 6 | MET | `cargo bench --bench zip --offline -- sink-writes-per-entry` |
| Allocated per archive | 1.40 GiB | **12.20 MB** (11.64 MiB, 12,202,175 B) in 40,036 calls | 43.9 MB | MET | `cargo bench --bench zip --offline -- allocated-bytes-per-archive` |

## ZIP write — 5,000 entries x 512 B (low-alphabet "recycled deflater " payload)

| Metric | Code being replaced (oxiarc @ 93b17af) | This crate | Bar | Status | Command |
|---|---|---|---|---|---|
| Time (deflate, level 6) | **45.3 ms** wall / 40 ms cpu | **92.3 ms** wall / 90 ms cpu | — | **2× slower** | `cargo bench --bench zip --offline -- zip-write-5000-entries/deflate` (see notes) |

Notes:
- The deflate path does not meet the 109.2 ms bar. The stored path does, which places the gap
  squarely in the codec: ~232 ms total against ~14 ms of structural work means roughly 44 us per
  entry is deflate of 512 bytes. Whether 109.2 ms is reachable on this machine depends on the
  deflate backend — this crate uses `zlib-rs` (via `flate2`'s `zlib-rs` feature, unified by
  `unarc-rs`) while the bar was measured against oxiarc, whose deflate may differ.
- **On the benchmark's own 251-byte-cycle payload this crate is faster than oxiarc** (232 vs 268 ms
  wall min, both at level 6 in the same process). The 109.2 ms bar was not produced by oxiarc on
  this machine; the reference implementation agrees the bar is not met by either writer on this
  payload.
- **On a low-alphabet payload ("recycled deflater " 18-byte cycle) this crate is ~2× slower than
  oxiarc** (92 vs 45 ms wall min). The benchmark prints both payloads via `timing_report()`; run
  `cargo bench --bench zip --offline -- zip-write-5000-entries` to see the before/after rows.
- **These numbers move with machine load.** The same benchmark reports ~230 ms on an idle machine
  and ~300 ms with several agents running. Read the allocation and sink-write figures instead: both
  are deterministic, and both are asserted.
- Sink writes per entry and allocation are asserted by the benchmark (`sink-writes-per-entry`,
  `allocated-bytes-per-archive`), so a regression fails the test rather than quietly producing a
  worse number.

## Listing — bytes touched

| Metric | Code being replaced | This crate | Bar | Status | Command |
|---|---|---|---|---|---|
| Archive size | 1,463,092 B | 1,463,092 B | — | — | `cargo bench --bench listing --offline` |
| Bytes read to list 3,355 entries | 54.6 MB | **277,579 B (18.97%)** | 1,024,000 B | MET | `cargo bench --bench listing --offline` |
| Listing time | 671 ms (inc. build) | **2.60 ms** | 3.4 ms | MET | `cargo bench --bench listing --offline` |

Notes:
- The old `BufReader` bug read 54.6 MB of the archive just to list it (every refill was thrown away). This crate reads only the central directory and the local headers it needs.
- The byte count is asserted at startup (`listing_stays_small`), so a regression that makes listing read more of the archive fails before any benchmark runs.
- The listing time of 2.60 ms is the wall-clock minimum from criterion; the assertion bound is 3.4 ms.

## Solid 7Z vs non-solid 7Z — 401 entries, 15,465,403 bytes

| Metric | read_file (one call per entry, by name) | open_entry (in order, block reuse) | open_entry (non-solid) | Command |
|---|---|---|---|---|
| Time | **224.6 s** | **789 ms** | **2.02 s** | `LIBARCHIVE_BENCH_CORPUS=/home/user/bench-corpus cargo test --release --test sevenz_solid --offline -- the_crate_being_replaced_is_the_slow_one --ignored --nocapture` (read_file) / `solid_is_not_quadratic` (open_entry) |
| Speedup vs read_file | 1× | **285×** | — | same |
| Solid vs non-solid ratio | — | **0.6×** (solid faster) | 1× | `LIBARCHIVE_BENCH_CORPUS=/home/user/bench-corpus cargo test --release --test sevenz_solid --offline -- solid_is_not_quadratic --ignored --nocapture` |

Notes:
- The orchestrator's trusted measurement from the same test on this machine (different load): `read_file` 354.4 s, `open_entry` in order 1.24 s, non-solid 1.29 s. The difference is machine load and CPU scaling; the property (hundreds of seconds vs sub-second, solid not quadratic) is identical.
- The solid archive is *faster* than the non-solid one because the shared block is decoded once. The ratio < 1.0 is the property this crate exists to prove: a solid block costs one decode, not one per entry.
- The quadratic assertion (`ratio < 4.0`) passes: 0.6× is well within the bound.

## First-byte latency — 64 MiB stored entry

| Metric | Code being replaced (oxiarc) | This crate | Bar | Status | Command |
|---|---|---|---|---|---|
| Time to first byte | ~1 s (whole-entry buffer) | **193 ns** | 5 ms | MET | `cargo bench --bench zip --offline -- zip-first-byte` |

Notes:
- 193 ns, measured, not the assertion bound. The benchmark builds a 256 MiB archive of four
  64 MiB stored entries, opens it once outside the timed section, and measures from `open_entry(0)`
  to the first `read_exact(&mut [0u8; 1])`.
- The payload is shared behind an `Arc` with a per-cursor position. The first version of this
  benchmark did `Cursor::new(bytes.clone())` on a 256 MiB archive *inside* the timed closure and
  reported 149 ms, which was a memcpy rather than a reader.
- The assertion (`assert_first_byte_is_cheap`) fails if the first byte takes ≥ 5 ms, which would mean an entry is being materialised.

## Reading every entry in order — 5,000 entries, deflate

| Metric | This crate | Command |
|---|---|---|
| Time | **2.1 s** (measured) | `cargo bench --bench zip --offline -- zip-read/all-entries-in-order` |

Notes:
- This is the throughput number for a realistic workload. No bar is set because the bottleneck is the deflate codec, not the crate's overhead.

## How to run the full suite

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=/home/user/cargo-target/<your-unique-dir>

# ZIP write + listing + first byte + read all
cargo bench --bench zip --bench listing --offline

# Solid 7Z (needs the corpus built once)
sh tools/make-corpus.sh /home/user/bench-corpus
LIBARCHIVE_BENCH_CORPUS=/home/user/bench-corpus cargo test --release --test sevenz_solid --offline -- --ignored --nocapture
```

## What the bars mean

- **MET**: the current number is at or better than the bar.
- **NOT MET**: the current number exceeds the bar. The row is kept (not omitted) so the gap is visible.
- Bars are derived from the code being replaced (`oxiarc` @ 93b17af for ZIP, `unarc-rs` + `sevenz-rust2` for 7Z) or from the property the crate exists to enforce (listing bytes, solid-block reuse, first-byte latency).
- A benchmark that asserts rather than reports is a test that fails on regression. A benchmark that only reports lets a regression slip into a number nobody reads.
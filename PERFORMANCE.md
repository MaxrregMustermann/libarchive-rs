# Performance baselines

Every number below was measured by the orchestrator or by a named agent, in this wave, with the
command printed beside it. A number without a command is a guess, and this repository has a history
of shipping exactly that: the previous `PERFORMANCE.md` had eleven numbers for benchmarks that did
not exist. The rule from that is simple and it is the only reason this document can be trusted:

> **If a row here cannot be reproduced by running the command in that row, the row is wrong.**

Where a number could not be measured on an idle machine it says so, rather than being quietly
presented as a result.

## The one thing to read first: the host is not the target

The device is **1.1x to 2.9x slower than the host depending on the workload**, and the 5,000-entry
ZIP write is **not met on the device**. Every "this crate" figure below is a host figure unless the
row says otherwise.

| workload | API 24 emulator | host | ratio |
|---|---|---|---|
| list, 20,000 entries, 249 MB ZIP | 17.81 ms | 12.50 ms | 1.42x |
| first byte of a 2.9 MB entry | 56.03 us | 33.30 us | 1.68x |
| read all, 612 MB out | 3,099.57 ms | 1,084.78 ms | **2.86x** |
| extract, solid 7Z, 401 entries | 546.66 ms | 494.46 ms | 1.11x |
| **write, 5,000 entries, deflate** | **168.72 ms** | **97.43 ms** | **1.73x** |

    tools/run-on-device.sh /home/user/bench-corpus      # runs the device block, then the host
                                                       # block, from the same source, same commit

A second run of the same command gave `write 119.26 ms device / 71.29 ms host (1.67x)` and
`extract 500.47 / 378.39 (1.32x)`, so read the device column as *this emulator, this load*, not as a
constant. The ranking is the finding: the device penalty tracks **bytes decompressed**, not
operations, which is why read-all is 2.9x and extract only 1.1x.

**These are not phone numbers and must not be quoted as such.** `adb shell getprop
ro.product.cpu.abi` reports `x86` and the process is `qemu-system-i386-headless`: the API 24 emulator
runs a **32-bit** binary and the host is x86-64. Most of the gap is word size, not our code. There
are no ARM numbers on this machine; one x86 AVD is what exists.

## ZIP write — 5,000 entries x 512 B, 251-byte-cycle payload

| | oxiarc @ 93b17af (replaced) | this crate | bar | status |
|---|---|---|---|---|
| Time, level chosen by entry size (**default**) | — | **70.1 ms** wall / 70 ms cpu | 109.2 ms | MET (host) |
| Time, level 6 pinned (the old fixed behaviour) | — | 216.9 ms / 210 ms | 109.2 ms | not met |
| Time, same payload, oxiarc | **240.9 ms** / 240 ms | — | — | — |
| Time, stored (structural only) | — | 5.29 ms | 60 ms | MET |
| One-entry floor | — | 1.21 ms | — | — |
| Allocated per archive | 1.40 GiB | **12,275,429 B (11.71 MiB) in 40,037 calls** | 43.9 MB | MET |
| Sink writes per entry | 32 | **3.00** | 6 | MET |

    cargo bench --offline --bench zip

One command prints all of it: the benchmark's own `timing_report()` runs both writers interleaved in
one process and prints min-of-12 wall and cpu for each. **These were taken on a loaded machine**
(load average 5.8 to 20), so treat the milliseconds as an upper bound; the allocation and sink-write
figures are deterministic and are the ones worth quoting. Those two are also **asserted by the
benchmark**, so a regression fails `cargo bench` rather than quietly producing a worse number nobody
reads.

The default picks the deflate level from the entry size (`DEFLATE_LEVEL_THRESHOLD`, 8 KiB), so these
512-byte entries run at level 1. On this payload level 6 costs 3x the time for 0.01% more bytes,
which is why the default exists; `timing_report()` prints both so the benchmark is its own
before/after.

## ZIP write — the one real gap: 18-byte low-alphabet cycle

| payload | this crate | oxiarc @ 93b17af |
|---|---|---|
| 251-byte cycle (above) | **70.1 ms** | 240.9 ms |
| 18-byte low-alphabet cycle | **57.4 ms** | **44.4 ms** |

    cargo bench --offline --bench zip

On the bench's own payload we are 3.4x faster than the code we replace. On the low-alphabet
payload we are **~29% slower**, and we are not trading size for it. That is J-0026, and the profile
that explains it is J-0029:

    perf record -g --call-graph=dwarf    # on the low-alphabet write
    perf report

    76.83%  __memset_avx2_unaligned_erms  (libc.so.6)
    called from zlib-rs Compress::reset()

`Compress::reset()` memsets zlib's 64 KiB hash table, once per entry, 5,000 times, for entries of
512 bytes. That is the whole of the remaining gap. The reset itself cannot be removed — each ZIP
entry is an independently decodable stream — so the lever is `memLevel`, which *is* the hash-table
size. Whether closing it works is J-0029 and is not yet known.

## Listing — bytes touched

| | value |
|---|---|
| Archive size | 1,459,737 B |
| Bytes read to list 3,355 entries | **277,579 B (19.02%)** |
| Bar | 1,024,000 B — MET |
| Listing time | 1.93 ms, ~1.0 GiB/s |

    cargo bench --offline --bench listing

`listing_stays_small()` asserts the byte count **at startup, before any number is printed**, so a
regression that starts reading more of the archive fails before the benchmark runs. The old
`BufReader` bug read 54.6 MB just to list the archive; this reads only the central directory and the
local headers it needs.

## First-byte latency — 64 MiB stored entry

| | value |
|---|---|
| Time to first byte | **~170 ns** |
| Bar | 5 ms — MET by four orders of magnitude |

    cargo bench --offline --bench zip -- zip-first-byte

Measured, not the assertion bound. The payload is shared behind an `Arc` with a per-cursor position;
the benchmark opens the archive once outside the timed section and times from `open_entry(0)` to the
first `read_exact(&mut [0u8; 1])`. An earlier version of this benchmark cloned a 256 MiB buffer
*inside* the timed closure and reported 149 ms, which was a memcpy rather than a reader.

## Reading every entry in order — 5,000 entries, deflate

| | value |
|---|---|
| Time | 16.5 ms, ~120 MiB/s |

    cargo bench --offline --bench zip -- zip-read/all-entries-in-order

No bar is set: the bottleneck is the deflate codec, not this crate's overhead.

## Solid 7Z

Not re-measured in this wave. The banked figures are 441 ms solid / 678 ms non-solid for 401 entries,
and 354.4 s to 1.24 s (285x) against the old `read_file` path. The device run above measures solid
extraction independently and gets 546.66 ms device / 494.46 ms host, which is the same order and is
consistent with the banked host figure.

    LIBARCHIVE_BENCH_CORPUS=/home/user/bench-corpus \
      cargo test --release --test sevenz_solid --offline -- --ignored --nocapture

## Robustness: hostile input

Not a performance number, but it is the bar that matters most for a file manager, and it is a gate.

    cargo test --offline --test hostile

| | before this wave | after |
|---|---|---|
| cases | 7,618 | 7,861 |
| **panics** | **10** | **0** |
| **hangs** | **11** | **0** |
| silent wrong-length | 79 | 0 bad, 73 size-disagreeing (reported) |
| **total bad** | **100** | **0** |

A panic at the JNI boundary aborts the process on Android rather than raising a Java exception, so
those 21 were app crashes reachable from a slightly corrupt `.ha`, `.sq` or `.arc` file. They are
fixed in the vendored `unarc-rs`; see `vendor/unarc-rs/PATCHES.md`, which also records one commit
message that claimed a patch that was never applied.

The 73 "size-disagreeing" cases are reported, not failed, and the reason is checked rather than
assumed: `ha/ha_archive.rs:100`, `arc/arc_archive.rs:100` and `hyp/hyp_archive.rs:30` each verify a
CRC over the entry data, so a read that returned `Ok` has had its *bytes* verified. What disagrees is
the header's size field, which the CRC does not cover.

## Correctness gates that exist because of this project

    cargo test --offline --test differential -- --nocapture
    compared 64 archives, 79 oracle pairs, 1910 entry answers

against unarc-rs 0.7.2, sevenz-rust2 0.23.0 and oxiarc @ 93b17af. Thirteen disagreements are
`pending` in the corpus manifest with named owners; everything else is a failure.

    cargo test --offline --test writer_external
    cargo test --offline --test zip_integrity

`writer_external` proves every writer against the tool that owns that format — `tar`, `xz`,
`bzip2`, `gzip`, `zstd`, `lz4`, `7z`, `unzip`, python — comparing bytes and entry names rather than
exit status. A missing tool is a loud failure, never a skip.

## Reproducing all of it

    export PATH="$HOME/.cargo/bin:$PATH"
    export CARGO_TARGET_DIR=/home/user/cargo-target/<yours>
    export TMPDIR=/home/user/tmp          # /tmp is a 7.7 GiB tmpfs and filled to 98% in wave 2

    cargo bench --offline --bench zip --bench listing     # every host number above
    tools/run-on-device.sh /home/user/bench-corpus         # the device table; emulator must be up
    cargo test --offline                                  # 530 tests, including hostile + differential

For any wall-clock number, check `uptime` first. These were taken at load 5.8 to 20 and are upper
bounds; the deterministic figures (allocations, sink writes, bytes touched, case counts) are the ones
to hold us to.
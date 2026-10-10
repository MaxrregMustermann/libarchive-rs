#!/usr/bin/env bash
# Brings the collective up from nothing: teams, agents, jobs and the first wave.
# Idempotent, so it can be re-run after wiping /home/user/coord.
set -euo pipefail
cd /home/user/libarchive-rs
C="python3 tools/collective.py"
$C init

$C team add core     --leader ada  --charter "The public API, the error model, format detection, limits, and the reader and writer plumbing every format plugs into. Owns libarchive/src/{error,limits,detect,format,entry,reader,writer,compression}.rs and nothing inside formats/." || true
$C team add container --leader grace --charter "The multi-entry container formats: ZIP, TAR, 7Z, and the composed forms TAR.XZ and JAR. Owns libarchive/src/formats/{zip,tar,sevenz}.rs and the vendored sevenz-rust2 patch." || true
$C team add codecs   --leader ken   --charter "The single-stream compressors: GZ, BZ2, XZ, Z, LZ4, Zstd, Brotli, and the deflate and deflate64 primitives every other format is built on." || true
$C team add legacy   --leader mira  --charter "The long tail carried across from unarc-rs: RAR, ARJ, ACE, ARC, Zoo, SQ, UCB, UC2, Ha, Hyp, Ice, PackIce. Read-only. Differential-tested against unarc-rs itself." || true
$C team add bridge   --leader vic   --charter "The JNI boundary and the Kotlin side: the extern system functions, the error mapping, the archive handle cache, and emulator verification on API 24 for all four ABIs." || true
$C team add qa       --leader tess  --charter "Everything that decides whether it works: the differential harness against unarc-rs, the format corpus, the fuzz targets, and the benchmarks that hold the performance numbers." || true

add() { $C agent add "$@" 2>/dev/null || true; }
add ada   --team core     --model opencode/nemotron-3-ultra-free           --role leader
add ralph --team core     --model opencode/step-5-preview-free
add grace --team container --model opencode/longcat-2.5-preview-free       --role leader
add zara  --team container --model opencode/big-pickle
add ken   --team codecs   --model opencode/muse-spark-1.3-contributor-free --role leader
add otis  --team codecs   --model opencode/ling-3.1-flash-free
add mira  --team legacy   --model opencode/nemotron-3.5-lightning-free     --role leader
add nina  --team legacy   --model opencode/ling-3.0-flash-fin-free
add vic   --team bridge   --model opencode/space-bunny-free                --role leader
add hank  --team bridge   --model opencode/exo-free
add tess  --team qa       --model opencode/mimo-v2.6-flash-free             --role leader
add pip   --team qa       --model opencode/ling-3.1-flash-free
add dev   --team qa       --model opencode/step-5-preview-free

if ! $C job show J-0001 >/dev/null 2>&1; then bash tools/wave1.sh; fi
echo "collective is up"

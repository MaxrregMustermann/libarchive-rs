#!/bin/sh
# Forms the collective: teams, agents, and the first wave of jobs.
#
# Idempotent-ish: it refuses to overwrite an existing team or agent, so it can be
# re-run after adding a wave without destroying what is already there.
#
#     sh tools/setup-collective.sh

set -eu
cd "$(dirname "$0")/.."
C="python3 tools/collective.py"
$C init >/dev/null

# --- teams -------------------------------------------------------------------
# One team per thing that can be finished and measured on its own. A team exists
# only where two people can usefully work at once; below that it is a job, not an
# org chart.

$C team add core \
    --leader ada \
    --charter "The public API and everything that is not a format: ArchiveReader, ArchiveWriter, EntryStream, the error model, limits, detection, and the lint set that keeps panics out. Owner of every change to the public surface. If a job needs to add a method, change a type, or move something between modules, core signs it off first. Also owns the crate docs: lib.rs, SPEC.md, README.md, PERFORMANCE.md and NOTICE.md, which are referenced from the code and two of which do not exist yet." >/dev/null

$C team add zip \
    --leader grace \
    --charter "ZIP, read and write, and the performance bars attached to it. The write bar is 219.8 ms to 109.2 ms for 5,000 entries and 1.40 GiB to 43.9 MB allocated; we are currently at 169 ms and the allocation figure is unmeasured. ZIP is what the app writes and reads most, so every byte here is seen. Also owns the three performance fixes carried forward from the oxiarc fork at 93b17af, and the rule that a header is written in one piece and a deflater is reused across entries." >/dev/null

$C team add sevenz \
    --leader hank \
    --charter "The 7Z container. Solid blocks already read in order through the vendored BlockCursor; what is left is BCJ2 folders, which are currently refused rather than decoded, plus the coder chains, the encrypted path, and the writer. Also owns the vendored copy of sevenz-rust2 and PATCHES.md, and is the only team that may edit under vendor/." >/dev/null

$C team add codecs \
    --leader mira \
    --charter "The single-stream compressors and the compressed TARs: GZ, BZ2, XZ, Z, LZ4, Zstd, Brotli, and tar.gz, tar.bz2, tar.xz, tar.zst. Mostly working. What is left is Z, the recorded-size footer reads, and making sure a stream that does not record its size still reads and says so." >/dev/null

$C team add legacy \
    --leader ken \
    --charter "The long tail: RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp, Ice. Twelve formats that currently return Unsupported, which is a functional regression against unarc-rs and the single largest hole in the project. The parsers are Apache-2.0 and fuzzed; they are carried across rather than rewritten, and this team's job is the streaming adapter that turns them into EntrySource and the differential tests that prove it. See coord/notes/unarc-map.md." >/dev/null

$C team add bridge \
    --leader tess \
    --charter "The JNI bridge in jni/ and the Kotlin side in TheFileManager. Turns a jlong handle into a reader and an error into an IOException, and nothing else. Also owns the change that makes the app hold an archive open across per-file reads, which is what the solid-7Z fix needs to actually pay off. See coord/notes/bridge-contract.md, which already maps all nine exports and all five callbacks." >/dev/null

$C team add qa \
    --leader vic \
    --charter "Corpus, differential testing, fuzzing and benchmarks. Owns libarchive/tests/, the benchmark files, and the claim that a number was measured rather than believed. Builds archives with the real tools where they are available, reads them with both implementations, and compares. A format is not done until a differential test says it is." >/dev/null

$C team add android \
    --leader pip \
    --charter "Verification on a real Android runtime: the API 24 emulator, the four ABIs, the Gradle build, and the on-device tests. Host-side Rust tests do not count as verification of the bridge, because the bridge is where the panics and the JNI mistakes live. Owns the emulator, and is the only team that reboots it." >/dev/null

# --- agents ------------------------------------------------------------------
# A leader is an agent with the same model as its members; the distinction is
# responsibility, not capability. Models are spread so that one being rate limited
# does not take a whole team with it, and so that two agents in a team do not
# fail in the same way at the same time.

C="python3 tools/collective.py"

$C agent add ada  --team core    --model opencode/nemotron-3-ultra-free       --role leader
$C agent add grace --team zip    --model opencode/big-pickle                 --role leader
$C agent add hank --team sevenz --model opencode/step-5-preview-free         --role leader
$C agent add mira --team codecs  --model opencode/muse-spark-1.3-contributor-free --role leader
$C agent add ken  --team legacy  --model opencode/nemotron-3.5-lightning-free --role leader
$C agent add tess --team bridge  --model opencode/longcat-2.5-preview-free   --role leader
$C agent add vic  --team qa      --model opencode/mimo-v2.6-flash-free       --role leader
$C agent add pip  --team android --model opencode/space-bunny-free           --role leader

$C agent add iris --team zip     --model opencode/nemotron-3.5-lightning-free --role member
$C agent add june --team legacy  --model opencode/muse-spark-1.3-contributor-free --role member
$C agent add omar --team qa      --model opencode/step-5-preview-free       --role member
$C agent add wren --team bridge  --model opencode/space-bunny-free           --role member

echo
python3 tools/collective.py status

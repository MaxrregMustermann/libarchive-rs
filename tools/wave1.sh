#!/bin/sh
# Wave 1: the six jobs that unblock everything else.
#
#     sh tools/wave1.sh
#
# Each job carries its acceptance criteria into the agent's prompt, so the agent
# checks itself against something written down rather than against its own memory
# of what it meant to do.

set -eu
cd "$(dirname "$0")/.."
C="python3 tools/collective.py"

new() { $C job new --team "$1" --opened-by orchestrator --title "$2" --acceptance "$3"; }

# ---------------------------------------------------------------- legacy ----
J_LEGACY=$(new legacy \
"Turn the long tail from Unsupported into twelve working formats" \
"Each of RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice either opens
through formats::legacy::open and lists its entries, or is explicitly named in mail
as not achievable with a reason. The adapter streams: reading one entry of a 64 MiB
archive must not allocate 64 MiB, which a test asserts by counting allocations or by
showing the first byte arrives before the last. A differential test exists per format
that compares entry names, sizes and CRCs against unarc-rs 0.7.2 on the same bytes, and
the comparison runs in CI, not once by hand. Formats that need a whole archive in
memory say so in SPEC.md rather than pretending. If a format cannot be streamed, the
error says which and why.")

# ---------------------------------------------------------------- bridge ----
J_BRIDGE=$(new bridge \
"Implement the nine JNI exports so the app can actually call this library" \
"jni/src/lib.rs defines all nine extern \"system\" functions that RustArchive.kt
declares, with the exact signatures in coord/notes/bridge-contract.md section 1. The
return-value conventions hold: 0 from nativeOpen/nativeCreate is failure, null from
nativeNextEntry is end-of-archive and never failure, null from nativeReadData is
end-of-entry and never failure. A panic anywhere in the crate aborts the process rather
than reaching Java, so a test drives every export with a corrupt, truncated and
empty input and asserts an IOException comes back rather than a crash. The cdylib
builds for all four ABIs. No unwrap, expect or panic outside tests.")

# ------------------------------------------------------------------- zip ----
J_ZIP=$(new zip \
"Close the ZIP write gap: 169 ms to 109.2 ms for 5,000 entries" \
"The benchmark reports under 109.2 ms for 5,000 entries of 512 bytes, or a mail
explains why that bar is unreachable on this machine with the number that is. The
allocation count is measured and reported against the 43.9 MB bar; it is currently
unmeasured, which is not the same as passing. Sink writes per entry stay at or under
6. Every optimisation is justified by a measurement taken before and after, and the
measurement is in the benchmark file rather than in a transcript. A change that makes
a number worse without a stated reason is reverted.")

# -------------------------------------------------------------------- qa ----
J_QA=$(new qa \
"Build the corpus and the differential harness every other team needs" \
"A corpus generator produces archives per format under libarchive/tests/corpus/ with
a manifest naming each file and how it was made, and it is reproducible from the
repository. The differential harness reads an archive with libarchive and with the
crate that replaces it (oxiarc at 93b17af, unarc-rs 0.7.2, sevenz-rust2 0.23.0) and
compares entry names, sizes, CRCs and contents. It runs as part of cargo test, not
as a script somebody runs. A malformed corpus exists for every format: truncated,
wrong-magic, oversized-header and path-traversal cases, each of which must produce an
Err rather than a panic. If the real tools (7z, zip, unrar) are not installed, say so
in mail rather than generating fixtures that only this crate can read.")

# --------------------------------------------------------------- android ----
J_ANDROID=$(new android \
"Prove the bridge on the API 24 emulator, on all four ABIs" \
"cargo build produces the cdylib for arm64-v8a, armeabi-v7a, x86 and x86_64. The
Gradle task assemblesDebug succeeds with the app pointing at libarchive-rs rather than
oxiarc and unarc-rs. The app installs and starts on the Android_7 emulator (API 24,
-gpu angle_indirect, /dev/kvm). An instrumented or on-device test opens a real archive
and reads an entry through the JNI boundary, and the log shows the bytes arriving. A
host-side cargo test does not count for this job: the whole point is that the bridge
is where panics and signature mistakes live. If the emulator cannot boot, say so in
mail with the error rather than reporting the job done.")

# ------------------------------------------------------------------ core ----
J_CORE=$(new core \
"Write the four documents the code already references, and review the public API" \
"README.md, SPEC.md, PERFORMANCE.md and NOTICE.md exist, because lib.rs links to three
of them and two do not exist. PERFORMANCE.md states each bar, the number measured for
the code being replaced, and the number measured here, with the command that produces
it; where a bar is not met it says so rather than omitting the row. SPEC.md lists
every format and, for each, what it does and does not do - streaming or whole-entry,
encrypted or not, and which long-tail formats are refused. NOTICE.md carries the
Apache-2.0 attribution for unarc-rs, sevenz-rust2 and oxiarc. Separately: read the
public API in libarchive/src/reader.rs and writer/mod.rs and report anything that will
be expensive to change later, in mail, before team bridge and team legacy build on it.")

python3 - "$J_LEGACY" "$J_BRIDGE" "$J_ZIP" "$J_QA" "$J_ANDROID" "$J_CORE" <<'PY'
import subprocess, sys
jobs = dict(zip(
    ["legacy", "bridge", "zip", "qa", "android", "core"],
    sys.argv[1:7],
))
assign = {"legacy": "ken", "bridge": "tess", "zip": "grace",
          "qa": "vic", "android": "pip", "core": "ada"}
for team, job in jobs.items():
    subprocess.run(["python3", "tools/collective.py", "job", "assign", job, assign[team]], check=True)
PY

echo
python3 tools/collective.py status

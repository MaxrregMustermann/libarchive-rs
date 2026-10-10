#!/usr/bin/env bash
# Wave 1: every job produces a document, not code. The point is to find out what
# we are dealing with before anyone writes a line that has to be maintained.
set -euo pipefail
cd /home/user/libarchive-rs
C="python3 tools/collective.py"

$C job new --team core --title "Spec the three oxiarc streaming-writer performance fixes" \
  --acceptance "coord/notes/oxiarc-perf-fixes.md names each of the three fixes, cites the file and function in /home/user/oxiarc-prior that implements it, explains in one paragraph why it is faster, and says in one sentence what has to change in libarchive-rs to carry it across. The three fixes are: deflater reuse across entries, coalescing header writes into one sink write, and streaming an entry through add_stream instead of buffering it."

$C job new --team container --title "Work out exactly how to make solid 7Z reads linear and streaming" \
  --acceptance "coord/notes/sevenz-patch-plan.md states the minimum patch to sevenz-rust2 0.23.0 needed to build a block decode stack from outside the crate, with the real diff, and shows by reading the source that the resulting design decodes a 400-entry solid block once instead of 400 times. It must also show that a caller can stop mid-block and resume, which is what streaming requires."

$C job new --team qa --title "Design the test corpus and the differential harness" \
  --acceptance "coord/notes/testing-plan.md specifies a corpus of real archive files per format, how each one is obtained or generated, what the differential harness compares libarchive-rs against, and the command to run it. It must cover malformed input, encrypted input, and the Android ABI matrix."

$C job new --team legacy --title "Map unarc-rs: which formats, which readers, what streaming would take" \
  --acceptance "coord/notes/unarc-map.md lists every format unarc-rs 0.7.2 reads, the type that reads it, whether its reader is streaming or whole-entry, and the public API a caller can reach. For each format it states whether we must reimplement it or can carry the parser across unchanged, with the reason."

$C job new --team bridge --title "Write down the JNI and Kotlin contract we have to satisfy" \
  --acceptance "coord/notes/bridge-contract.md lists every extern system function the app currently calls with its exact signature, every field of every Kotlin data class that crosses the boundary, how errors are mapped, and precisely what has to change on the Kotlin side so an archive can be held open across per-file reads instead of reopened per file."

$C job new --team codecs --title "Inventory the compression codecs and their crate versions" \
  --acceptance "coord/notes/codecs.md lists every compression method the library must read and write, the crate that provides it, whether that crate already builds for all four Android ABIs with no runtime dependency, and which of them are unused by the app today."

$C mail send --from orchestrator --to ada,grace,ken,mira,vic,tess \
  --subject "Wave 1 is research only. Write documents, do not write code." \
  --body "$(cat <<'EOF'
Your job is open. Two rules for this wave.

1. Produce a document. Not code. A document I can read in ten minutes and act on,
   or tell you it is wrong. Commit it to your own branch and mail the team leader
   the path and a two-line summary. A wrong document costs one read; a wrong
   implementation costs a day.

2. Cite. Every claim about what the code we replace actually does needs a file
   path and a line number. If you are reasoning from memory of how a format
   works rather than from the source in front of you, say so in the document.
   I would much rather read "I believe X but could not confirm it" than a
   confident paragraph built on a guess.

What I most need from wave 1, in priority order:

  a. sevenz-rust2 0.23.0's block decode stack is private. Without it, a solid
     7Z block costs one decode per entry and 400 entries took 15.5 seconds. Tell
     me the minimum patch that exposes it, and tell me whether the result can
     stop mid-block and resume, because streaming needs that. This is the single
     highest-value thing anyone does this wave.
  b. The three unmerged performance fixes on the oxiarc fork at 93b17af. They are
     prior art from the maintainer's own repo and we must carry them forward
     rather than rediscover them. Name the files and say what changes in our
     code to carry each across.
  c. What the app's Kotlin actually needs from the boundary, and what has to
     change there so an archive can be held open across per-file reads.

I am writing the core reader and writer myself while you do this. If you find
something that changes what I am about to build, mail me and everyone at once
rather than waiting for the end of your run.
EOF
)"

echo "--- wave 1 dispatched ---"

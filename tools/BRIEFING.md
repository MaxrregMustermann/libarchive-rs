# Briefing: you are an agent in a software collective

You are one of thirteen agents working on `libarchive-rs`, a Rust library that replaces
`oxiarc` and `unarc-rs` inside an Android file manager. You run in your own git worktree on
your own branch. Nothing you write reaches `main` until the orchestrator has merged it and run
the tests, so you cannot break anyone. You can, however, waste a lot of work if you do not
understand the shape of the project first.

## What we are building

`libarchive-rs` reads and writes about twenty-five archive and compression formats for an
Android file manager: ZIP, 7Z, TAR and its compressed forms, GZ, BZ2, XZ, Z, LZ4, Zstd, and the
legacy long tail (RAR, ARJ, ACE, ARC, Zoo, SQ, UCB, UC2, Ha, Hyp, Ice). It has to be as fast as
the code it replaces, and it must never panic, because a panic crossing the JNI boundary
crashes the app instead of raising a Java exception.

Two properties are the whole point of the library and everything else is in service of them:

1. **Streaming.** `ArchiveReader::open_entry` returns a `Read`. It must not materialise an entry.
   The code it replaces decompressed an entire entry into a `Vec` before the caller saw one byte,
   which cost twice the entry's size in memory and about a second of latency on a 132 MB file.
2. **The archive stays open.** Entries are addressed by index into an archive that is held open
   across reads. This is what makes a solid 7Z block cost one decode instead of one per entry.
   Reading entry *n* of a solid block by name, as `sevenz-rust2`'s `read_file` does, decodes the
   block from its start every time and is quadratic: 400 entries took 15.5 seconds where the
   equivalent non-solid data took 204 milliseconds.

## How the collective works

Everything is files under `coord/` in the repository. There is no server and no daemon.

    python3 tools/collective.py mail inbox <you>            # your mail
    python3 tools/collective.py mail read <you>              # mark it read
    python3 tools/collective.py mail send --from <you> \
        --to <them> --subject "..." --body "..."            # send
    python3 tools/collective.py job show J-0007              # your job
    python3 tools/collective.py job state J-0007 done --note "..."

Your team is in `coord/teams/<team>/CHARTER.md`. Your team's leader merges your work and is
accountable for it. Read your charter. If your charter is wrong, or the job you were given does
not fit it, say so in mail rather than improvising — the orchestrator reads that mail.

Rules that are not negotiable:

- **Write acceptance criteria into the job before you start**, and check yourself against them.
- **Never merge to `main`.** Commit to your own branch.
- **Never weaken a test to make it pass.** If a test is wrong, fix the test and say why in mail.
- **If you cannot do the job, say so in mail and stop.** A clear "this is not achievable because
  X" is worth more to this project than a plausible-looking partial implementation.
- **Never use `unwrap()`, `expect()` or `panic!` outside a test.** The crate denies these lints.
- **Send mail when you finish, when you are blocked, and when you learn something the next person
  needs.** A finding that only exists in your transcript is a finding that is lost.

## The environment

    export PATH="$HOME/.cargo/bin:$PATH"
    export JAVA_HOME=/opt/jdk/jdk-21.0.12.1+1
    CARGO_TARGET_DIR=/home/user/cargo-target/<something-unique-to-you>

`CARGO_TARGET_DIR` must be under `/home/user` and must be your own, or you will fight other
agents over the same build directory. `/tmp` is a small nearly-full tmpfs; do not build there.

The reading of the code we replace is on this machine, unpacked, if you need to look at it:

    /home/user/oxiarc-prior      the oxiarc fork at 93b17af, which carries three unmerged
                                performance fixes we must carry forward
    /home/user/TheFileManager    the Android app, branch master, at a7c6b84a
    ~/.cargo/registry/src/*/     the sources of every dependency, including unarc-rs 0.7.2 and
                                sevenz-rust2 0.23.0

## Where the truth lives

`libarchive/src/lib.rs` documents what the library is for and why the API is shaped this way.
Read it first, every time, whatever else you do. If you find yourself about to change the public
API, read it first and then write to the leader of `core` and say what you changed and why.

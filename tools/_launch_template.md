Before you write anything, do these three things. They are not warm-up; they are how you avoid
wasting the whole run.

1. `cd` is already your worktree — you are in `/home/user/wt/<your name>`, on your own branch.
   `main` is the integration branch and you must never merge to it.
2. Build here: `export PATH="$HOME/.cargo/bin:$PATH"` and
   `export CARGO_TARGET_DIR={TARGET}`. That directory is yours alone and is already warm; another
   agent is building in a different one. If you build anywhere else you will slow down every agent
   on the machine.
3. `cargo test --workspace --offline` takes about two minutes and everything must pass before you
   change anything. If it does not pass on a clean checkout, that is a finding worth more than your
   job — stop and mail it.

Then do the job. The acceptance criteria above are the contract; the orchestrator checks them
rather than your account of them, so an unmet one is worth telling them about.

When you finish:

- Commit to your own branch. A new commit, never `amend` or `rebase` onto something already pushed.
- Send mail. `python3 tools/collective.py mail send --from <you> --to <them> --subject ... --body ...`.
  Who "them" is depends on what you did: the team leader for work you finished, the orchestrator
  for anything blocked or surprising. Send mail when you find something too, not only when you are
  done — the next agent may be about to repeat your mistake.
- Close the job: `python3 tools/collective.py job state <job> done --note "<one line>"`, or
  `blocked` with the reason.

Two things that are worth more here than a finished feature:

**A measurement.** If you claim something got faster, smaller or more correct, the number and the
command that produces it belong in a benchmark or a test in the repository. This project has
already shipped a benchmark asserting a performance property that had never been compiled.

**A test that fails before your change and passes after it.** That is what makes the work checkable
by someone who was not there. A test that passes both before and after is a test of nothing, and is
worse than no test because it looks like evidence.

{TASK}

{note}

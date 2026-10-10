#!/usr/bin/env python3
"""Merge an agent's branch into main, safely, and say what happened.

    python3 tools/merge.py <agent> [--dry-run]

Agents branch from whatever `main` was when they were launched, which is rarely what `main` is
now. Merging naively therefore produces conflicts in files the agent never touched — the
coordination tooling in particular, which lives in the repository and is rewritten every few
commits. This rebases the agent's commits onto current `main` first, so the diff the agent
actually wrote is what gets reviewed, and refuses rather than guesses when that rebase needs a
decision.

It never merges to a branch other than `main`, and it never runs the tests: the orchestrator runs
the tests after merging, and a merge script that also runs a build is a merge script that
sometimes half-merges.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def git(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args], cwd=REPO, check=check, capture_output=True, text=True
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("agent")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--land", action="store_true", help="rebase, merge and verify")
    args = parser.parse_args()

    if args.land:
        land(args.agent)
        return

    branch = f"agent/{args.agent}"
    if git("rev-parse", "--verify", branch, check=False).returncode != 0:
        sys.exit(f"no such branch: {branch}")

    base = git("merge-base", branch, "main").stdout.strip()
    commits = git("log", "--oneline", f"{base}..{branch}").stdout.strip()
    if not commits:
        print(f"{branch} has nothing that is not already on main")
        return

    print(f"{branch} is {len(commits.splitlines())} commit(s) ahead of its fork point:")
    for line in commits.splitlines():
        print(f"  {line}")
    diff = git("diff", "--stat", f"{base}..{branch}").stdout.strip()
    print("\n" + diff + "\n")

    if args.dry_run:
        return

    if git("status", "--porcelain").stdout.strip():
        sys.exit("main's working tree is dirty; commit or stash it before merging")

    # Rebasing onto main rather than merging main in: a merge would record the divergence and
    # every future merge would have to carry it. A rebase leaves a linear history where each
    # commit is one agent's contribution, which is the only way to read `git log` later.
    result = git("rebase", "main", branch, check=False)
    if result.returncode != 0:
        print("rebase needs a decision; resolve by hand:")
        print(result.stdout + result.stderr)
        sys.exit(1)

    print(f"rebased {branch} onto main; it is now a fast-forward")
    print(f"\nTo land it:\n    git checkout main && git merge --ff-only {branch} && git push origin main")


def land(agent: str) -> None:
    """Rebase, land, and verify that nothing already on `main` was dropped.

    The orchestrator lost three merged documents this way once: a commit was made on the merge
    branch instead of on `main`, `main` was then fast-forwarded to a rebase whose parent predated
    that commit, and the documents vanished with no conflict and no error. Git is perfectly happy
    to do this, which is the problem — nothing about the command sequence looks wrong.

    So landing checks three things, and the third is the one that would have caught it:

      1. `main` is clean;
      2. the rebase's merge base really is the current `main`, so the fast-forward is honest;
      3. afterwards, every path that existed on `main` before still exists, and every commit that
         was reachable from `main` before is still reachable.
    """
    branch = f"agent/{agent}"
    before = git("rev-parse", "main").stdout.strip()
    paths_before = set(git("ls-tree", "-r", "--name-only", before).stdout.splitlines())

    if git("status", "--porcelain").stdout.strip():
        sys.exit("main's working tree is dirty; commit or stash it before landing")

    if git("checkout", "-B", f"land/{agent}", branch).returncode != 0:
        sys.exit(f"could not check out {branch}")
    rebased = git("rebase", "main")
    if rebased.returncode != 0:
        git("checkout", "main")
        sys.exit("rebase needs a decision; resolve by hand:\n" + rebased.stdout + rebased.stderr)

    base = git("merge-base", f"land/{agent}", "main").stdout.strip()
    if base != before:
        git("checkout", "main")
        sys.exit(
            f"refusing to land: land/{agent}'s merge base is {base[:8]} but main was {before[:8]}. "
            "A fast-forward would drop whatever landed on main in between."
        )

    git("checkout", "main")
    if git("merge", "--ff-only", f"land/{agent}").returncode != 0:
        sys.exit("merge failed; main has been left alone")

    after_paths = set(git("ls-tree", "-r", "--name-only", "HEAD").stdout.splitlines())
    lost = sorted(paths_before - after_paths)
    unreachable = git("log", "--oneline", f"HEAD..{before}").stdout.strip()
    if unreachable or lost:
        git("reset", "--hard", before)
        detail = unreachable + ("\n  deleted paths:\n  " + "\n  ".join(lost) if lost else "")
        sys.exit("refusing to land: the merge would lose work.\n" + detail)

    head = git("rev-parse", "--short", "HEAD").stdout.strip()
    print(f"landed {branch} as {head}")
    print(f"verified: all {len(paths_before)} paths and every prior commit survived")


if __name__ == "__main__":
    main()

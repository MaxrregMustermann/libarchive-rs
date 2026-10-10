#!/usr/bin/env python3
"""Launch an agent on its job, in its own worktree, with a warm build directory.

`collective.py run` starts the process. This adds the three things that make a launch
actually work rather than merely start:

  * a per-agent `CARGO_TARGET_DIR`, told to the agent explicitly, because the default
    is shared and six agents fighting over one build directory makes every build slow
    and occasionally wrong;
  * a seed of that directory from an already-built one, by hard link, so the agent
    does not spend its first two minutes recompiling `zstd` before it can think;
  * a prompt that says what to do *and* what not to do, which is the only place the
    orchestrator's intent can reach the agent.

    tools/launch.py <agent> <job> "<what to do>"
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import collective  # noqa: E402

TARGETS = Path("/home/user/cargo-target")
SEED = TARGETS / "orch"


def target_dir(agent: str) -> Path:
    return TARGETS / agent


def seed(agent: str) -> str:
    """Hard-link an already-built target directory into the agent's own.

    `cp -al` rather than `cp -a`: the files are large and identical, and hard links
    cost one inode each. Cargo is happy to share them because it replaces rather than
    writes in place; the risk of two cargos writing the same inode is avoided by the
    fact that only one agent ever builds in a given directory.
    """
    dest = target_dir(agent)
    if dest.exists():
        return f"existing {dest}"
    if not SEED.is_dir():
        return f"no seed at {SEED}, agent will build cold"
    dest.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cp", "-al", str(SEED), str(dest)], check=True)
    return f"seeded {dest} from {SEED}"


def main() -> None:
    agent, job, task = sys.argv[1], sys.argv[2], sys.argv[3]
    note = sys.argv[4] if len(sys.argv) > 4 else ""
    warm = seed(agent)
    body = Path("tools/_launch_template.md").read_text()
    body = body.replace("{TASK}", task)
    body = body.replace("{TARGET}", str(target_dir(agent)))
    path = Path("tools/_launch_prompt.md")
    path.write_text(body)
    try:
        args = subprocess.run(
            [
                "python3",
                "tools/collective.py",
                "run",
                agent,
                "--job",
                job,
                "--prompt-file",
                str(path),
                "--note",
                note,
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        print(f"{args.stdout.strip()}  [{warm}]")
    finally:
        path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()

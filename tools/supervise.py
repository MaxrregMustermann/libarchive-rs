#!/usr/bin/env python3
"""Run an agent, and keep running it until it does real work.

    python3 tools/supervise.py cairn --job J-0040 --prompt-file /home/user/wave5/prompt-cairn.md

The collective's free models sit behind a provider that returns 503s and "Unexpected server error"
under load. `collective.py run` launches one attempt and walks away, so a single overload response
ends the agent's session before it has read a single file - and the run looks like a finished run,
which is how a wave ends up with three agents that reported nothing.

This supervises instead. It launches, waits, and reads the log to decide what happened:

  * real work        - the transcript shows the agent acting and no transport error; done.
  * transport error  - 503, overloaded, stream failed, unknown error; back off, and on repeated
                       failure rotate to another model that has answered recently.
  * dead model       - deprecated or unavailable; rotate rather than retry.

It never treats a transport error as the agent's answer, and it never retries a run that actually
did the work. Attempts and their outcomes go in `coord/supervisor/<agent>.log` so that a human can
read why an agent took four attempts.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
COORD = Path("/home/user/coord")

# Models that have answered recently. Rotating among these beats retrying one that is overloaded.
# Measured by probing each with a trivial prompt at 02:5x, not taken from the registry: the
# standing `models.json` said muse-spark was fine and it was returning "Model unavailable".
ROTATION = [
    "opencode/nemotron-3.5-lightning-free",
    "opencode/longcat-2.5-preview-free",
    "opencode/space-bunny-free",
    "opencode/big-pickle",
    "opencode/mimo-v2.6-flash-free",
]

# The provider telling us it is busy. None of these are the agent's fault and none are an answer.
TRANSPORT = (
    "unexpected server error",
    "upstream error",
    "service temporarily overloaded",
    "streaming response failed",
    "upstream request failed",
    "error sending request",
    "rate limit",
    "context length exceeded",
    "idle timeout",
)
# The model itself is gone. Retrying is pointless; rotate.
DEAD_MODEL = ("model has been deprecated", "model is unavailable", "endpoint is unavailable")

ACTIONS = ('\n$ ', "> build", "> plan", "tool", "Tool")


def verdict(log: Path) -> str:
    """`work`, `transport`, `dead-model` or `empty`, from the run's transcript."""
    try:
        text = log.read_text(errors="replace")
    except OSError:
        return "empty"
    tail = text[-6000:].lower()
    low = text.lower()
    if any(m in low for m in DEAD_MODEL):
        return "dead-model"
    if any(m in tail for m in TRANSPORT):
        return "transport"
    # An attempt that never produced an action line did nothing at all.
    if sum(text.count(a) for a in ACTIONS) < 2:
        return "empty"
    return "work"


def set_model(agent: str, model: str) -> None:
    subprocess.run(
        [sys.executable, str(REPO / "tools" / "collective.py"), "agent", "set-model",
         agent, "--model", model],
        cwd=REPO, capture_output=True, text=True,
    )


def registry_model(agent: str) -> str:
    data = json.loads((COORD / "registry.json").read_text())
    return data["agents"][agent]["model"]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("agent")
    ap.add_argument("--job", default=None)
    ap.add_argument("--briefing", default=None)
    ap.add_argument("--prompt", default=None)
    ap.add_argument("--prompt-file", default=None)
    ap.add_argument("--note", default="")
    ap.add_argument("--max-attempts", type=int, default=6)
    ap.add_argument("--timeout", type=int, default=5400, help="seconds to allow one attempt")
    args = ap.parse_args()

    journal = COORD / "supervisor"
    journal.mkdir(parents=True, exist_ok=True)
    log = journal / f"{args.agent}.log"

    def say(message: str) -> None:
        line = f"{time.strftime('%H:%M:%S')} {message}"
        print(line, flush=True)
        with log.open("a") as h:
            h.write(line + "\n")

    for attempt in range(1, args.max_attempts + 1):
        cmd = [sys.executable, str(REPO / "tools" / "collective.py"), "run", args.agent,
               "--note", args.note]
        if args.job:
            cmd += ["--job", args.job]
        if args.briefing:
            cmd += ["--briefing", args.briefing]
        if args.prompt:
            cmd += ["--prompt", args.prompt]
        if args.prompt_file:
            cmd += ["--prompt-file", args.prompt_file]

        started = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True).stdout.strip()
        say(f"attempt {attempt}: {started}")
        if not started:
            say("dispatcher did not launch; check the registry")
            return

        run_log = Path(started.split("log=")[-1])
        pid = int(started.split("pid=")[-1].split()[0])

        deadline = time.time() + args.timeout
        while time.time() < deadline:
            try:
                os_kill = subprocess.run(["kill", "-0", str(pid)], capture_output=True)
                if os_kill.returncode != 0:
                    break
            except OSError:
                break
            time.sleep(5)

        state = verdict(run_log)
        say(f"attempt {attempt} verdict: {state} ({run_log.name})")
        if state == "work":
            say(f"{args.agent} did work; stopping")
            return

        if state == "dead-model":
            current = registry_model(args.agent)
            for candidate in ROTATION:
                if candidate != current:
                    say(f"model {current} is gone; rotating to {candidate}")
                    set_model(args.agent, candidate)
                    break
        else:
            # Overload usually clears in seconds. Rotate only once repeated attempts fail.
            if attempt >= 2:
                current = registry_model(args.agent)
                for candidate in ROTATION:
                    if candidate != current:
                        say(f"rotating off {current} after {attempt} transport failures")
                        set_model(args.agent, candidate)
                        break
            back = min(20 * attempt, 90)
            say(f"backing off {back}s")
            time.sleep(back)

    say(f"gave up after {args.max_attempts} attempts; the agent did not get to work")


if __name__ == "__main__":
    main()
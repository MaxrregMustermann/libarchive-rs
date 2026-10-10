#!/usr/bin/env python3
"""Probe every model in the `opencode/` provider and report whether it answers.

The collective loses agents silently when a model is deprecated, rate-limited or
timing out, and a dead model looks exactly like a slow agent unless somebody
watches for it. This runs one trivial prompt per model, concurrently, and writes
the result to `coord/models.json` so the dispatcher can pick a live one.

    python3 tools/modelcheck.py                 # probe all
    python3 tools/modelcheck.py --model X --model Y
    python3 tools/modelcheck.py --show          # print the last result
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import subprocess
import time
from pathlib import Path

COORD = Path(os.environ.get("COLLECTIVE_COORD", "/home/user/coord"))
RESULTS = COORD / "models.json"

MODELS = [
    "opencode/nemotron-3-ultra-free",
    "opencode/nemotron-3.5-lightning-free",
    "opencode/muse-spark-1.3-contributor-free",
    "opencode/longcat-2.5-preview-free",
    "opencode/big-pickle",
    "opencode/step-5-preview-free",
    "opencode/space-bunny-free",
    "opencode/mimo-v2.6-flash-free",
    "opencode/exo-free",
    "opencode/ling-3.1-flash-free",
    "opencode/ling-3.0-flash-fin-free",
]

# The probe has to be cheap but non-trivial: "say OK" can be answered from a
# cached completion, and a model that answers that but not real work is no use.
PROBE = (
    "Answer with exactly one word and nothing else: what is the capital of France?"
)


def probe(model: str, timeout: int) -> dict:
    started = time.time()
    try:
        proc = subprocess.run(
            ["opencode", "run", "--model", model, "--agent", "build", PROBE],
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=str(Path(__file__).resolve().parent.parent),
        )
    except subprocess.TimeoutExpired:
        return {"model": model, "ok": False, "why": "timeout", "seconds": timeout}
    elapsed = round(time.time() - started, 1)
    out = (proc.stdout or "") + (proc.stderr or "")
    low = out.lower()
    if proc.returncode == 0 and "paris" in low:
        return {"model": model, "ok": True, "why": "", "seconds": elapsed}
    reason = ""
    for needle, why in (
        ("deprecated", "deprecated"),
        ("unavailable", "unavailable"),
        ("idle timeout", "504 idle timeout"),
        ("endpoint is unavailable", "endpoint unavailable"),
        ("rate limit", "rate limited"),
        ("429", "rate limited"),
        ("401", "unauthorised"),
        ("402", "out of credit"),
        ("no available", "no capacity"),
    ):
        if needle in low:
            reason = why
            break
    if not reason:
        reason = (out.strip().splitlines() or ["no output"])[-1][:160]
    return {"model": model, "ok": False, "why": reason, "seconds": elapsed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", action="append", dest="models")
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--show", action="store_true")
    args = parser.parse_args()

    if args.show:
        print(RESULTS.read_text() if RESULTS.is_file() else "no probe has been run yet")
        return

    models = args.models or MODELS
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(models)) as pool:
        results = list(pool.map(lambda m: probe(m, args.timeout), models))

    RESULTS.parent.mkdir(parents=True, exist_ok=True)
    existing = {}
    if RESULTS.is_file():
        try:
            existing = {r["model"]: r for r in json.loads(RESULTS.read_text())}
        except json.JSONDecodeError:
            existing = {}
    for result in results:
        existing[result["model"]] = result
    RESULTS.write_text(json.dumps(list(existing.values()), indent=2) + "\n")

    live = [r["model"] for r in results if r["ok"]]
    for result in sorted(results, key=lambda r: (not r["ok"], r["seconds"])):
        mark = "LIVE " if result["ok"] else "DEAD "
        print(f"{mark} {result['model']:46} {result['seconds']:>6}s  {result['why']}")
    print(f"\n{len(live)}/{len(results)} live")


if __name__ == "__main__":
    main()

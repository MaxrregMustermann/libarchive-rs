#!/usr/bin/env python3
"""The collective's coordination substrate: mail, teams, jobs and a dispatcher.

Every agent has an inbox. Work arrives as jobs. Teams have a leader who is
responsible for their members' work landing. Everything is a file under
``coord/`` so that a human can read the whole history of the project with
``ls`` and ``cat``, and so that an agent which has been launched with a single
prompt can still take part without any daemon.

    collective.py init
    collective.py team add core --leader ada --charter "..."
    collective.py agent add ada --team core --model opencode/... --role leader
    collective.py mail send --from ada --to bob --subject "..." --body "..."
    collective.py mail inbox bob
    collective.py job new --title "..." --team core
    collective.py job assign J-0001 bob
    collective.py run bob --note "check your inbox"
    collective.py status
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# The mail store lives outside the repository so that every agent's git worktree shares one copy.
# A directory committed into the repo would be checked out separately into each worktree, and six
# agents would then be reading six different inboxes.
COORD = Path(os.environ.get("COLLECTIVE_COORD", "/home/user/coord"))
WORKTREES = Path(os.environ.get("COLLECTIVE_WORKTREES", "/home/user/wt"))
REGISTRY = COORD / "registry.json"

NAME_RE = re.compile(r"^[a-z][a-z0-9_]{1,20}$")


def die(message: str) -> "None":
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S")


def load_registry() -> dict:
    if not REGISTRY.is_file():
        return {"agents": {}, "teams": {}, "jobs": {}}
    with REGISTRY.open() as handle:
        return json.load(handle)


def save_registry(registry: dict) -> None:
    tmp = REGISTRY.with_suffix(".json.tmp")
    with tmp.open("w") as handle:
        json.dump(registry, handle, indent=2, sort_keys=True)
        handle.write("\n")
    tmp.replace(REGISTRY)


def bump(registry: dict, kind: str) -> int:
    """Counts up and returns the new value, for the message and job id sequences."""
    counters = registry.setdefault("_counters", {})
    counters[kind] = counters.get(kind, 0) + 1
    return counters[kind]


def agent_dir(name: str) -> Path:
    return COORD / "mail" / name


def check_name(name: str) -> None:
    if not NAME_RE.match(name):
        die(f"{name!r} is not a valid agent or team name: lowercase, 2-21 chars, [a-z0-9_]")


# ---------------------------------------------------------------- scaffolding


def cmd_init(args: argparse.Namespace) -> None:
    for path in (
        COORD / "mail",
        COORD / "teams",
        COORD / "jobs",
        COORD / "runs",
        COORD / "worktrees",
    ):
        path.mkdir(parents=True, exist_ok=True)
    readme = COORD / "README.md"
    if not readme.exists():
        readme.write_text(PROTOCOL)
    # The briefing is kept in the repository as well, so that a fresh checkout carries it, but
    # coord/ is not versioned and this directory may have been created from nothing.
    briefing = COORD / "BRIEFING.md"
    if not briefing.exists():
        briefing.write_text(BRIEFING)
    if not load_registry().get("_counters"):
        registry = load_registry()
        registry["_counters"] = {"mail": 0, "job": 0, "run": 0}
        save_registry(registry)
    print(f"collective initialised at {COORD}")


# ---------------------------------------------------------------------- teams


def cmd_team_add(args: argparse.Namespace) -> None:
    check_name(args.name)
    registry = load_registry()
    teams = registry.setdefault("teams", {})
    if args.name in teams:
        die(f"team {args.name} already exists")
    teams[args.name] = {
        "charter": args.charter or "",
        "leader": args.leader,
        "members": [args.leader] if args.leader else [],
        "formed": now(),
    }
    save_registry(registry)
    charter = COORD / "teams" / args.name / "CHARTER.md"
    charter.parent.mkdir(parents=True, exist_ok=True)
    charter.write_text(
        f"# Team {args.name}\n\n"
        f"Leader: {args.leader or '(none yet)'}\n\n"
        f"## Charter\n\n{args.charter or 'Not written yet.'}\n\n"
        f"## Members\n\n"
        + "\n".join(f"- {m}" for m in teams[args.name]["members"])
        + "\n"
    )
    print(f"team {args.name} formed, leader {args.leader}")


def cmd_team_join(args: argparse.Namespace) -> None:
    registry = load_registry()
    team = registry.get("teams", {}).get(args.team)
    if team is None:
        die(f"no such team: {args.team}")
    if args.agent not in team["members"]:
        team["members"].append(args.agent)
    if team.get("leader") == args.agent:
        print(f"{args.agent} leads {args.team}")
    save_registry(registry)
    charter = COORD / "teams" / args.team / "CHARTER.md"
    body = charter.read_text() if charter.exists() else f"# Team {args.team}\n"
    body = body.rstrip("\n") + f"\n- {args.agent}\n"
    charter.write_text(body)
    print(f"{args.agent} joined {args.team}")


# --------------------------------------------------------------------- agents


def cmd_agent_add(args: argparse.Namespace) -> None:
    check_name(args.name)
    registry = load_registry()
    agents = registry.setdefault("agents", {})
    if args.name in agents:
        die(f"agent {args.name} already exists")
    agents[args.name] = {
        "team": args.team,
        "model": args.model,
        "role": args.role,
        "branch": f"agent/{args.name}",
        "worktree": str(WORKTREES / args.name),
        "joined": now(),
    }
    save_registry(registry)
    for box in ("inbox", "sent"):
        (agent_dir(args.name) / box).mkdir(parents=True, exist_ok=True)
    print(f"agent {args.name} added to {args.team} as {args.role} on {args.model}")


def cmd_agent_set_model(args: argparse.Namespace) -> None:
    registry = load_registry()
    agent = registry.get("agents", {}).get(args.name)
    if agent is None:
        die(f"no such agent: {args.name}")
    agent["model"] = args.model
    save_registry(registry)
    print(f"{args.name} now runs {args.model}")


# ----------------------------------------------------------------------- mail


def cmd_mail_send(args: argparse.Namespace) -> None:
    registry = load_registry()
    bump(registry, "mail")
    message_id = f"M-{registry['_counters']['mail']:04d}"
    recipients = [r.strip() for r in args.to.split(",") if r.strip()]
    unknown = [r for r in recipients if r not in registry.get("agents", {})]
    if unknown:
        die(f"unknown recipient(s): {', '.join(unknown)}")
    if args.body_file:
        body = Path(args.body_file).read_text()
    elif args.body:
        body = args.body
    else:
        body = sys.stdin.read()
    text = (
        "---\n"
        f"id: {message_id}\n"
        f"from: {args.sender}\n"
        f"to: [{', '.join(recipients)}]\n"
        f"cc: [{', '.join(args.cc.split(',') if args.cc else [])}]\n"
        f"subject: {args.subject}\n"
        f"date: {now()}\n"
        f"job: {args.job or '-'}\n"
        "---\n\n"
        f"{body.strip()}\n"
    )
    (agent_dir(args.sender) / "sent").mkdir(parents=True, exist_ok=True)
    (agent_dir(args.sender) / "sent" / f"{message_id}.md").write_text(text)
    for recipient in recipients:
        (agent_dir(recipient) / "inbox").mkdir(parents=True, exist_ok=True)
        (agent_dir(recipient) / "inbox" / f"{message_id}.md").write_text(text)
    save_registry(registry)
    print(f"{message_id} {args.sender} -> {', '.join(recipients)}: {args.subject}")


def cmd_mail_inbox(args: argparse.Namespace) -> None:
    inbox = agent_dir(args.agent) / "inbox"
    if not inbox.is_dir():
        die(f"{args.agent} has no inbox yet")
    messages = sorted(inbox.glob("*.md"))
    unread = [m for m in messages if not (m.with_suffix(".read")).exists()]
    selected = unread if args.unread else messages
    if not selected:
        print(f"{args.agent}: inbox empty")
        return
    for path in selected:
        print(path.read_text())
        print("=" * 70)


def cmd_mail_read(args: argparse.Namespace) -> None:
    inbox = agent_dir(args.agent) / "inbox"
    targets = sorted(inbox.glob("*.md"))
    if args.id:
        targets = [inbox / f"{args.id}.md"]
    for path in targets:
        if not path.exists():
            die(f"no such message: {path}")
        path.with_suffix(".read").write_text(path.read_text())
    print(f"{args.agent} marked {len(targets)} message(s) read")


def cmd_mail_address(args: argparse.Namespace) -> None:
    registry = load_registry()
    agent = registry.get("agents", {}).get(args.agent)
    if agent is None:
        die(f"no such agent: {args.agent}")
    team = registry.get("teams", {}).get(agent["team"], {})
    leader = team.get("leader")
    inbox = agent_dir(args.agent) / "inbox"
    messages = sorted(inbox.glob("*.md"))
    unread = [m for m in messages if not m.with_suffix(".read").exists()]
    print(
        json.dumps(
            {
                "agent": args.agent,
                "team": agent["team"],
                "role": agent["role"],
                "model": agent["model"],
                "leader": leader,
                "members": team.get("members", []),
                "worktree": agent["worktree"],
                "branch": agent["branch"],
                "unread": [m.stem for m in unread],
                "jobs": [
                    job
                    for job, data in registry.get("jobs", {}).items()
                    if data.get("assignee") == args.agent and data.get("state") != "done"
                ],
            },
            indent=2,
        )
    )


# ------------------------------------------------------------------------ jobs


def cmd_job_new(args: argparse.Namespace) -> None:
    registry = load_registry()
    bump(registry, "job")
    job_id = f"J-{registry['_counters']['job']:04d}"
    registry.setdefault("jobs", {})[job_id] = {
        "title": args.title,
        "team": args.team,
        "state": "open",
        "assignee": None,
        "opened_by": args.opened_by,
        "opened": now(),
        "acceptance": args.acceptance or "",
        "notes": [],
    }
    save_registry(registry)
    path = COORD / "jobs" / f"{job_id}.json"
    path.write_text(json.dumps(registry["jobs"][job_id], indent=2) + "\n")
    print(job_id)


def cmd_job_assign(args: argparse.Namespace) -> None:
    registry = load_registry()
    job = registry.get("jobs", {}).get(args.job)
    if job is None:
        die(f"no such job: {args.job}")
    job["assignee"] = args.agent
    job["state"] = "assigned"
    save_registry(registry)
    (COORD / "jobs" / f"{args.job}.json").write_text(json.dumps(job, indent=2) + "\n")
    print(f"{args.job} assigned to {args.agent}")


def cmd_job_state(args: argparse.Namespace) -> None:
    registry = load_registry()
    job = registry.get("jobs", {}).get(args.job)
    if job is None:
        die(f"no such job: {args.job}")
    if args.note:
        job.setdefault("notes", []).append({"at": now(), "by": args.agent, "note": args.note})
    job["state"] = args.state
    if args.state == "done":
        job["closed"] = now()
    save_registry(registry)
    (COORD / "jobs" / f"{args.job}.json").write_text(json.dumps(job, indent=2) + "\n")
    print(f"{args.job} is now {args.state}")


def cmd_job_show(args: argparse.Namespace) -> None:
    registry = load_registry()
    job = registry.get("jobs", {}).get(args.job)
    if job is None:
        die(f"no such job: {args.job}")
    print(json.dumps(job, indent=2))


# -------------------------------------------------------------------- running


def ensure_worktree(agent: str, registry: dict) -> Path:
    """Gives an agent its own git worktree with the mail directory linked in.

    Separate worktrees are what let several agents work at once without three
    of them writing the same file. The symlink is what lets an agent reach the
    collective's shared state with a relative path, which survives being run
    from anywhere.
    """
    path = WORKTREES / agent
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        branch = registry["agents"][agent]["branch"]
        subprocess.run(
            ["git", "worktree", "add", "-B", branch, str(path), "main"],
            cwd=REPO,
            check=True,
        )
    link = path / "coord"
    if link.is_dir() and not link.is_symlink():
        # The checkout materialised a real directory, which happens when the
        # worktree predates coord being moved out of the repository.
        subprocess.run(["rm", "-rf", str(link)], check=True)
    if not link.exists() and not link.is_symlink():
        link.symlink_to(COORD)
    return path


def cmd_run(args: argparse.Namespace) -> None:
    registry = load_registry()
    agent = registry.get("agents", {}).get(args.agent)
    if agent is None:
        die(f"no such agent: {args.agent}")
    worktree = ensure_worktree(args.agent, registry)
    bump(registry, "run")
    run_id = f"R-{registry['_counters']['run']:04d}"
    save_registry(registry)
    log = COORD / "runs" / f"{run_id}-{args.agent}.log"
    briefing = (COORD / "BRIEFING.md").read_text()
    job = ""
    if args.job:
        data = registry.get("jobs", {}).get(args.job)
        if data is None:
            die(f"no such job: {args.job}")
        job = (
            f"\n\n# Your job {args.job}\n\n{data['title']}\n\n"
            f"Acceptance criteria:\n{data.get('acceptance') or '(none written — write some first)'}\n"
        )
    task = Path(args.prompt_file).read_text() if args.prompt_file else args.prompt
    prompt = f"{briefing}{job}\n\n# What to do right now\n\n{task}"
    if args.note:
        prompt += f"\n\nOrchestrator note: {args.note}"
    command = [
        "opencode",
        "run",
        "--model",
        agent["model"],
        "--dir",
        str(worktree),
        "--agent",
        args.opencode_agent,
        prompt,
    ]
    with log.open("w") as handle:
        handle.write(f"$ {' '.join(command)}\n\n")
        handle.flush()
        process = subprocess.Popen(
            command,
            cwd=worktree,
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    (COORD / "runs" / f"{run_id}-{args.agent}.pid").write_text(f"{process.pid}\n")
    print(f"{run_id} {args.agent} pid={process.pid} log={log}")


def cmd_status(args: argparse.Namespace) -> None:
    registry = load_registry()
    print("== teams ==")
    for name, team in sorted(registry.get("teams", {}).items()):
        print(f"  {name:10} leader={team['leader'] or '-':10} members={','.join(team['members'])}")
    print("== agents ==")
    for name, agent in sorted(registry.get("agents", {}).items()):
        inbox = agent_dir(name) / "inbox"
        unread = (
            len([m for m in inbox.glob("*.md") if not m.with_suffix(".read").exists()])
            if inbox.is_dir()
            else 0
        )
        print(f"  {name:10} {agent['role']:7} {agent['model']:42} unread={unread}")
    print("== jobs ==")
    for job_id, job in sorted(registry.get("jobs", {}).items()):
        print(
            f"  {job_id} {job['state']:9} {str(job.get('assignee') or '-'):10} {job['title']}"
        )
    print("== live runs ==")
    for pidfile in sorted((COORD / "runs").glob("*.pid")):
        try:
            pid = int(pidfile.read_text().strip())
        except ValueError:
            continue
        alive = os.path.exists(f"/proc/{pid}")
        print(f"  {pidfile.stem:28} pid={pid:<8} {'running' if alive else 'finished'}")


BRIEFING = r"""
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

"""

PROTOCOL = """# The collective

This directory is the whole of the coordination substrate. It is deliberately a
set of plain files: an agent is launched with one prompt and no daemon, and
still takes part, and a human can read the entire history with `ls` and `cat`.

    coord/registry.json          agents, teams, jobs, counters
    coord/mail/<agent>/inbox/    mail delivered to an agent
    coord/mail/<agent>/sent/     mail an agent has sent
    coord/teams/<team>/CHARTER.md what a team is for and who is in it
    coord/jobs/<job>.json        a unit of work, with its acceptance criteria
    coord/runs/<run>.log         the transcript of one agent run

## Mail

A message is one file with a YAML-ish header:

    ---
    id: M-0042
    from: ada
    to: [grace]
    cc: []
    subject: the 7z decode stack
    date: 2026-01-01T00:00:00
    job: J-0007
    ---

Use it with `tools/collective.py mail ...`. Reading a message creates a
`.read` marker beside it, so an agent that is re-launched does not reprocess
mail it has already seen.

## Teams

A team has a charter and a leader. The leader is responsible for their
members' work landing, and is the one the orchestrator merges. An agent may
join an existing team or form a new one; both are one command and both are
recorded.

## Jobs

A job has an acceptance criterion written down *before* the work starts. An
agent that cannot meet the criterion should say so in mail rather than commit
something that only looks finished. The orchestrator checks the criterion, not
the agent's account of it.

## Running

    tools/collective.py run <agent> --note "..."

launches `opencode run` in that agent's own git worktree, on that agent's own
branch. Agents never share a working tree, which is what lets several work at
once. Nothing lands on `main` until the orchestrator has merged it and run the
tests.
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("init").set_defaults(func=cmd_init)

    team = sub.add_parser("team").add_subparsers(dest="team_command", required=True)
    team_add = team.add_parser("add")
    team_add.add_argument("name")
    team_add.add_argument("--leader")
    team_add.add_argument("--charter")
    team_add.set_defaults(func=cmd_team_add)
    team_join = team.add_parser("join")
    team_join.add_argument("agent")
    team_join.add_argument("team")
    team_join.set_defaults(func=cmd_team_join)

    agent = sub.add_parser("agent").add_subparsers(dest="agent_command", required=True)
    agent_add = agent.add_parser("add")
    agent_add.add_argument("name")
    agent_add.add_argument("--team", required=True)
    agent_add.add_argument("--model", required=True)
    agent_add.add_argument("--role", default="member")
    agent_add.set_defaults(func=cmd_agent_add)
    agent_model = agent.add_parser("set-model")
    agent_model.add_argument("name")
    agent_model.add_argument("--model", required=True)
    agent_model.set_defaults(func=cmd_agent_set_model)
    agent_address = agent.add_parser("address")
    agent_address.add_argument("agent")
    agent_address.set_defaults(func=cmd_mail_address)

    mail = sub.add_parser("mail").add_subparsers(dest="mail_command", required=True)
    mail_send = mail.add_parser("send")
    mail_send.add_argument("--from", dest="sender", required=True)
    mail_send.add_argument("--to", required=True)
    mail_send.add_argument("--cc", default="")
    mail_send.add_argument("--subject", required=True)
    mail_send.add_argument("--body", default=None)
    mail_send.add_argument("--body-file", default=None)
    mail_send.add_argument("--job", default=None)
    mail_send.set_defaults(func=cmd_mail_send)
    mail_inbox = mail.add_parser("inbox")
    mail_inbox.add_argument("agent")
    mail_inbox.add_argument("--all", action="store_true", dest="unread", help="include read mail")
    mail_inbox.set_defaults(func=cmd_mail_inbox)
    mail_read = mail.add_parser("read")
    mail_read.add_argument("agent")
    mail_read.add_argument("--id")
    mail_read.set_defaults(func=cmd_mail_read)

    job = sub.add_parser("job").add_subparsers(dest="job_command", required=True)
    job_new = job.add_parser("new")
    job_new.add_argument("--title", required=True)
    job_new.add_argument("--team", required=True)
    job_new.add_argument("--acceptance", default="")
    job_new.add_argument("--opened-by", default="orchestrator")
    job_new.set_defaults(func=cmd_job_new)
    job_assign = job.add_parser("assign")
    job_assign.add_argument("job")
    job_assign.add_argument("agent")
    job_assign.set_defaults(func=cmd_job_assign)
    job_state = job.add_parser("state")
    job_state.add_argument("job")
    job_state.add_argument("state")
    job_state.add_argument("--note", default="")
    job_state.add_argument("--agent", default="agent")
    job_state.set_defaults(func=cmd_job_state)
    job_show = job.add_parser("show")
    job_show.add_argument("job")
    job_show.set_defaults(func=cmd_job_show)

    run = sub.add_parser("run")
    run.add_argument("agent")
    run.add_argument("--prompt", default=None)
    run.add_argument("--prompt-file", default=None)
    run.add_argument("--job", default=None)
    run.add_argument("--note", default="")
    run.add_argument("--opencode-agent", default="build")
    run.set_defaults(func=cmd_run)

    sub.add_parser("status").set_defaults(func=cmd_status)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

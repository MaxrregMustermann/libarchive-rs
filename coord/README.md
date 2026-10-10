# The collective

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

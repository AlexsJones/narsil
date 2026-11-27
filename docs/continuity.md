# Continuity

Work survives whatever interrupts it: closing the terminal, `narsil stop`, a crash, a power cut, a
full context. That comes from one decision: nothing about a run exists only in memory.

## The loop is a function of the transcript

What a session does next is decided by its last message:

- a model turn with tool calls: run the calls that have not finished, then record the results
- a user turn: call the model
- a model turn without tool calls: the turn is over

Nothing about a run exists only in memory. Resuming a paused session, recovering after a crash, and
taking the next step are the same code path.

- **Stop and resume**: `narsil stop` suspends work in flight and leaves it resumable; `narsil start`
  picks goals back up, and chats wait for `/continue`.
- **Crash**: on start, sessions still marked running are known to have been cut off. A tool call that
  was mid-flight is reported to the agent as interrupted, and a shell command it left behind is
  stopped. Goals resume by themselves.
- **Parallel subagents**: `spawn_agents` creates child sessions in the same store. They share memory,
  task notes and project instructions, and each has its own context. The call records its children,
  so after an interruption the same children are continued rather than started again.

## Two stores, for two kinds of thing

**What the agent knows, and what there is to do,** are folders of Markdown notes that link to each
other (see [memory](memory.md) and [work](work.md)). You can read and edit them with any editor.

**What the agent was doing** is a SQLite database (`~/.local/share/narsil/narsil.db`), because that
needs transactions: a model turn and the tool calls it asks for are recorded together or not at all.

- **Transcripts**: every message of every session, append-only.
- **Tool journal**: each tool call is recorded as pending before it runs, running while it runs, and
  done with its result. Shell commands also record their process group.
- **Memory index**: full-text search and the link graph over the memory notes, rebuilt from the notes
  whenever they change. Also `task` notes, the scratchpad shared by the agents on one task.
- **Work index and record**: which work items are ready and who holds what, rebuilt from the work
  notes, and the record of which session did what to each item and where in its transcript.
- **Subagents**: for each one, the task it was given, the objective it serves, and how it ended.
- **Goals**: objectives for unattended work, with a progress journal.

## Goals

A goal keeps running: when the model ends a turn, the supervisor starts the next one, until the agent
calls `goal_complete` or `goal_blocked`, a turn or time budget is reached, or it stalls (several turns
in a row without using a tool). A goal's work hangs under one root item: `goal_complete` is refused
while items under it are open, a goal with nothing left but questions for you blocks itself, and it
starts again by itself once you answer one. Provider outages are retried with backoff for as long as it takes.
`[notify] command` runs when a goal completes, blocks or pauses.

### Where a goal ends

Before it works on a goal, the agent says where the goal ends (`goal_define`): the checks that will
hold when it is done, or that there is no such end. "Get the tests passing" has one. "Improve my
website" does not, and an agent given it will find something more to do for as long as it is let.

So that you can tell which you have started:

- The panel shows the checks under the goal (`done when:`), or `open-ended` in its heading with
  `∞ no finish line: it works until you stop it`.
- `narsil goal list` marks it `[open-ended, runs until stopped]`, and a notice says so when the agent
  declares it.
- An open-ended goal pauses after an hour (`[goals] open_ended_hours`), so that going on is something
  you chose: `narsil goal resume <id>` gives it another.

### What a goal is allowed

```toml
[goals]
max_hours = 0            # pause after this long; 0: no limit
max_iterations = 0       # pause after this many turns
max_tokens = 0           # pause after this many tokens, in and out, subagents included
open_ended_hours = 1.0   # for a goal with no finish line; 0: never
```

Each counts from when the goal was started or last resumed, so resuming a goal that ran out gives
it the same again. A paused goal keeps everything: its plan, its journal, its session.

For work that should continue after you log out, run `narsil service install` and then
`loginctl enable-linger $USER`.

## Sessions

Plain `narsil` opens a new session. `narsil resume <id>` reopens one with its history, and `narsil -c`
continues the latest one in this directory. Leaving the TUI prints the command to come back to each
session that was open; the daemon and any running work carry on without it.

What carries work from one session to the next is not the transcript but the [work graph](work.md),
which a new session in the same directory is told about, and [memory](memory.md).

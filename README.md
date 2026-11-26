# narsil

A persistent agent harness in Rust. A background daemon runs the agents; a terminal UI attaches
to it. Work is journaled as it happens, so a session can be stopped and resumed, survives a crash,
and can keep going overnight on a goal without anyone at the keyboard.

It talks to any major model API: Anthropic directly, and everything that speaks OpenAI-style chat
completions (OpenAI, Gemini, OpenRouter, DeepSeek, Mistral, Groq, xAI, and local servers such as
llama.cpp, Ollama, LM Studio and vLLM).

## Quick start

```sh
cargo install --path .        # puts `narsil` in ~/.cargo/bin
narsil setup                  # choose a backend, store the key, pick a model, set permissions and sudo
narsil                        # open the TUI in the current directory
```

```sh
narsil start | stop | restart | status     # the daemon (the TUI starts it on demand)
narsil -c                                  # continue the latest session in this directory
narsil resume [id]                         # pick an earlier session, or open one by id
narsil run "how many files are in this directory?"      # one prompt, no TUI
narsil goal add "get the test suite passing and keep it green"   # unattended work
narsil goal list | show | pause | resume | cancel
narsil memory list | add | search | rm
narsil config path | show | edit [config|soul|agents]
narsil doctor                              # config, backend reachability, daemon, sudo
narsil service install                     # systemd user unit: restart on crash, start at login
```

## Files you edit

All under `~/.config/narsil/` (`narsil config path` prints the exact locations):

| File | What it is for |
| --- | --- |
| `config.toml` | Backends and models, permissions, goal limits, sudo, shell, voice, notifications. Commented. |
| `SOUL.md` | Who the agent is with you: voice, how it gives feedback, how it exercises judgment. |
| `AGENTS.md` | Standing operating instructions: how to work, what to be careful with, when to save memory. |
| `credentials.toml` | API keys, mode 0600. Environment variables named in `config.toml` take precedence. |
| `./AGENTS.md` | Per-project instructions, read from the working directory upward. |

`SOUL.md` and `AGENTS.md` are read when a session's context is built, so edits apply to new sessions
and after the next compaction. `config.toml` applies after `narsil restart` or `/reload`.

## How it works

### One store

Everything lives in one SQLite database (`~/.local/share/narsil/narsil.db`):

- **Transcripts**: every message of every session, append-only.
- **Tool journal**: each tool call is recorded as pending before it runs, running while it runs, and
  done with its result. Shell commands also record their process group.
- **Memory**: durable facts in `global` scope (about you) and `project` scope (about a directory), and
  `task` notes shared by every agent working on the same task. Full-text searchable.
- **Objectives**: the plan a session is working through, with a status per entry.
- **Subagents**: for each one, the task it was given, the objective it serves, and how it ended.
- **Goals**: objectives for unattended work, with a progress journal.

### The loop is a function of the transcript

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

### Subagents

The agent decides when to hand independent pieces of work to subagents, and can name the objective
each one serves. `[agents] max_parallel` (default 4) run at once, in the order given; the rest queue.
`[model] subagent` gives them a different model from the session's.

You see them as they work:

- **In the conversation**, the `spawn_agents` call is a live roster: one row per subagent with its
  state (queued, running, done, failed, interrupted), its objective (marked ◆) or task, time worked
  and tool calls, and beneath it what it is doing right now or how its report begins.
- **In the side panel**, each subagent sits under the objective it serves, and that objective spins
  while someone is working on it. Subagents with no objective are listed under AGENTS.
- **The status line** counts the subagents working and queued.

Select one in the panel and press enter (or use `/agents`) to open its own transcript and watch it
live. There, esc returns to the parent, a message you send is delivered at its next step, and `/pause`
stops that one subagent while the others carry on. All of this survives closing the TUI, `narsil
stop`, and a crash: the roster is read back from the store.

### Models

`/model` lists the models of every configured provider, fetched live. Type to filter, press enter to
select, and use tab to choose what the selection applies to: this session, new sessions
(`[model] default`) or subagents (`[model] subagent`). `/model haiku` opens the list already filtered,
and a full `/model provider/model-id` switches at once. `/provider` lists the providers and
`/provider <name>` one provider's models. Providers are added with `narsil setup`.

A session can change model between turns, including to another provider. The transcript records the
change, the new model gets a fresh system prompt, and the previous model's reasoning blocks are no
longer replayed. From the shell: `narsil run --session <id> --model <provider/model-id> "..."`.

### Context

A session's system prompt is assembled once per context epoch from: the harness preamble, `SOUL.md`,
`AGENTS.md`, project instruction files, the environment, durable memory, task notes, objectives and
the goal with its progress journal. It is then frozen; later changes are appended to the transcript.
A stable prefix is what makes provider prompt caching work, and what the Anthropic API requires for
replaying signed reasoning blocks.

When a conversation reaches `compact_at_tokens`, the agent writes itself a handoff summary and a new
epoch begins from that summary alone, with a freshly assembled system prompt.

### Goals

A goal keeps running: when the model ends a turn, the supervisor starts the next one, until the agent
calls `goal_complete` or `goal_blocked`, a turn or time budget is reached, or it stalls (several turns
in a row without using a tool). Provider outages are retried with backoff for as long as it takes.
`[notify] command` runs when a goal completes, blocks or pauses.

For work that should continue after you log out, run `narsil service install` and then
`loginctl enable-linger $USER`.

### Permissions and sudo

- `[permissions] mode = "ask"` confirms shell commands and file writes in interactive sessions;
  `"auto"` never asks. Goals use `goal_mode`, `"auto"` by default, because nobody is there to answer.
  `allow` lists simple commands that never ask; `deny` lists substrings that are always refused.
- `[privilege] sudo = true` (the default) lets the agent use sudo. `narsil setup` checks that your
  account is in `wheel` and offers to add it. Commands run without a terminal, so sudo can never sit
  waiting for a password: it either works without one or fails at once. Setup offers to install a
  passwordless rule (`/etc/sudoers.d/narsil`) for unattended use; that is a deliberate, separate
  choice, since it gives an unattended agent root.

## TUI

| Key | Action |
| --- | --- |
| enter | send (while the agent works, the message is queued for its next step) |
| alt+enter, ctrl+j | new line |
| esc | interrupt the agent (pauses the goal, in a goal's session); from a subagent, go back to its parent |
| tab | focus the side panel; up/down jumps the conversation to each entry; enter opens a subagent; esc returns |
| pgup / pgdn | scroll |
| ctrl+o / ctrl+t | expand tool output / show or hide thinking |
| F2 | voice input |
| ctrl+d | quit; the daemon and any running work continue |

Commands: `/new` `/sessions` `/resume <id>` `/continue` `/compact` `/goal <objective>` `/goals`
`/pause` `/agents` `/back` `/model [text]` `/provider [name]` `/memory [query]` `/remember <text>`
`/forget <id>` `/auto` `/ask` `/panel` `/reload` `/title <text>` `/help`.

The side panel appears when the session has a goal, objectives or subagents. It lists the goal, the
objectives with their status and the subagents working on each, and recorded milestones.

### Voice

Voice goes through commands you choose, so any recorder and speech-to-text engine can be used:

```toml
[voice]
enabled = true
record_command = "pw-record --rate 16000 --channels 1 {file}"
transcribe_command = "whisper-cli -m ~/models/ggml-base.en.bin -f {file} -nt -np"
speak_command = ""          # optional text-to-speech; receives each reply on stdin
```

F2 starts recording, F2 again stops it and puts the transcript in the input box.

## Development

```sh
cargo test
NARSIL_HOME=/some/dir narsil setup --provider mock --yes    # an isolated instance with a scripted model
```

`NARSIL_HOME` puts config, data, state and the socket under one directory. The `mock` backend follows a
script embedded in the prompt (see `src/provider/mock.rs`), which is how stop/resume, crash recovery,
subagents and compaction are exercised without a model.

Source layout: `engine.rs` (the loop), `store.rs` (the database), `context.rs` (prompt assembly),
`goals.rs` (the supervisor), `daemon.rs` and `client.rs` (the socket), `provider/` (backends),
`tools/` (what the model can call), `tui/` (the interface), `setup.rs`, `service.rs`, `voice.rs`.

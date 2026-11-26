# Configuration

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

## Models

`/model` lists the models of every configured provider, fetched live. Type to filter, press enter to
select, and use tab to choose what the selection applies to: this session, new sessions
(`[model] default`) or subagents (`[model] subagent`). `/model haiku` opens the list already filtered,
and a full `/model provider/model-id` switches at once. `/provider` lists the providers and
`/provider <name>` one provider's models. Providers are added with `narsil setup`.

A session can change model between turns, including to another provider. The transcript records the
change, the new model gets a fresh system prompt, and the previous model's reasoning blocks are no
longer replayed. From the shell: `narsil run --session <id> --model <provider/model-id> "..."`.

## Permissions and sudo

- `[permissions] mode = "ask"` confirms shell commands and file writes in interactive sessions;
  `"auto"` never asks. Goals use `goal_mode`, `"auto"` by default, because nobody is there to answer.
  `allow` lists simple commands that never ask; `deny` lists substrings that are always refused.
- `[privilege] sudo = true` (the default) lets the agent use sudo. `narsil setup` checks that your
  account is in `wheel` and offers to add it. Commands run without a terminal, so sudo can never sit
  waiting for a password: it either works without one or fails at once. Setup offers to install a
  passwordless rule (`/etc/sudoers.d/narsil`) for unattended use; that is a deliberate, separate
  choice, since it gives an unattended agent root.

Also configured in `config.toml`: [the Obsidian vault](obsidian.md), [key bindings](tui.md),
[what is logged](observability.md), and [limits on goals](continuity.md#goals).

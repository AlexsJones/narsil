# Observability

Everything narsil does is recorded as it happens: each session run, model call, tool call,
compaction, wait for your approval, wait to retry, and goal turn, with its timing, what it ran
inside, and its details (tokens, time to the first token, the provider's request id, errors).

```sh
narsil trace              # the latest session; or: narsil trace <session or goal id>
```

```
● session c3hb › “Count the lines in one.txt and two.txt, then tell me the total.”      1.29s
├─ ◆ decides what to do next  [claude-haiku-5-5 · 4.8k in, 57 out]                      669ms
│  └─ ▸ bash wc -l one.txt two.txt  → 3 one.txt (+2 lines)                                3ms
└─ ◆ “The two files have 4 lines in total.”  [claude-haiku-5-5 · 4.9k in, 31 out]       586ms

1.29s on the clock · 2 model calls 1.25s (97%) · 1 tool calls 3ms (0%) · narsil itself 30ms
tokens 9.7k in (4.8k read from cache, 49%) · 88 out
```

It is the tree that was walked: each decision the model made with what it said, the tools it then
ran with what came back, and a subagent's whole run beneath the call that started it. `/trace` shows
the same inside the TUI.

| Command | What it gives |
| --- | --- |
| `narsil trace --timeline` | The same steps as a flat list in time order, with request ids |
| `narsil trace --flamegraph out.svg` | A flamegraph of where the time went; open it in a browser |
| `narsil trace --chrome out.json` | A trace for ui.perfetto.dev, one track per session |
| `narsil trace --folded` / `--json` | Folded stacks for other flamegraph tools; the raw records |
| `narsil stats --since 7d` | Totals over a period, per model and per tool; `/stats` in the TUI |
| `narsil status` | What is in flight this moment: which session is in which model call or tool |
| `narsil logs -f` | The daemon log; `--session <id>` for one session or goal |

Because a step is recorded when it starts, not when it ends, whatever was running when the daemon
was killed is on record, and shows in the trace as cut off with how long it had run.

## One id finds everything

With several sessions, subagents and goals at work at once, everything narsil records says whose it
is, written the same way in the log, in `narsil status`, in `narsil stats` and in the trace:

| Written | Whose |
| --- | --- |
| `w31q3ky4` | a session |
| `w31q3ky4 goal:x6j4z062` | a goal's session |
| `w31q3ky4>9xk2m1aa count-two` | a subagent, after the session it works for |
| `goal:x6j4z062` | a goal, when no session is at work on it |

```
2026-11-26 14:38:47.168  INFO [w31q3ky4] model call used 1225 input tokens ...
2026-11-26 14:38:49.402 DEBUG [w31q3ky4>9xk2m1aa count-two] tool call finished tool=bash is_error=false bytes=5
2026-11-26 14:38:51.077  INFO [326aqdwe goal:2ftg7tes] goal is now completed: ok
```

```sh
narsil logs --session w31q3ky4        # one session's lines, its subagents' among them; -f follows
narsil logs --session 2ftg7tes        # or a goal's, by the goal's id
narsil trace w31q3ky4                 # the same id, for the tree of what it did
```

A session id can be given in short (`w31q`). Lines that belong to no session (the daemon starting,
the notes being re-read) carry no id. `narsil status` lists the sessions running and what each is
in the middle of, and `narsil stats` has a line per session beside the totals.

## The log

In the TUI, `/logs` opens the log in a window: this session's lines, with its subagents', following
as they are written. `e` shows errors only, `w` warnings and errors, `i` info and up, `a` everything;
`s` switches between this session and all of them; `/` finds text; `f` goes back to following.
`/logs errors` opens straight on the errors.

The log is leveled:

`[telemetry] level` sets how much is logged; `NARSIL_LOG=debug narsil start` overrides it for one
run, and accepts filters such as `info,narsil::provider=debug`. `log_format = "json"` writes one JSON
object per line. Recorded steps are kept for `keep_days`.

For CPU profiling, as opposed to time on the clock, build with the symbols a profiler needs:
`cargo build --profile profiling`, then `perf record -g target/profiling/narsil daemon`.

To record something new, give its span one of the names in `telemetry::RECORDED` (or add a name)
with `#[tracing::instrument]` or `tracing::info_span!`; see the top of `src/telemetry.rs`.

## What you can see without asking

- **The side panel** in the [TUI](tui.md): the goal, the work graph with who holds each item, and
  what each subagent is doing now.
- **The notes.** What the agent knows ([memory](memory.md)), what there is to do ([work](work.md)) and
  how a goal is going are Markdown files you can open, not rows in a database.
- **`◈`** in purple marks every call that touches your [Obsidian vault](obsidian.md).

# Memory

Memory is a folder of Markdown notes, by default `~/.local/share/narsil/memory/`. One note is one
entry: the file name is its title, the text is its body, and `[[Another title]]` in the text is a
relation to another entry. A few properties at the top say where it applies:

```markdown
---
scope: global            # or project:/path/to/project
kind: user               # user | feedback | project | reference | note
updated: 2026-11-26
---

British spelling in prose. See [[Tone]].
```

- `Global/` holds what is true everywhere; `Projects/<name>/` what is true of one directory. A note
  you write yourself needs no properties: it is global unless it sits under a known project.
- The notes are the source of truth. Edit, add, rename or delete them and narsil follows; deleting a
  note forgets the entry. The index in the database is rebuilt from them.
- The agent saves with `memory_save` (linking related entries), finds entries with `memory_search`,
  and walks the graph with `memory_read`, which shows what an entry links to and what links to it.
- A new session gets the most recent entries in its prompt, up to `[context] memory_budget_chars`,
  and the titles of the rest.
- **It is written as the work goes, dead ends included.** Closing a work item or a goal asks what was
  learned, and each finding becomes a note linked to the work it came from: kind `lesson` for an
  approach that worked, `dead_end` for something tried that failed for a reason that will still hold
  (the data does not exist, a tool is missing). A goal that stops as blocked is asked for its dead
  ends in the same way.
- **It is asked for at the moments it tends to be skipped.** These are lines added to what the agent
  already sees, not extra model calls, except the last:
  - after two failed tool calls in a row: is this a dead end?
  - when a command works where another form of it had just failed (`BUILD_MODE=frugal ./build.sh`
    after `./build.sh`): record what made the difference. The same command passing later means
    something was fixed in between, which is ordinary work and brings no reminder.
  - when a chat turn is about to end with something having failed and nothing written down: one more
    step, asking whether the reason will still hold next time.

  A command that prints its own exit status (`...; echo exit=$?`, which models like to write) is
  judged by the status it printed, and commands whose status is an answer (`test`, `grep`, `diff`)
  are not counted as failures.
- **It comes back without being asked for.** When something new is asked of a session, and when an
  agent takes a work item, narsil searches memory for the subject and puts what it finds in front of
  the agent, dead ends marked "tried before and failed" with their date. That costs a search, not a
  model call, and is what stops the same fruitless search being made twice.
- `[memory] dir` puts the folder somewhere else, for instance in a git repository.

`narsil memory sync` shows where the notes are and reports anything it could not use, such as two
notes with the same name.

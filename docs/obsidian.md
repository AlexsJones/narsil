# Obsidian

`narsil setup` offers the vaults Obsidian knows about (or `narsil setup --obsidian <folder>`). With
a vault set under `[obsidian]`:

- **Your notes become something the agent can use.** `vault_search` finds notes by their words,
  `vault_read` reads one with the notes it links to and the notes that link to it, and `vault_write`
  creates, appends to or replaces a note. In `ask` mode a write needs your approval. A replaced note
  is moved to the vault's `.trash` first, so it can be put back.
- **Memory lives in the vault**, under `narsil/Memory/`, so it appears in your graph beside your own
  notes, and an entry can link to one of your notes by name. `memory_in_vault = false` keeps memory
  in its own folder instead.
- **The work notes live in the vault too**, under `narsil/Work/`, linked to each other by what blocks
  what, so the plan is a graph you can see and edit.
- **Each goal gets a note** under `narsil/Goals/` with its status, its work items and progress, refreshed
  as it runs: what an overnight goal did is readable in the morning without opening narsil.

- **Your notes are the first place it looks.** The agent is told that anything about you (who you
  are, your work, the people and projects in your life, your plans and decisions) and anything you
  may have written down is to be looked for in the vault before the web and before asking you. And
  when you ask for something, narsil itself searches the vault for notes on the subject and names
  the best matches to the agent before it starts. Only the notes' paths are given; the agent reads
  what it needs with `vault_read`, where you can see it. `lookup = false` turns this off.
- **You can close parts of it.** `exclude` under `[obsidian]` lists what narsil must not read,
  search or write:

  ```toml
  exclude = ["Journal", "Areas/Health", "*.private.md"]
  ```

  A bare name closes a folder or note of that name wherever it is; a path is taken from the top of
  the vault; `*` and `?` are wildcards. Closed notes are left out of searches, listings, links and
  the automatic lookup; asked for outright, the answer is that they are closed. This holds for the
  vault tools and for `read_file`, `write_file`, `edit_file`, `list_dir`, `glob` and `grep`, by
  whatever path the note is reached (`..`, a link into the vault). The prompt says that parts are
  closed, without naming them. narsil's own folder is never closed to it.

  **What this is and is not.** A shell command cannot be held to a list of paths, so while anything
  is listed, `bash` refuses any command that names the vault or is run from inside it. That stops
  the agent wandering in, or reaching for `cat` when a tool says no. It is not a sandbox: a command
  built to dodge the check (the path assembled piece by piece) would get through. Notes that must
  never reach a model belong in another vault, or outside this one.
- **You can see when it touches the vault.** In the TUI, `◈` in purple marks every call that reads
  or writes your notes (the vault tools, and any file tool or command that reaches into the vault)
  and the notes narsil names for a request. The status line shows `◈ vault`, how many parts are
  closed, and `◈ vault away` when its drive is not there.

`[obsidian] vault` must name the vault itself, the folder that holds `.obsidian`. `narsil doctor`
and the TUI say so if it names the folder above. A NAS's `#recycle` and `@eaDir` folders are never
treated as notes.

A vault on a drive that is not mounted is treated as away, not as empty. Memory is then served from
the index, new entries are held, and they are written into the vault when it returns. Nothing is
written into the empty mount point. If the memory folder is lost for good, `narsil memory restore`
writes every entry out again.

What is in a note the agent reads is sent to the model provider, like anything else it reads.

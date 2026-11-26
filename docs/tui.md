# The terminal UI

| Key | Action |
| --- | --- |
| enter | send (while the agent works, the message is queued for its next step) |
| alt+enter, ctrl+j | new line |
| esc | interrupt the agent (pauses the goal, in a goal's session); from a subagent, go back to its parent |
| tab | complete a slash command being typed; otherwise focus the side panel |
| ctrl+t / alt+w | new tab / close this tab |
| alt+right, alt+left | next tab, previous tab |
| ctrl+g then 1…9 | go to that tab; after ctrl+g, left/right, t and w also move between, open and close tabs |
| f1, alt+k | the list of key bindings as they are bound now |
| pgup / pgdn | scroll |
| ctrl+o / alt+t | expand tool output / show or hide thinking |
| f2 | voice input |
| ctrl+d | quit; the daemon and any running work continue |

**Tabs.** Each tab is a session of its own with its own connection to the daemon, and they all work at
once. The tab bar appears with the second tab. A tab shows a spinner while it works, `!` when it is
waiting for an approval, and `•` when it has finished since you last looked. Plain `narsil` opens a new
session; `narsil resume <id>` (printed for every tab when you leave) reopens one with its history.

Going to a tab by number takes two keystrokes because terminals keep the one-stroke versions for
themselves: Ptyxis, GNOME Terminal and Ghostty all use alt+1…9 for their own tabs. If yours does not,
`goto_tab_with = "alt"` under `[keys]` makes alt+digit work as well.

**Keys.** Everything in the table except the typing keys can be rebound under `[keys]` in `config.toml`
(`new_tab = "ctrl+n"`; several keys for one action are separated by spaces). The keys window lists what
is in force and the name to use for each, and says so if a binding could not be read.

**Commands.** Slash commands complete as you type: the rest of the best match shows dimmed, tab or the
right arrow takes it, and more tabs walk through the others.

Commands: `/new` `/sessions` `/resume <id>` `/continue` `/compact` `/goal <objective>` `/goals`
`/pause` `/agents` `/back` `/cd <dir>` `/model [text]` `/provider [name]` `/memory [query]`
`/remember <text>` `/forget <id>` `/trace` `/stats` `/keys` `/auto` `/ask` `/panel` `/reload`
`/title <text>` `/help`.

The side panel appears when there is a goal, unfinished work in this directory, or subagents. It shows
the goal, the work graph (✓ done, ▶ in progress, ○ ready, · blocked, ? waiting on you) with the
subagents holding each item, and recorded milestones. A session's panel holds what is unfinished here
and what that session itself worked on; selecting an item jumps to where the session last touched it.
Work finished in an earlier session is part of that session's history, and an unfinished item from
another session says which one to resume.

## Voice

Voice goes through commands you choose, so any recorder and speech-to-text engine can be used:

```toml
[voice]
enabled = true
record_command = "pw-record --rate 16000 --channels 1 {file}"
transcribe_command = "whisper-cli -m ~/models/ggml-base.en.bin -f {file} -nt -np"
speak_command = ""          # optional text-to-speech; receives each reply on stdin
```

F2 starts recording, F2 again stops it and puts the transcript in the input box.

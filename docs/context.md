# Context cohesion

A model is only as good as what is in front of it when it decides. narsil's job here is to put the
right things there at the right moment, keep them consistent across everything that would otherwise
scatter them (a long run, a compaction, a new session, a subagent), and leave the rest out.

## What the agent starts a step with

- **A system prompt that does not shift under it.** Assembled once, then fixed (below).
- **What memory holds on the subject, before it starts.** When something new is asked, and when an
  item of work is taken, narsil searches [memory](memory.md) and puts what bears on it in front of
  the agent: above all, what was tried before and failed. This is a search, not a model call.
- **Where the work stands.** A session in a directory with [work](work.md) laid out is told what is
  ready, what is in progress and what waits on you. After a compaction it is told again, as the work
  then stands.
- **For a subagent, exactly its piece.** A subagent given a work item gets the item, why it exists,
  and the results of the items it rests on: what it needs to begin, and not its parent's whole
  conversation.
- **Your own notes first.** With an [Obsidian vault](obsidian.md), the notes that bear on a request
  are named to the agent before it starts, and the parts of the vault you have closed never reach it.

## What is kept out

- Memory entries beyond a budget are listed by title only; the agent reads one when it needs it.
- Finished work is a line of progress, not a transcript: any item can be read in full on request.
- Reminders (to record a dead end, to take an item before working on it) are a line added to what
  the agent already sees, at the moment they apply, not standing instructions repeated every turn.

## Keeping each request small

Every request to the model carries the whole conversation again, so what goes into it is paid for
at every later step.

- What a tool gives back is cut in the middle at 12,000 characters (`[context] tool_output_limit`);
  the agent can ask for the part it needs.
- Taking a work item gives what it rests on and what memory holds on the subject in brief, not in full.
- After five small steps in a row, each a request of its own (one `cat`, one `grep`, one `sed -i`),
  the agent is told to do several at once, and to change files with `edit_file`.

## The system prompt and compaction

A session's system prompt is assembled once per context epoch from: the harness preamble, `SOUL.md`,
`AGENTS.md`, project instruction files, the environment, durable memory, task notes, objectives and
the goal with its progress journal. It is then frozen; later changes are appended to the transcript.
A stable prefix is what makes provider prompt caching work, and what the Anthropic API requires for
replaying signed reasoning blocks.

When a conversation reaches `compact_at_tokens`, the agent writes itself a handoff summary and a new
epoch begins from that summary alone, with a freshly assembled system prompt.

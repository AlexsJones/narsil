# Getting started

## Install

```sh
brew install AlexsJones/narsil/narsil     # macOS and Linux
```

or from source, with a Rust toolchain:

```sh
cargo install --path .                    # puts `narsil` in ~/.cargo/bin
```

Then:

```sh
narsil setup                  # choose a backend, store the key, pick a model, set permissions and sudo
narsil                        # open the TUI in the current directory
```

narsil talks to Anthropic directly, and to everything that speaks OpenAI-style chat completions:
OpenAI, Gemini, OpenRouter, DeepSeek, Mistral, Groq, xAI, and local servers such as llama.cpp,
Ollama, LM Studio and vLLM.

## Commands

```sh
narsil start | stop | restart | status     # the daemon (the TUI starts it on demand)
narsil -c                                  # continue the latest session in this directory
narsil resume [id]                         # pick an earlier session, or open one by id
narsil run "how many files are in this directory?"      # one prompt, no TUI
narsil goal add "get the test suite passing and keep it green"   # unattended work
narsil goal list | show | pause | resume | cancel
narsil memory list | add | search | rm | sync | restore
narsil work [item title]                    # this directory's work graph, or one item in full
narsil config path | show | edit [config|soul|agents]
narsil doctor                              # config, backend reachability, daemon, sudo
narsil trace [id]                          # the tree of what a session or goal did, with timings
narsil stats --since 24h                   # time and tokens per model, time and failures per tool
narsil service install                     # systemd user unit: restart on crash, start at login
```

Next: [configuration](configuration.md), [the terminal UI](tui.md).

# Local models

Ask narsil which open models your own machine can run an agent on, and it asks
[llmfit](https://github.com/AlexsJones/llmfit), which knows your hardware and a catalogue of models:
whether one fits in memory, how fast it should be, how much context it has, whether it can call tools.

```sh
brew install llmfit
```

With llmfit installed, the agent has a `local_models` tool, and a plain question reaches it:

```
what's the best local model for agentic work on this machine?
will Qwen3.6-27B run here at Q4, and how fast?
what would I need to run a 70B model?
```

`narsil doctor` says whether llmfit was found. Without it the tool is not offered, and narsil says so
instead of answering from general knowledge, which does not know your machine.

## What narsil adds to llmfit's answer

llmfit ranks models for a machine. narsil asks it the question an agent harness needs answered:

- **Models that call tools.** One that cannot is no use to an agent, however well it scores.
- **Sized for an agent's context.** Memory and speed are worked out for 32,768 tokens, not llmfit's
  default of 8,192: narsil's own instructions and tools take about 8,000 before any work is done.
- **That you can serve.** With only llama.cpp, Ollama or LM Studio installed, a model needs a GGUF
  build to be of use, and those without one are left out.

The answer has three parts: the best by llmfit's own score; the most capable that run at a usable
speed (8 tokens a second or more), since an agent is better served by a more capable model than a
faster one once it is fast enough; and those more capable still that are too slow as llmfit lists
them but usable at a lighter quantisation. llmfit lists each model at the heaviest quantisation that
fits, which is also the slowest, so the last part asks it again at `Q4_K_M`.

Models that llmfit cannot tell apart (the same size, speed and scores) are given as one entry, under
a lab's own release where there is one, with the fine-tunes of it counted beside it. Each entry comes
with the command that serves it.

Speeds are llmfit's estimates for your hardware unless it has a measurement, and say which they are.

## Using the model it recommends

Serve it with the command given, then point narsil at the server:

```sh
llama-server -hf unsloth/Qwen3.6-27B-GGUF:Q4_K_M -c 32768 -ngl all --jinja --port 8080
narsil setup --provider local --base-url http://127.0.0.1:8080/v1
```

Setup reads the context size from the server and sets the conversation to be compacted before it is
reached. `/model` then switches between the local model and any other.

## Settings

```toml
[llmfit]
command = "llmfit"        # the program's name or path; "" to leave llmfit out
runtimes = []             # recommend only for these: "llama.cpp", "Ollama", "LM Studio", "MLX"
```

With `runtimes` empty, narsil recommends for whichever of them it finds installed.

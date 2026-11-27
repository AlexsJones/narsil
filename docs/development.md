# Development

```sh
cargo test                                                   # everything that needs no model: about 5 seconds
NARSIL_HOME=/some/dir narsil setup --provider mock --yes    # an isolated instance with a scripted model
```

`NARSIL_HOME` puts config, data, state and the socket under one directory. The `mock` backend follows a
script embedded in the prompt (see `src/provider/mock.rs`), which is how stop/resume, crash recovery,
subagents and compaction are exercised without a model.

## Tests for memory and work in flight

There are three layers, and a change to memory, the work graph or the vault should leave all of them
passing.

| Layer | Where | What it pins down |
| --- | --- | --- |
| Unit | `#[cfg(test)]` in `src/` | One module at a time: the notes and their index, what is recalled for a request, the work graph's rules, what the exclude list closes, how the vault is searched. |
| Scenarios | `tests/memory.rs`, `tests/work.rs`, `tests/vault.rs` | The whole thing running: each test starts its own daemon with the scripted model (`tests/common/mod.rs`), has it do some work, and checks what was left behind: the notes on disk, what a later session is told before its model is called, what survives a `kill -9`, what reaches the model from a closed part of the vault (nothing). |
| Live | `tests/live.rs` | The same promises with a real model: does it write a dead end down and stay out of it next time, use a lesson, file work where the work is, look in the vault first. |

The scenarios check what narsil does with what a model tells it, and are exact and free. The live
tests check that a model, given narsil's prompt and tools, tells it, which is what changes when the
prompt, a tool's description or the model changes. They cost a few pence and run only when asked:

```sh
NARSIL_LIVE_PROVIDER=anthropic NARSIL_LIVE_MODEL=claude-sonnet-5-5 NARSIL_LIVE_KEY_ENV=MY_KEY_VARIABLE \
  cargo test --test live -- --ignored --nocapture --test-threads 1
```

`NARSIL_LIVE_PROVIDER` is a `narsil setup` preset (`anthropic`, `deepseek`, `openai`, ...) and
`NARSIL_LIVE_KEY_ENV` names the environment variable that holds the key. Each test prints what it
measured (how many times the failing script was run in each session, what was saved). A model's
behaviour varies from run to run, so a failure here is worth a second run before a conclusion.

A scenario that fails leaves its instance under the temporary directory (`narsil-test-*`) to be
looked at. When adding behaviour of this kind, add the scenario, then break the behaviour on purpose
and see the scenario fail: a test that cannot fail is not checking anything.

Source layout: `engine.rs` (the loop), `store.rs` (the database), `memory.rs` (the memory notes),
`work.rs` (the work graph),
`obsidian.rs` (vaults, notes and links), `telemetry.rs` (logs, recorded spans, traces),
`context.rs` (prompt assembly), `goals.rs` (the supervisor), `daemon.rs` and `client.rs` (the socket), `provider/` (backends),
`tools/` (what the model can call), `tui/` (the interface), `setup.rs`, `service.rs`, `voice.rs`.
`tests/` holds the scenarios and the live tests.

`tests/usage.rs` checks the account of tokens: that a request's receipt is the sum of its calls,
subagents and compaction included, and that each backend's way of reporting usage (served by a
stand-in) arrives as the same numbers. `tests/accuracy.rs` holds it to real backends and runs only
when asked for: a recorder between narsil and the backend keeps every answer, and narsil's record
must match it call for call.

```sh
NARSIL_LIVE_PROVIDER=deepseek NARSIL_LIVE_MODEL=deepseek-v4-pro NARSIL_LIVE_KEY_ENV=DEEPSEEK_API_KEY \
  cargo test --test accuracy -- --ignored --nocapture
NARSIL_LIVE_PROVIDER=local NARSIL_LIVE_MODEL=<model> NARSIL_LIVE_BASE_URL=http://127.0.0.1:8080/v1 \
  cargo test --test accuracy -- --ignored --nocapture
```

Run it after changing how a provider's usage is read, and for a backend narsil has not been checked
against.

`tests/local.rs` checks what narsil asks of llmfit, with a script standing in for it. A test instance
has no llmfit unless the test gives it one, so the suite is the same on a machine with llmfit
installed and on one without.

## Releasing

The git tag is the version. Pushing one releases it:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` then runs [GoReleaser](https://goreleaser.com) (`.goreleaser.yaml`) on
a Mac runner. It builds the macOS binaries natively and cross-compiles the Linux ones with zig, names
and checksums the archives, writes the changelog from the commits since the last tag, creates the
GitHub release, and commits the updated cask to the Homebrew tap,
[AlexsJones/homebrew-narsil](https://github.com/AlexsJones/homebrew-narsil). `narsil --version` reports
the tag, whatever `Cargo.toml` says.

- **A rehearsal**: run the workflow by hand (`gh workflow run release.yml`). Everything is built and
  packaged and kept as an artifact of the run; nothing is published.
- **Locally**: `goreleaser release --snapshot --clean`, with zig and cargo-zigbuild installed
  (`brew install cargo-zigbuild`). The macOS builds need a Mac.
- **The tap** is written with a deploy key that can push to that repository and nothing else. Its
  private half is the `HOMEBREW_TAP_DEPLOY_KEY` secret of this repository.
- `.github/workflows/ci.yml` builds and tests every push and pull request on Linux, and on macOS
  for information.

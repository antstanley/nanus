---
title: Installation
description: Build nanus from source, store a provider key, and confirm what the harness resolved.
sidebar:
  order: 2
---

`nanus` is distributed as source. Building it produces two binaries: `nanus`, the core,
and `nanus-tui`, the interface.

## Requirements

- **Stable Rust**, pinned by [`rust-toolchain.toml`](https://github.com/antstanley/nanus/blob/main/rust-toolchain.toml)
  and edition 2024. `rustup` reads the pin for you.
- **`cargo-nextest`** if you want to run the test suite: `cargo install cargo-nextest`.
  Building and running the binaries does not need it.
- A Unix-like system. The local link between the core and the interface is a Unix domain
  socket, and the workspace already depends on `nix` for process groups, so there is no
  Windows build.

## Build from source

```sh
git clone https://github.com/antstanley/nanus.git
cd nanus
cargo build --release --workspace
```

That writes `target/release/nanus` and `target/release/nanus-tui`. If you want them on your
`PATH`, copy or link both:

```sh
install -m 0755 target/release/nanus target/release/nanus-tui ~/.local/bin/
```

## Store a provider key

A credential is never written to the configuration file and never taken as a command-line
argument, so it cannot end up in a config, a log, or a process list. `nanus auth set` reads
the key from standard input:

```sh
nanus auth set deepseek       # paste the key and press enter, or pipe it in
```

The key is stored in a chain of backends, tried in order: the macOS keychain, a `0600` file
under the nanus home, then the provider's environment variable. You can skip the store
entirely and export the variable instead:

```sh
export DEEPSEEK_API_KEY=...
```

| Provider | Environment variable |
| --- | --- |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `zai` | `ZAI_API_KEY` |
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |

`nanus auth set <provider>` and `nanus auth status` work for every provider and plan. The account
is the provider's name, or `provider:plan` for a plan with a key of its own (`zai:coding`), so a
key stored for one provider or plan can never be sent to another.

## Verify the install

Both commands below read only local state, so they need no key:

```console
$ nanus config
config file: /Users/you/.config/nanus/config.toml
provider: deepseek (plan api)
model: deepseek-flash
endpoint: https://api.deepseek.com
max tokens: 128000 (ceiling 256000)
reasoning effort: Medium
approval policy: per_call
sandbox mode: ReadOnly
max steps per turn: 512
context budget: 64000 estimated tokens (older turns are dropped past it)
max parallel tools: 4
tui detail: compact
markdown answers: true
mermaid diagrams: true
workspace root: <the current directory>
service socket: /Users/you/.config/nanus/run/agent.sock
service log: /Users/you/.config/nanus/nanus-service.log
credential: set for deepseek (keychain)
```

`nanus auth status` reports the same credential chain provider by provider, and
`nanus config` prints the provider, plan, model, and endpoint it resolved to - which is the
fastest way to see what the harness thinks it is before spending a request.

## Next steps

- [Quickstart](/getting-started/quickstart/) - run a turn, then a session you can resume.
- [Configuration](/guides/configuration/) - change the model, the approval policy, or the sandbox.

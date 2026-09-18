---
title: Introduction
description: What nanus is, why it is built this way, and the three lifetimes one agent can have.
sidebar:
  order: 1
---

`nanus` is a coding-agent harness written in safe Rust. It is a headless CLI and an
interactive terminal interface over a Rust implementation of the
[Cordis](https://github.com/cordiverse/cordis) meta-framework. Every part the agent runs
on - the filesystem, the shell, the session log, the model adapter, and the tool registry -
is a plugin mounted on a shared kernel context, and the agent loop is built over the
handles those plugins publish.

Most agent harnesses are a loop, a pile of tools, and a config file that grew until it
became an API. You can use them, but changing one is hard, because the parts know about
each other and the seams are in the wrong places. `nanus` starts from the other end: there
is no privileged core to patch, just a context and components that register into it. That
buys three things that are hard to get any other way.

- **Replace the model without touching the tools.** The adapter sits behind an `LlmPort`.
  Four providers ship and are *selected*, not compiled in, so one line of configuration
  changes the provider and nothing else notices.
- **Replace the tools without touching the model.** Each tool is a plugin behind a
  `ToolExecutor`. Add one and its schema joins the prompt; remove one and it stops
  existing, with no dead description left in every request.
- **Unload a component and get your system back.** Every registration is recorded *with
  its inverse*. Unloading a plugin withdraws its services, unregisters its listeners, and
  deactivates its consumers in reverse order. No stale registrations, and no restart to
  get clean.

That last property has a name in the literature - *temporal composability* - and it is the
reason the project exists rather than being another loop in a `main.rs`.

## One agent, three lifetimes

The agent is the same object in every mode. What differs is how long it lives and how a
client reaches it - and the transport is the same in all three, so there is one code path
that runs a turn whether an interface is watching or not.

| Mode | Lifetime | Reached by |
| --- | --- | --- |
| `nanus run <task>` | one turn | stdout, in the same process |
| `nanus tui` / a bare `nanus` | the interface's | the local link, for a shell-scoped agent |
| `nanus service` | until stopped | the local link, for a process |

The interface is its own binary. It is always a *client* of the agent over a local Unix
socket, whether the agent was started by the same shell or has been up since boot. `nanus`
does not link `nanus-tui` at all, so a change to rendering cannot change what a script
runs.

## What it can do

A bounded turn (512 steps by default, and an ending that always says why it stopped), a
bounded prompt (older turns dropped past a context budget, with a notice), seven tools
that run together up to a concurrency limit, and an approval policy that fails closed by
default. Every run persists an append-only transcript, so a conversation is a thing you
can name, resume, or attach to while it is still running.

Exactly seven tools ship:

| Tool | What it does |
| --- | --- |
| `read` | Reads a file through a 1-based offset/limit line window, with line numbers and a byte ceiling. |
| `write` | Creates or replaces a file. |
| `edit` | Replaces text, requiring the match to be unique unless `replace_all` is set. |
| `read_image` | Attaches a PNG, JPEG, WebP, or GIF to the conversation. |
| `glob` | Finds files by path pattern, anchored to the workspace root. |
| `grep` | Finds text inside files, grouped by file. |
| `bash` | Runs a program in the workspace root, reporting stdout, stderr, and the exit code. |

The count is the design: each is a mechanism a shell cannot provide as well. See the
[CLI reference](/reference/cli/) for the commands, and the
[configuration guide](/guides/configuration/) for every setting.

:::caution
`nanus` runs programs and edits files in a workspace you point it at. The defaults are
conservative - `per_call` approval and a read-only sandbox - but the sandbox is not
OS-enforced. Read [SAFETY.md](https://github.com/antstanley/nanus/blob/main/SAFETY.md)
before running it on a machine you care about.
:::

## Next steps

- [Installation](/getting-started/installation/) - build the binaries and store a provider key.
- [Quickstart](/getting-started/quickstart/) - a first run, then a named session you can resume.
- [Configuration](/guides/configuration/) - the config file, its fields, and the environment variables.

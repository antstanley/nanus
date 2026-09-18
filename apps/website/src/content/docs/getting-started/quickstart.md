---
title: Quickstart
description: From a fresh clone to a named session you can resume, a transcript you can replay, and an agent that outlives the shell.
sidebar:
  order: 3
---

This assumes you have [built the binaries and stored a key](/getting-started/installation/).
From a clone of the repository, with `nanus` on your `PATH`:

## One turn

```sh
nanus run "Summarise this repository."
```

**stdout carries the answer and nothing else.** Reasoning and tool activity go to stderr, so
a pipe sees only the result:

```sh
nanus run "List the top-level files" > answer.txt
```

Add `--verbose` to watch the reasoning and every tool call on stderr while the same clean
answer goes to stdout:

```sh
nanus --verbose run "Find the TODO comments and group them by file."
```

The exit code is part of the contract: `0` only for a completed turn, non-zero otherwise.
A script can tell a finished run from a failed one without parsing output.

:::note
`--quiet` is accepted and does nothing. The default already is what it asks for, and it
conflicts with `--verbose`.
:::

## A session you can come back to

A run records its transcript. Name it and it becomes something you can resume - including
one that failed, which is exactly the run worth continuing.

```sh
nanus run --name nightly "summarise what changed today"

# tomorrow, continue the same conversation
nanus run --resume nightly "now open a pull request for the fixes"
```

Names resolve case-insensitively, so `Nightly` and `nightly` are the same session.

## Sit in front of it

A bare `nanus` with a terminal starts the interface; it is always a client of the agent
over a local socket.

```sh
nanus                  # start the interface against a shell-scoped agent
nanus tui --connect    # or attach to a `nanus service`
```

Every session is also a transcript you can read without a key and without an agent:

```sh
nanus sessions                    # list what is available, newest first
nanus tui --session               # read the most recent one
nanus tui --session <id>          # read a particular one
nanus tui --session --scroll 50   # open fifty rows back, where the tool calls are
```

## An agent that outlives the shell

```sh
nanus service start     # detached by default
nanus service status    # what is running, and which sessions it holds
nanus service stop      # ask it to stop, no signal needed
```

Several clients can attach to one live session at once, and a prompt to a busy session is
refused rather than queued - the turn in flight is not interrupted by a second writer.

## Where to go next

- [Configuration](/guides/configuration/) - the config file and every field.
- [Model providers](/guides/providers/) - the four adapters and their plans.
- [CLI reference](/reference/cli/) - every command and flag.
- The repository's [SAFETY.md](https://github.com/antstanley/nanus/blob/main/SAFETY.md) -
  what the agent can do, and what the defaults do and do not protect you from.

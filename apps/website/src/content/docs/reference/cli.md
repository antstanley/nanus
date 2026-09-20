---
title: CLI reference
description: Every nanus command and flag, and the output contract each one keeps.
sidebar:
  order: 1
---

## Global options

`--verbose`, `--quiet`, and `--config <PATH>` are accepted on `nanus` itself.

`--verbose` sends progress to stderr so stdout still carries only the answer. `--quiet` is
accepted and does nothing, because the default already is what it asks for, and it conflicts
with `--verbose`. `--config <PATH>` points at an explicit configuration file, overriding
`NANUS_CONFIG` and the default location.

## Commands

| Command | What it does |
| --- | --- |
| `nanus run [--name NAME \| --resume NAME\|ID] <TASK>` | One prompt, one answer on stdout, then exit. |
| `nanus tui` / `nanus ui` | Start the interface against a shell-scoped agent. |
| `nanus tui --connect [--socket PATH]` | Talk to a `nanus service` instead. |
| `nanus tui --resume REF` / `--name NAME` | Open or record a particular session. |
| `nanus tui --session [ID] [--scroll ROWS]` | Read a recorded transcript; no key needed. |
| `nanus service start [--foreground] [--socket PATH] [--log PATH]` | Start a service, detached by default. |
| `nanus service stop [--socket PATH]` | Ask a running service to stop. |
| `nanus service status [--socket PATH]` | Report whether one is running, and which sessions it holds; non-zero when nothing answers. |
| `nanus config` | Print the effective configuration and the provider, plan, model, and endpoint it resolves to; no key needed. |
| `nanus auth set <PROVIDER>[:<PLAN>]` | Store a key, read from standard input. |
| `nanus auth login <PROVIDER>[:<PLAN>]` | Authorize a plan that is reached with a browser rather than a key; waits for the service. |
| `nanus auth clear <PROVIDER>[:<PLAN>]` | Remove a stored credential, key or authorization. |
| `nanus auth status` | Report which providers and plans have a credential, and where a key is read from; no key needed. |
| `nanus sessions` | List recorded sessions, newest first; no key needed. |
| `nanus sessions name <NAME> <SESSION>` | Record or change a session's name. |
| `nanus sessions delete <NAME\|ID>` | Remove a session and release its name. |
| `nanus sessions show [--json] <NAME\|ID>` | Report what a session ran under and what it spent; no key needed. |

A bare `nanus` starts the interface when there is a terminal and prints usage when there is
not. The usage text is built from the same parser the commands are, so help and grammar
cannot disagree.

## The output contract

`nanus` keeps a stable, scriptable surface:

- **stdout carries the answer and nothing else.** Reasoning and tool activity go to stderr.
- **The exit code is meaningful.** `0` only for a completed turn, non-zero otherwise.
- **Reporting commands need no key.** `config`, `auth status`, `sessions`, and
  `tui --session` read local state only, so they are safe to run before a credential exists.
- **`sessions show --json` emits machine-readable output.** The human-readable form is the
  default.

## Examples

```sh
# One turn, verbose progress on stderr, clean answer on stdout
nanus --verbose run "Find the TODO comments and group them by file."

# A named session, resumed later
nanus run --name nightly "summarise what changed today"
nanus run --resume nightly "now open a pull request for the fixes"

# A service that outlives the shell
nanus service start
nanus tui --connect
nanus service stop

# Read a transcript without an agent or a key
nanus tui --session --scroll 50
```

---
title: Configuration
description: The nanus config file, every field and its default, and the environment variables that override it.
sidebar:
  order: 1
---

`nanus` reads one flat TOML file, every field defaulted, with a real `config_version`
migration chain. The file lives at `<platform config dir>/nanus/config.toml` - on macOS,
`~/Library/Application Support/nanus/config.toml`; on Linux, `$XDG_CONFIG_HOME/nanus/config.toml`
or `~/.config/nanus/config.toml`. `NANUS_CONFIG` overrides the path.

Unknown keys are ignored, and **no field can hold a credential**: a key is a secret, so it
goes through [the credential chain](/getting-started/installation/#store-a-provider-key)
instead.

## Example

```toml
# A configuration that changes the provider and tightens the sandbox, leaving
# everything else at its default.
config_version = "0.1.0"
provider = "anthropic"
approval_policy = "all_calls"
sandbox_mode = "read_only"
reasoning_effort = "high"
```

Every provider field is optional, and absent means "the provider's own answer applies". A
file that names none of them runs `deepseek` with its own host and model. A file that sets
`provider` alone gets that provider's default host and model. An unknown provider or plan
is refused at startup with a sentence naming the ones the build offers.

## Fields

| Field | Default | Values |
| --- | --- | --- |
| `provider` | `deepseek` | `deepseek`, `zai`, `anthropic`, `openai` |
| `plan` | the provider's default | `api`, `coding` (z.ai), `subscription` (OpenAI, authorized with OAuth) |
| `base_url` | the plan's endpoint | an override, for a proxy or a gateway |
| `model` | the plan's or provider's default | any id the provider serves |
| `max_tokens` | `128000` | per-response budget, capped at the provider's ceiling |
| `reasoning_effort` | `medium` | `minimal`, `low`, `medium`, `high` |
| `approval_policy` | `per_call` | `per_call`, `permitted`, `all_calls` |
| `sandbox_mode` | `read_only` | `read_only`, `workspace_write`, `danger_full_access` |
| `max_steps_per_turn` | `512` | steps in one turn |
| `context_budget` | `64000` | estimated prompt tokens for one request |
| `max_parallel_tools` | `4` | how many of a step's calls may be in flight at once |
| `tui_detail` | `compact` | `compact`, `full` |
| `markdown` | `true` | render the model's answers as markdown |
| `mermaid` | `true` | draw `mermaid` fences as text diagrams |
| `system_prompt` | built-in | override the system prompt |
| `workspace_root` | the current directory | root the tools are confined to |
| `service_socket` | `<nanus home>/run/agent.sock` | where a service listens |
| `service_log` | `<nanus home>/nanus-service.log` | where a detached service writes |
| `config_version` | the build's version | the schema the file was written with; a newer one is refused |

## Request controls

`max_tokens` is a per-response budget, capped at the provider's documented ceiling because a
request above it is refused rather than truncated. `reasoning_effort` reaches DeepSeek, z.ai
(as a thinking mode), and OpenAI (as its own four-step scale). It is **not** sent to
Anthropic, which has no place in the message model for the signed thinking blocks a
tool-using turn would need to replay; the omission is reported where it is configured rather
than silently ignored.

`context_budget` bounds one request. The estimate is characters over four plus a small
per-message cost - deliberately approximate, because there is no tokenizer in the harness
and the provider reports the real count with every response. Past the budget the *oldest
turns* are dropped whole, with a notice the model reads where the gap is. A turn whose
newest part does not fit is refused with a sentence naming the field rather than sent to a
provider that would reject it.

## Environment variables

| Variable | Effect |
| --- | --- |
| `NANUS_CONFIG` | Override the configuration file path. |
| `NANUS_HOME` | Override the session-store home, and the default socket and log paths. |
| `NANUS_TUI` | Override the path to the interface binary. |
| `DEEPSEEK_API_KEY` | DeepSeek credential; the last store in the chain. |
| `ZAI_API_KEY` | z.ai credential, for the `api` plan. The `coding` plan has its own
  (`ZAI_CODING_API_KEY`). |
| `ANTHROPIC_API_KEY` | Anthropic credential. |
| `OPENAI_API_KEY` | OpenAI credential, for the `api` plan. The `subscription` plan is authorized,
  not keyed: see `nanus auth login`. |
| `NO_COLOR` | Render with no colour at all, keeping bold and italic. |
| `RUST_LOG` | Tracing filter for the service and core logs. |

## Check what resolved

`nanus config` prints the effective configuration and the provider, plan, model, and
endpoint it resolved to. It needs no key, so it is safe to run first:

```sh
nanus config
```

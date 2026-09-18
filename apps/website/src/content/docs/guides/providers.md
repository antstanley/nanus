---
title: Model providers
description: The four providers nanus ships, the plans they offer, and how to switch between them.
sidebar:
  order: 2
---

Four providers ship with `nanus`. Each is an adapter behind the `LlmPort`, and nothing in
the tools, the domain, or the agent loop knows which one is in use - switching is a line of
configuration, not a rebuild.

| Provider | Adapter | Models offered | Plans |
| --- | --- | --- | --- |
| `deepseek` (default) | `nanus-adapter-deepseek` | `deepseek-flash`, `deepseek-v4-pro` | `api` |
| `zai` | `nanus-adapter-openai` | `glm-4.5`, `glm-4.5-air`, `glm-4.5-flash` | `api`, `coding` |
| `anthropic` | `nanus-adapter-anthropic` | `claude-sonnet-4-20250514`, `claude-opus-4-20250514` | `api` |
| `openai` | `nanus-adapter-openai` | `gpt-5`, `gpt-5-mini`, `gpt-5-codex` | `api`, `coding`, `subscription`¹ |

¹ OpenAI's `subscription` plan - the ChatGPT coding tier reached with an OAuth token - is
listed and **refused with its reason** rather than silently absent. It needs an OAuth token
and the Responses API, and this build does neither.

## Choosing a provider

Set `provider` (and optionally `plan` and `model`) in the config file:

```toml
provider = "zai"
plan = "coding"
model = "glm-4.5"
```

A **plan is an endpoint plus a default model.** z.ai's `coding` plan is the same key and
protocol at a different host; OpenAI's `coding` plan is a coding model on the same host.
`base_url` overrides either, for a proxy or a gateway.

A model id belongs to the provider that offers it: naming a DeepSeek id with
`provider = "openai"` is a request the provider refuses rather than a quiet substitution,
and retired model ids do not resolve.

## Credentials

Each provider reads its key from [the credential chain](/getting-started/installation/#store-a-provider-key),
falling back to its environment variable:

```sh
nanus auth set zai        # or: export ZAI_API_KEY=...
nanus auth status         # which providers have a credential, and where it is read from
```

The account a key is stored under is the provider's name, so a key stored for one provider
can never be sent to another.

## What the adapters implement

All four stream responses over SSE with reasoning content and tool calls reassembled from
their frames - DeepSeek and z.ai send `data:`-framed chunks ending in `[DONE]`, Anthropic
sends event-typed frames ending in `message_stop`. Usage accounting decodes each provider's
own spelling of prompt, cached, and generated tokens, including how much of the generation
was thinking.

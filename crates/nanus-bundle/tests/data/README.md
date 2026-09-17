# Captured wire traces

`deepseek-tool-call.sse` is a **real** server-sent-events response from
`https://api.deepseek.com/chat/completions`, recorded so the replay in
[`../live_wire.rs`](../live_wire.rs) is a captured shape rather than a documented
one. The test that uses it embeds it with `include_str!`, so the bytes it asserts
against are exactly the bytes below.

## How it was recorded

- Endpoint: `POST https://api.deepseek.com/chat/completions`
- Model: `deepseek-flash`
- Mode: streaming, `stream_options.include_usage`, thinking enabled at
  `reasoning_effort: medium` — the mode the harness sends by default.
- Request: one system message describing the workspace, one user message asking the
  model to run `printf hello` with a `bash` tool, and the `bash` and `read` tool
  schemas.
- The model answered with a `bash` call whose arguments arrived in eleven frames:
  one announcing the call (`index`, `id`, `type`, `function.name`, empty
  `arguments`) and ten carrying argument fragments a few characters at a time. The
  fragments reassemble to `{"command": "printf hello"}`.

## What it verifies

The framing a real response uses and a hand-written replay might not:

- the call-announcing frame carries `"type":"function"` and an empty `arguments`;
- every later fragment carries `index` and `function.arguments` only — no `id`, no
  `name`, no `type`;
- argument fragments are tiny and split mid-token;
- a final frame carries `"content":""`, `"reasoning_content":null`, the
  `tool_calls` finish reason, and usage, including
  `completion_tokens_details.reasoning_tokens`;
- frames carry fields the adapter ignores (`id`, `object`, `created`,
  `system_fingerprint`, `logprobs`).

If a future API change makes this trace stop decoding, the replay fails and
[`wire.rs`](../../../nanus-adapter-deepseek/src/wire.rs) is the only adapter file
involved.

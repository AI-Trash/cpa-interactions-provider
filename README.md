# cpa-interactions-provider

A [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) native plugin that exposes Google's Gemini **Interactions API** (the new stateful `interactions.create` surface, GA June 2026) behind an OpenAI-compatible `/v1/chat/completions` endpoint.

The plugin keeps an in-memory conversation map (`history_hash → interaction_id`) so callers using a plain OpenAI SDK stay stateless and let the gateway chain `previous_interaction_id` on their behalf. This unlocks the Interactions API's implicit caching, server-side state, and managed agents (Deep Research, Antigravity) without any change to the client.

## What it does

- Accepts OpenAI Chat Completions requests routed by the host.
- Inspects `messages`, computes a stable hash of the established history (everything before the last user turn) plus model + system + tools.
- Looks up `previous_interaction_id` in its in-process session map.
- Builds a Gemini Interactions API request:
  - When the requested model matches a known agent name (e.g. `antigravity-preview-05-2026`, `deep-research-preview-04-2026`), the request uses `agent` instead of `model` and attaches `environment: "remote"` for Antigravity agents.
  - Otherwise the request passes `model` straight through.
- Sends the request via `host.http.do` (Go host still owns transport, logging, and auth injection via `AuthAttributes["api_key"]`).
- Stores the returned `interaction.id` (and `environment_id` when present) keyed by the *post-turn* history hash so the next user turn hits the same chain.
- Folds the returned `steps[]` (currently `model_output.content[].text` and `thought.summary[].text` if present) into an OpenAI `chat.completion` object.

## Why a plugin instead of a fork

CLIProxyAPI's built-in Gemini executor targets the legacy `generateContent` surface. The Interactions API has a fundamentally different shape (`steps[]` chronology, server-side state, optional agents), so wrapping it required either forking the host or writing a custom executor. The plugin ABI (introduced in CLIProxyAPI v7) lets us ship this as a single cdylib that the host loads at startup, with no changes to the host binary.

## Install

1. Download the latest release artifact for your platform from the [Releases page](https://github.com/AI-Trash/cpa-interactions-provider/releases):
   - Linux: `libcpa_interactions_provider.so`
   - macOS: `libcpa_interactions_provider.dylib`
   - Windows: `cpa_interactions_provider.dll`
2. Drop the library into CLIProxyAPI's plugin directory. The host searches:
   ```
   plugins/<GOOS>/<GOARCH>-<variant>
   plugins/<GOOS>/<GOARCH>
   plugins
   ```
3. Enable it in `config.yaml`:

   ```yaml
   plugins:
     enabled: true
     dir: "plugins"
     configs:
       cpa-interactions-provider:
         enabled: true
         priority: 1
         # Optional overrides:
         endpoint: "https://generativelanguage.googleapis.com/v1beta/interactions"
         store: true
         agents:
           - "antigravity-preview-05-2026"
           - "deep-research-preview-04-2026"
           - "deep-research-max-preview-04-2026"
         # Antigravity agents need an environment; default is "remote".
         default_environment: "remote"
   ```

4. Point your OpenAI SDK at CLIProxyAPI with the model name set to a Gemini chat model (e.g. `gemini-2.5-flash`, `gemini-3-pro-preview`) or an agent name (e.g. `antigravity-preview-05-2026`). The plugin's `ModelRouter` will claim the request and route it to the Interactions executor.

## Configuration fields

| Field | Type | Default | Description |
| ----- | ---- | ------- | ----------- |
| `enabled` | boolean | `true` | Master switch. When `false`, the router declines all requests. |
| `priority` | integer | `1` | Host plugin ordering; higher numbers run earlier for ModelRouter. |
| `endpoint` | string | `https://generativelanguage.googleapis.com/v1beta/interactions` | Override the upstream Interactions API URL. Use `https://us-central1-aiplatform.googleapis.com/v1/...` for Vertex. |
| `store` | boolean | `true` | Whether interactions are stored server-side so `previous_interaction_id` chaining works. Set `false` only for stateless debugging. |
| `agents` | array | `[antigravity-preview-05-2026, deep-research-preview-04-2026, deep-research-max-preview-04-2026]` | Names treated as agents (`agent` field) instead of models. |
| `default_environment` | string | `remote` | Used when an agent needs an `environment` field but no env ID has been captured yet. Leave empty to omit. |
| `route_all_models` | boolean | `true` | When `false`, only requests for known agents are claimed; Gemini chat models fall through to the built-in executor. |

## Auth

The plugin reads the Gemini API key from the auth record the host selected for the request. Configure an auth file in CLIProxyAPI with `provider=gemini_interactions` (or use any provider whose `AuthAttributes["api_key"]` resolves to a valid Gemini key) and the host will inject it as `x-goog-api-key` on the upstream call. For OAuth-backed Gemini CLI subscriptions, the host's existing auth injection works unchanged.

## Session map semantics

- Key: `blake3(model || established_messages || system || tools || generation_config)` where `established_messages` means every message *except* the last user turn.
- Value: `{ interaction_id, environment_id? }`.
- Hit: send `previous_interaction_id` + only the latest user turn as `input`.
- Miss on a non-empty established history: fall back to a fresh request (the server can't recover the missing history, so the chained prefix is dropped; the current turn still completes).
- Concurrent requests on the same key: insert-only semantics. The first response writes the binding; subsequent identical concurrent turns see it but always create a new interaction server-side (legacy behavior).

The map is process-local; restart drops it. Multi-instance deployments should add a shared backing store (Redis).

## Limitations of v0.1

- Text-only message handling. Multimodal parts (image/audio/video) are dropped silently.
- Tool/function calling from the client side is ignored. The plugin does forward tool definitions and folds `function_call` steps into `tool_calls` on the response, but client-follow-up `tool` roles are not yet mapped back to `function_result` steps.
- Streaming responses are returned as a single non-streaming chunk (no SSE translation yet).
- `background=true` long-running agents are not supported. The plugin blocks until the synchronous response arrives; agents that return `status=in_progress` will yield an error rather than polling.
- Cancellation propagation (aborting the upstream request when the client disconnects) is not implemented.

## Build

```bash
cargo build --release
```

The output is `target/release/libcpa_interactions_provider.so` (Linux), `libcpa_interactions_provider.dylib` (macOS), or `cpa_interactions_provider.dll` (Windows).

The repository ships a GitHub Action that builds these artifacts for Linux, macOS, and Windows and attaches them to each tagged release.

## License

MIT
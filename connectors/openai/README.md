# openai

The OpenAI connector (ADR-0217): the Responses and Chat Completions APIs by API key, and the
Codex subscription through a Codex login. With the `gpt` kit under `kits/` it is the owner's
"ChatGPT bundle".

| Route | Credential | Wire |
|---|---|---|
| `openai` | an API key (`OPENAI_API_KEY` or `p1 login openai`) | Responses over SSE |
| `openai-chat` | the same key | Chat Completions over SSE |
| `openai-codex-subscription` | `codex-oauth`: the Codex CLI login, imported into p1's store (`store_only`) | Responses on `/codex/responses` over WebSocket, falling back to SSE (ADR-0047) |

Modules (`components/`, built in the `connectors/` guest workspace):

- `connectors/codex-oauth`: the credential method that reads and refreshes a Codex login (the
  `codex` format of `connectors/shared/logins`);
- `connectors/codex`: the usage probe of the subscription's rate-limit windows.

`openai` and `openai-codex-subscription` bind every shipped GPT profile, `openai-chat` the GPT-5
ones. The connector prefers none: a bare GPT model keeps the route it runs on, and otherwise
follows ADR-0217 Decision 3: name a route
for the family or the model in `settings.toml [preferred_routes]`, or a bare model naming
several routes is refused.

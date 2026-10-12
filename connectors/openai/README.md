# openai

The OpenAI connector: the Responses and Chat Completions APIs by API key (`openai`, `openai-chat`) and the Codex subscription through a Codex login (`openai-codex-subscription`, WebSocket with SSE fallback), the `codex-oauth` login module and the `codex` usage probe.

Layout: `routes/`, `profiles/` (models only this vendor serves), `components/` (module sources); see `connectors/README.md` and ADR-0217. Filled by units 2 and 3 of #924.

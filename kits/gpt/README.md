# gpt

The GPT kit (family `gpt`, ADR-0217): the Codex CLI's tool set under Codex's own names, schemas
and result text (`exec_command`, `write_stdin`, `apply_patch`, `request_user_input`; ADR-0218,
`docs/design/tools.md` "The Codex faces"; nothing of p1's own, so no `skill` tool, and as a leaf
no worker or workflow tools), the GPT prompt,
the `AGENTS.override.md` instruction file and the compaction preferences of the owner's `gpt`
environment of b98a8167. Its `[context]` states no window: the Codex subscription's served
window (272,000) is the `context_limit` of the Codex route's bindings
(`connectors/openai/routes/openai-codex-subscription.toml`), not the public API's. With the
`openai` connector it is the "ChatGPT bundle". See `kits/README.md`.

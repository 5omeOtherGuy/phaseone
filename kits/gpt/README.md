# gpt

The GPT kit (family `gpt`, ADR-0217): the Codex tool pair `shell` + `apply_patch`, the GPT prompt,
the `AGENTS.override.md` instruction file and the compaction preferences of the owner's `gpt`
environment of b98a8167. Its `[context]` states no window: the Codex subscription's served
window (272,000) is the `context_limit` of the Codex route's bindings
(`connectors/openai/routes/openai-codex-subscription.toml`), not the public API's. With the
`openai` connector it is the "ChatGPT bundle". See `kits/README.md`.

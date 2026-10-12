# anthropic

The Anthropic connector (ADR-0217): the Messages API, reached two ways that never fall back to
each other.

| Route | Credential | Wire |
|---|---|---|
| `anthropic` | an API key (`ANTHROPIC_API_KEY` or `p1 login anthropic`) | Messages over HTTPS, `x-api-key` |
| `anthropic-subscription` | `claude-code-oauth`: the Claude Code login, imported into p1's store (`store_only`) | Messages over HTTPS with Claude Code's identity block and betas (`[adapter_settings.dialect]`) |

Modules (`components/`, built in the `connectors/` guest workspace):

- `connectors/claude-code-oauth`: the credential method that reads and refreshes a Claude Code
  login (the `claude_code` format of `connectors/shared/logins`);
- `connectors/claude`: the usage probe of the subscription's rate-limit windows.

Both routes bind every shipped Claude profile. The connector prefers neither: a bare Claude model
keeps the route it runs on, and otherwise follows ADR-0217 Decision 3.

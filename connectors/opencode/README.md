# opencode

The OpenCode connector (ADR-0217): OpenCode Zen and OpenCode Go by API key.

| Route | Credential | Wire |
|---|---|---|
| `opencode-zen` | `OPENCODE_ZEN_API_KEY` or `p1 login opencode-zen` | Chat Completions with OpenCode's client identity (`[adapter_settings.client_identity]`) |
| `opencode-go-subscription` | `OPENCODE_API_KEY` or `p1 login opencode-go-subscription` | Chat Completions |
| `opencode-go-messages` | `opencode-go-subscription`'s (`credential_route`) | Messages |
| `opencode-go-glm` | `opencode-go-subscription`'s (`credential_route`) | Chat Completions, GLM 5.3 |

Modules (`components/`, built in the `connectors/` guest workspace):

- `connectors/opencode`: the credential method that borrows the OpenCode CLI's login (the
  `opencode` format of `connectors/shared/logins`), for an account that names it;
- `connectors/opencode-go`: the usage probe of the Go plan's quota.

`opencode-go-subscription` and `opencode-go-messages` both bind `deepseek-v4.1-flash`. The
connector prefers neither: a bare DeepSeek model keeps the route it runs on, and otherwise
follows ADR-0217 Decision 3.

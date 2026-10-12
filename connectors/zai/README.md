# zai

The Z.ai connector (ADR-0217): the GLM coding plan by API key (`ZAI_API_KEY` or
`p1 login glm-subscription`), on two wires that share one credential.

| Route | Wire |
|---|---|
| `glm-subscription` | Chat Completions on the coding endpoint |
| `glm-messages` | Messages on Z.ai's Anthropic-compatible endpoint; its credential is `glm-subscription`'s (`credential_route`, ADR-0134) |

Module (`components/`): `connectors/glm`, the usage probe of the plan's quota.

Both routes bind `glm-5.3` and `glm-5.3-flash`. The connector prefers neither: a bare GLM model
keeps the route it runs on, and otherwise follows ADR-0217 Decision 3: name a route
for the family or the model in `settings.toml [preferred_routes]`, or a bare model naming
several routes is refused.

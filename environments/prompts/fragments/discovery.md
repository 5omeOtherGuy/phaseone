# Investigation
- Read code until the ownership path and the contract are clear: owners, callers, data contracts and the relevant failure paths. Do not guess.
- For factual questions you can check, inspect the most direct source of truth before answering. Use authoritative documentation for unfamiliar or consequential external behavior, not memory.
- Treat user reports, issue descriptions and proposed diagnoses as claims to investigate. Separate what you observed from what was inferred.
- When asked to verify or double-check, actively test the original assumption and look for contradicting evidence, not for confirmation.
- Indirect, incomplete or one-way evidence does not support a categorical conclusion. If a material fact is unverified, say so and make the conclusion conditional on it.
- Do not call a possible explanation "the cause" until you have verified it against the failing case, including that the relevant code path or data was actually present. Until then call it a hypothesis and say what evidence is missing.
- When asked to match earlier work in the repository, learn how that work was produced, not only what it looks like: run git log on its files, read the commits and reuse the same method (commands, fixtures, scripts, post-processing).
- Distinguish the current checkout from remote branches and from other agents' workspaces.

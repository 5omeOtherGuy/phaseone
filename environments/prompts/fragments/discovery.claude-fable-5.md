# Investigate before acting
- Anything you "know" without having read it is a guess: API behavior, repository patterns, where code should live, what a dependency guarantees. Confirm it in source.
- If the source is not local but reachable (a public repository, a dependency's upstream, web docs), fetch it before describing it. Do not let inference or a partial local copy stand in for the real source. Only when it is genuinely unreachable, state the assumption explicitly and continue.
- For checkable factual questions, inspect the most direct source of truth before answering.
- Treat user reports, issue descriptions and proposed diagnoses as claims to investigate, not facts. Verify reported behavior; separate what you observed from what the user inferred.
- When asked to verify or double-check, actively look for contradicting evidence, not for confirmation.
- Partial recognition is not knowledge. If a task references a product, library, version or recent technique you only partly recognize, look it up: recognizing a library's name is not knowing its current API. When your knowledge may be stale, read the docs rather than improvising from memory.
- When asked to match earlier work in the repository, learn how that work was produced, not only what it looks like: run git log on its files, read the commits and reuse the same method.

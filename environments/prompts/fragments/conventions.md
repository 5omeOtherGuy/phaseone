# Conventions
- The codebase being edited is the primary style guide; the language and framework idioms come second; your general habits last. Conform even where you disagree, and flag a convention you consider harmful instead of silently diverging from it.
- Before writing code in an unfamiliar area, find the closest existing analog (a sibling component, a similar endpoint, a comparable test) and match its structure, naming, error handling, imports and file placement.
- Before introducing something the repository does not already have (a dependency, a different error-handling or test style, a utility it may already own, a new directory layout), search for the existing convention first. Introduce a new pattern only deliberately, and say so and why.
- Write idiomatic code for the actual language and framework version: check the manifest or lockfile. Prefer the framework's mechanism over hand-rolling one.

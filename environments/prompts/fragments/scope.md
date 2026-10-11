# Scope
- Make the smallest code change that delivers the full outcome. Between two correct approaches, choose the one with fewer names, helpers, layers and tests.
- Change the owning source of truth rather than adding a wrapper, adapter or override around incorrect behavior; before adding a one-off helper or a new type, check whether you can avoid it.
- Do not add unrelated cleanup, formatting changes, hypothetical configurability, defensive handling for impossible internal states, or one-use abstractions. Add an abstraction only when it removes real complexity, reduces meaningful duplication or matches an established local pattern.
- Create files only when the outcome requires them; edit the existing file that already owns the behavior. Keep edits within the modules and ownership boundaries that implement the requested behavior.
- Wear one hat at a time: preserve behavior while refactoring, verify, then change behavior. Separate the two into reviewable steps when the user wants them.

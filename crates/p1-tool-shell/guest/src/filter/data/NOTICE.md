# Third-party notice: RTK filter definitions

## Pytest structured parser (#515)

`../structured/pytest.rs` adapts only the pure terminal summary and section
parser from RTK (<https://github.com/rtk-ai/rtk>), Apache License 2.0, pinned
commit `6d4b77e`, path `src/cmds/python/pytest_cmd.rs` (parser at lines 77 and
311). Read-only donor snapshot:
`~/.agents/xo/dispatch/p1-iris-tools-audit/src/rtk/src/cmds/python/pytest_cmd.rs`.
The license is `LICENSE-APACHE-2.0` in this directory.

p1 modifications: strict count/duration recognition; errors, empty collection,
xfail/xpass and warnings supported; diagnostic sections retained verbatim,
without donor caps, truncation or reindentation; unknown, malformed, interrupted
or plugin output declines. No runner code, injected flags or command rewriting
copied. Shrinking and recovery notices use p1's common renderer.

## Declarative filter definitions

The `.json` files in this directory are one-off JSON conversions (Python
`tomllib`, content unchanged) of the `.toml` filter files of the donor
`iris-agent` (`src/tools/bash/filter/data/`), most of which Iris vendored from
RTK. p1's shell guest may depend only on serde, serde_json and regex
(ADR-0081), so it reads JSON rather than TOML. Of the donor's 64 files, 62 are
here: `npm-install` (Iris-authored) and `shellcheck` are left out because p1's
filter corpus (`crates/p1-tool-shell/tests/output_filters.rs`) requires those
classes to pass through raw. The conversion drops TOML comments; the donor
files marked `# modified from RTK upstream: added unless error-guards` are
brew-install, bundle-install, composer-install, dotnet-build, poetry-install,
pulumi-stack, quarto-render, tofu-validate and uv-sync.

p1 modifications (#507, #509): only `match_command` patterns changed. A
program name followed by `\b` also matched hyphenated neighbours
(`ssh-keygen`, `helm-docs`, `iptables-save`, `dotnet build-server`), so every
trailing `\b` became `(?:\s|$)`, as RTK upstream tightened ssh, liquibase and
spring-boot after the pinned commit; `liquibase` is anchored at the start,
`spring-boot` takes upstream's jar-name rule, `gradle` matches `gradle`,
`gradlew` and `./gradlew` (it required the name twice), and `gcc` matches
`g++` (a `\b` after `++` never matched).
#521: the end is now `(?:\s|[<>]|&>|$)`, as a redirection ends a shell word (`jq<in.json`).

The donor notice follows, unchanged.

Most `.toml` files in this directory are vendored from RTK
(<https://github.com/rtk-ai/rtk>), licensed under the Apache License 2.0
(see `LICENSE-APACHE-2.0` in this directory).

- Upstream: rtk-ai/rtk, commit `31f9d43d81f90d29e89142f3306473e786e59f6c`
  (2026-07-03), path `src/filters/*.toml`.
- Iris modifications are marked with a `# modified from RTK upstream: ...`
  comment at the top of the affected file. The systematic change: the Iris
  engine requires an `unless` error-guard on every `match_output`
  short-circuit rule (ADR-0037), so guards were added where upstream had none.
- Files with an `# iris-authored` header comment are original to Iris and are
  not from RTK.
- Inline `[[tests.<name>]]` sections are the upstream test cases, ported
  verbatim unless the file is marked modified; they run in
  `src/tools/bash/filter/mod.rs` unit tests.

Re-syncing with upstream: diff this directory against `src/filters/` at the
upstream HEAD, re-apply the marked Iris modifications, and run `cargo test`
(the inline tests and the corpus quality suite are the acceptance gate).

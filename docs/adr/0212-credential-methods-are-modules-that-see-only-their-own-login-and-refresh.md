---
adr: 212
title: Credential methods are modules that see only their own login and refresh
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0040, ADR-0074, ADR-0081, ADR-0086, ADR-0110, ADR-0200, ADR-0209, ADR-0210, ADR-0211, issue #886, issue #889]
---
# ADR-0212: Credential methods are modules that see only their own login and refresh

## Context

ADR-0211 made a credential method a port, `p1_auth::CredentialMethod`. The subscription
connectors' methods still lived in `p1-auth` behind the cargo feature `subscriptions`:
- the Claude Code and Codex OAuth logins;
- the OpenCode and Pi key stores.

Wave 3 of the split (#886) moves the connectors out of the core. #889 asks for their code to
become modules built with the SDK and loaded through the installed sets of ADR-0209.

Until now a module never received a credential value. A provider module gets a bearer scheme
and an account-id header from the WIT; the native transport broker attaches the token
(ADR-0086). A credential module must read a login file and build a refresh exchange, so it must
see values.

The lead decided for the owner, 2026-10-12, option A (credential modules), under six
constraints:
1. Least exposure. A credential module receives only the bytes of its own method's login file
   and its refresh token exchange. It never gets the API access token on the request path.
   Provider modules keep today's invariant: the native broker attaches the token.
2. Usage probes. The host builds the request from the module's declared endpoint and attaches
   the token natively. The module only decodes the response body. Probe modules see no
   credential.
3. Refresh. The host sends a refresh only over HTTPS, and only to the token origins the
   module's manifest declares. Those origins are:
   - shown at `p1 modules install`;
   - pinned in the lock entry beside the digest;
   - confirmed again when they change.

   Locks, pinned-directory I/O and the bounded exchange stay native.
4. Empty class allocation. No network, files, clock or randomness: pure functions only. The
   loader refuses anything more (`ImportsExceedAllocation`).
5. Only the module the route's credential method selects is handed values, and only for that
   route's login.
6. This record names the invariant it narrows and the residual risk. The owner can supersede
   it.

## Decision

- **A new module class, `credential-method`**, with the world
  `p1:module/credential-method` in WIT 1.1.0. The WIT package and the SDK go from 1.0.0 to
  1.1.0 (ADR-0210: additive, a host serves 1.0 modules unchanged). The SDK gains the
  `credential-method` feature.
  - The world exports five pure functions over JSON texts and bytes (shapes:
    `docs/design/modules/credential.md`):
    - `describe`;
    - `read-login(file, key)`;
    - `refresh-request(refresh-token, file)`;
    - `refresh-response(body, sent-at-ms)`;
    - `merge-login(file, body, sent-at-ms, now-ms, keep-access)`.
  - It imports nothing. Its allocation in `modules/capabilities.toml` is `[]`, so a component
    that imports even a type-only interface is refused.
- **Token origins are part of the package's identity.**
  - A credential method's manifest declares `token_origins`. Each origin is
    `https://host[:port]`; a keyed login that never refreshes declares `[]`. No other class
    may declare any.
  - The build, the release manifest and the package archive carry them.
  - `p1 modules install` prints them. It installs a credential method only with
    `--confirm-origins`, both on its first install and whenever the origins differ from the
    lock's.
  - The lock entry pins them, and a session refuses a package whose set declares others
    (`LockMismatch` on `token_origins`).
- **Everything that is not a format stays native and generic in `p1-auth`.**
  - `ModuleMethod` turns a module's description into a `CredentialMethod`.
  - `LoginFile` reads, locks, refreshes and writes back a login file through the pinned
    directory. It runs the single-flight rotation that the Claude Code and Codex sources used
    to run separately.
  - `KeyFile` reads one entry of a keyed login.
  - Before anything is sent, `ModuleMethod` refuses a refresh request that is not HTTPS to a
    declared origin.
  - p1 keeps the clock and decides what a token is worth:
    - the 5-minute refresh margin;
    - an expired or rejected token is refused;
    - a store entry needs an expiry.
- **The composition is explicit.**
  - A route's `kind`, or a `borrow` entry's store, that is not `api-key` or `none` names a
    credential module.
  - The host's `credentials::InstalledCredentials` lists the installed ones: a lock entry of
    the class, else the release's `p1/<name>` package.
  - The host hands that list to `Locations::with_modules`, and `p1-auth`'s `Methods` resolves
    names through it once per lookup.
  - Listing reads manifests and locks only. A module is compiled, from the process-wide memo,
    the first time a route names it.
  - A name no installed module provides still parses. It is refused when the route is used,
    and by `p1 env check`.
- **The connectors' logins are modules.**
  - The four formats (Claude Code, Codex, OpenCode and Pi) left `p1-auth` for the connectors'
    own crate. Four components export them with the SDK, under the method and store names
    routes already spell: `claude-code-oauth`, `codex-oauth`, `opencode` and `pi`.
  - The two OAuth ones declare `https://platform.claude.com` and `https://auth.openai.com`.
  - Where the connectors' packages live and how a distribution installs them is #889's next
    part. Until then they are built into p1's release set.
  - `p1-auth` has no `subscriptions` feature any more.
  - `p1 login <route> --import [DIR]` copies any OAuth module method's login into p1's store.
    `--from-claude-code` remains its other spelling.
- **The invariant this narrows.** Before: "a module never receives a credential value". Now:
  "only a credential-class module of the route's own method, and only its login's values".
  - Only that module is called for that route, and only with that route's login file or its
    refresh response.
  - Every other class, provider modules included, still receives none.

## Consequences

- **Residual risk.** An installed credential module can read the tokens of its own login and
  send them out through its declared token origin. The install-time confirmation and the
  pinned origins bound where they can go; they do not stop a malicious module from sending
  them there. The owner can supersede this record.
- The Claude Code import no longer reads Claude Code's account file (`.claude.json`): the
  module sees only the login file. An imported store entry records no account id. Claude routes
  use it only to scrub echoes; existing entries keep theirs.
- The two OAuth rotations are one now.
  - A Codex refresh whose response rotates no refresh token, but carries a usable access token,
    is written back keeping the old refresh token, as Claude Code's was. It used to fail.
  - Error messages name the login uniformly: "the Codex login at … cannot be used: …; run
    `codex login`".
- `p1 login <route> --import` works for a Codex route too.
- A route file naming an uninstalled method loads, and fails when it is used.
- A third-party credential method whose route borrows a login is limited to origins p1 ships
  for that method (ADR-0110). A store-only route uses endpoint approval instead.
- Usage probes and the provider dialects are #889's next part; until then they still name the
  connector methods by name in `p1-usage` and the provider crates.
- Pushing tag `module-sdk-v1.1.0` on the merge commit releases the SDK with the new feature.
- The bump exposed one place where an older minor did not link. The restricted path stubs
  every unknown import as a trap under its exact name, and that stub shadowed the semver
  lookup of a 1.0 component's `workers-observe` and `subagents-start`. Those lists are now
  linked under the name the component imports them by.

## Alternatives considered

- **Native connector crates linked by a build feature**, the state ADR-0200 left. Rejected:
  the owner wants modules developed outside this repository (ADR-0211's context), and a cargo
  feature cannot be installed.
- **A credential module given network or file capabilities** to run its own exchange.
  Rejected by the lead's constraints 3 and 4: locks, pinned I/O and the bounded exchange stay
  native, and the class imports nothing.
- **Typed WIT records instead of JSON texts.** JSON keeps the world small and matches
  `workflow-decision`. `p1-auth` checks every answer: header-safe tokens, origin-checked
  requests, closed description fields.
- **A separate WIT package `p1:credential`.** Rejected: one package, one version rule and one
  SDK (ADR-0210).

## Evidence

- `cargo test -p p1-auth` (52 passed). The new `tests/methods.rs` covers:
  - composition and listing;
  - an uninstalled method refused with the installed list;
  - the chain over a module method;
  - a keyed login by name;
  - refresh requests to an undeclared origin, over http, or through a userinfo URL are never
    sent, and the login file stays byte-identical.
- `cargo test -p p1-connector-logins` (99 passed): the former `p1-auth` connector suites
  (rotation, contention, write-back, login_dir, precedence, store-only, validation, Codex),
  driven natively through the chain.
- `crates/p1-module-tests/tests/credential_modules.rs`:
  - the built `p1/codex-oauth` and `p1/opencode` components serve the chain, including a real
    refresh and write-back;
  - install refuses without `--confirm-origins` and asks again when origins change;
  - the lock pins the origins, and a mismatched pin is refused at load;
  - a component that imports anything is refused.
- `scripts/check-module-boundaries.sh`: clean, with the four credential components importing
  nothing.

# Changelog

User-visible changes to p1, newest first. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). p1 is pre-release and has no version numbers yet: each release is tagged `main-<sha>` (see the README's Install section), and the first entries below are the changes since the latest release, `main-ca4a85651478` (2026-10-12).

## [Unreleased]

### Added

- The Codex tool set for gpt models: `exec_command`, `write_stdin`, `apply_patch`, `request_user_input` and `update_plan`, each with Codex's own name, schema and result text; the gpt kit sends the four Codex sends by default ([#936](https://github.com/5omeOtherGuy/phaseone/pull/936), ADR-0219).
- `SECURITY.md` and issue forms for bug reports and feature requests ([#947](https://github.com/5omeOtherGuy/phaseone/pull/947)).

### Fixed

- The module runtime takes wasmtime 49.0.3, the security releases that fix host panics and a stack buffer overflow a guest module could trigger ([#949](https://github.com/5omeOtherGuy/phaseone/pull/949)).

### Changed

- Releases are published only when the owner dispatches `release.yml`; nothing publishes on a schedule or prunes old releases ([#935](https://github.com/5omeOtherGuy/phaseone/pull/935), ADR-0218).
- The release binary is built on a pinned Ubuntu 24.04 runner, so it keeps requiring glibc 2.39 or newer instead of moving with GitHub's `ubuntu-latest` ([#941](https://github.com/5omeOtherGuy/phaseone/pull/941)).

---
adr: 180
title: Attachments are references in history, a new user-content item and a content-addressed local store
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: ["https://github.com/5omeOtherGuy/phaseone/issues/791", "https://github.com/5omeOtherGuy/phaseone/issues/710", "docs/design/seams.md (family 4 attachments, family 5 model capabilities)"]
---
# ADR-0180: Attachments are references in history, a new user-content item and a content-addressed local store

## Context

The seam design #710 (ADR-0153, decision 9) plans image and file input: an `AttachmentStore`
port with reference types in `p1-contracts`, `Item::User` growing from text to content blocks,
`RouteDescription.input_modalities`, a per-route projection in each provider, and a local store
adapter (pinned directory, exchange writes, 0400 objects, never deleted; ADR-0108, ADR-0111).
The build order #791 orders its first slice and amends one point: the frozen core suites
construct `Item::User { text }` and assert its journal records byte-exactly, so that variant
and its `user_input` record must not change.

Providers run as WebAssembly modules (ADR-0071) and cannot reach a host store, while a request
for an image-capable model has to carry the image's bytes. `ProviderRequest` cannot grow a field
without changing every request literal, frozen expectations among them.

## Decision

1. **Contracts** (`p1-contracts::attachment`): `AttachmentId` (lowercase hex SHA-256 of the
   stored bytes, checked on construction and deserialization so it is never path-shaped),
   `ImageType`, `ImageRef`, `FileRef`, `UserBlock { Text, Image, File }`, `ImageLimits`,
   `ImageTarget`, `AttachmentError` and the host-side `AttachmentStore` port with the #710
   signatures. `RouteDescription.input_modalities: Vec<InputModality>` (omitted when empty, so a
   text-only route's `Environment` record is unchanged); profiles carry the same list
   (`input_modalities = ["text", "image"]`).
2. **History, deviating from #710:** `Item::User { text }` and `RecordBody::UserInput { text }`
   are unchanged. Input with attachments is a new `Item::UserContent { blocks }`, journalled as
   a new `RecordBody::UserContent { blocks }` and projected back on resume. Text-only input keeps
   using `Item::User`.
3. **References only, resolved per request:** `UserBlock::Image` has a `data` field (base64) and
   `UserBlock::File` a `path` field that serde skips. The host's provider decorator
   (`p1-host/src/attachments.rs`, composed at the one root around every route provider) fills
   them for one request: the image bytes only when the route reports image input, the file's
   read-only path always. The module protocol carries both to the provider module
   (`WireUserBlock`); no journal record, summary or replaced history can carry them.
4. **Projection:** the Messages adapter sends a base64 image block when its profile takes
   images and refuses a request whose image was not resolved; every other block, and every
   block on every other route, goes as text: an image as its placeholder, a file as one line
   naming its read-only path (`UserBlock::as_text`).
5. **Local store** (`p1-attachment-local`, a native host OS service, ADR-0081):
   `<state dir>/attachments/objects/<sha256>` (ADR-0171), created 0700 on the first save. The
   root and `objects` are opened with `O_NOFOLLOW` and refused when another user owns or can
   write them; an object is staged exclusively, synced and linked with `RENAME_NOREPLACE` (a hard
   link where `renameat2` is missing), mode 0400, never replaced and never deleted. Every read
   re-hashes the object. Images are validated without a codec: the format by signature, the
   dimensions from the header, limits of 5 MB and 8000 px a side, and metadata cut losslessly
   (PNG text/EXIF/time chunks; JPEG APP1 and the other metadata APP segments and comments, APP0,
   APP2 and APP14 kept). A file's display name loses its path and control characters and is
   credential-masked before it is stored in a reference.

## Consequences

- The journal stays the single truth with references only; an attachment message survives
  resume, and a text-only session's journal is byte-identical to before.
- Every exhaustive match on `Item` or `RecordBody` gains one arm. Two frozen suites
  (`p1-context/tests/acceptance_sol.rs`, `p1-core/tests/acceptance_lead.rs`) needed one
  compile-forced arm each (dispatcher-approved 2026-10-11); no assertion changed.
- A JPEG loses its orientation flag with its EXIF, since the pixels cannot be rotated without
  re-encoding; GIF and WebP metadata is not cut yet; an image above a route's target is refused,
  not downscaled. Re-encoding needs an image crate, which is an owner question.
- A file's read-only path lies outside the workspace, so the confined file tools cannot read
  it yet; the `attachments` WIT capability and `read_image` tool are a later slice.
- `p1-core` has no entry point for a user-content turn yet: the front-end slice (#694, ACP image
  prompts) adds it with the intake path.
- No shipped profile takes images yet: flagging one changes its routes' composed `Environment`, so
  a session recorded before resumes with "environment changed". The profiles are flagged with the
  intake slice (#694), the first that can put an image into history; other routes keep text
  placeholders until their wire mapping lands.

## Alternatives considered

- Growing `Item::User` into blocks (#710 as written): changes the frozen suites' records.
- A field on `ProviderRequest` carrying resolved bytes: changes every request literal, frozen
  expectations among them.
- Resolving inside the provider: a module has no store and must not get one.
- A fourth "resolved image" block variant: a value valid in one place only; the skipped fields
  make the journal guarantee structural instead.
- A new image or hash crate: no new dependency in this slice; SHA-256 is copied (with its FIPS
  vectors) from `p1-module-runtime/src/sha256.rs`, base64 is a short encoder in the contracts.

## Evidence

`cargo test -p p1-attachment-local` (store round trip, 0400/0700 modes, dedup, corrupt and
missing objects, symlinked and shared roots, image sniffing, dimensions and metadata cuts);
`cargo test -p p1-journal --test attachments` (record holds references only, `user_input`
unchanged, projection); `cargo test -p p1-provider-anthropic --test images`;
`cargo test -p p1-provider-openai -p p1-provider-openai-chat --lib attachments`;
`cargo test -p p1-module-protocol` (schemas, fixtures, refused path-shaped ids); `cargo test -p
p1-host --lib attachments` (decorator); `cargo test -p p1-core` (frozen suites green).

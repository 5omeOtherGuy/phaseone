---
adr: 183
title: ACP image prompts are user-content turns, refused where the route takes no images
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: [694, 670, 791, 766, "docs/design/seams.md (family 4 image attachments, family 13 ACP agent door)"]
---
# ADR-0183: ACP image prompts are user-content turns, refused where the route takes no images

## Context

dsh advertises `promptCapabilities.image` and accepts ACP image blocks (owner 2026-10-10: the dsh
floor, #694). ADR-0180 (#791) landed the attachment contracts, `Item::UserContent` and
`RecordBody::UserContent`, the local store and Anthropic image blocks, and left three things to
this slice: a core entry point for a user-content turn, a shipped profile that takes images, and
the readers of `UserContent` (ACP replay, the session catalog's first prompt, the shadow hook).
The dispatcher's brief: advertise `image` only when the session's active route accepts images,
refuse an image on any other route with invalid params, never drop it, store the bytes and keep
references in the journal.

`initialize` is answered before a session exists: the router of several sessions (ADR-0156)
answers it before any session process has assembled its provider, and a session process answers
it while the host still assembles (#695). A model switch can change the route mid-session.

## Decision

1. **Core API:** `Agent::run_content_turn(blocks, cancel)` runs a turn whose input is
   `UserBlock`s: it commits `RecordBody::UserContent` and pushes `Item::UserContent` where
   `run_turn` commits `UserInput` and pushes `Item::User`; nothing else differs. `Agent::route()`
   returns the current provider's `RouteDescription`. Text-only turns are unchanged.
2. **Front-end port:** `SessionHandle` gains three methods with defaults that refuse:
   `save_image(bytes)` validates and stores one image and returns its `ImageRef`, refused when
   the route of the next turn takes no image input (`Agent::route().accepts_images()`, read
   under the agent lock, so after a waiting model switch) or the store refuses it;
   `read_image(image)` returns stored bytes for a replay; `prompt_content(blocks, cancel)` runs
   the content turn. `ImageData::decode` (base64, padding optional) joins the encoder in
   `p1-contracts::attachment`.
3. **ACP:** a `session/prompt` with at least one `image` block (PNG, JPEG, GIF, WebP `mimeType`,
   base64 `data`) is a content prompt: its images are saved in the client's order, then the turn
   runs on text and image blocks, a resource-link list as a last text block. Any refusal answers
   invalid params (`-32602`) before a turn, with the reason; the prompt is not run. A prompt
   without an image stays a text prompt (byte-identical `user_input`). A content prompt is never
   a slash command. `_session/steering` takes no images.
4. **Advertising:** `promptCapabilities.image` is decided by the host at start, for the model
   the run options select: its profile lists `image` and its route's adapter is one whose
   provider sends images (`routes::IMAGE_ADAPTERS`, today `anthropic-messages`). The router
   (`serve_with_archive(.., image_prompts)`) and the session driver
   (`AcpFrontEnd::with_image_prompts`) advertise that value; each prompt is still checked
   against the live route (decision 2).
5. **Replay:** `session/load` sends a `UserContent` record's text blocks as text
   `user_message_chunk`s and each image as an image `user_message_chunk` with the bytes read from
   the store; an image the store cannot read is its `ImageRef::unavailable` placeholder text.
   The session catalog previews such a prompt as its text with `[image]` markers; the shadow hook
   observes its text blocks (nothing for an image alone).
6. **Profiles:** the six shipped Claude profiles take `input_modalities = ["text", "image"]`
   (revision 2).

## Consequences

- An ACP client can send screenshots to a Claude session; the journal holds references only,
  and a loaded session shows the images again.
- Flagging the Claude profiles changes those routes' composed `Environment` record
  (`input_modalities`): a Claude session recorded before this change resumes with "environment
  changed", as ADR-0180 foresaw. Text-only routes are unchanged.
- The advertised flag is the start-time model's: after a switch to a model without images the
  client may still send images and gets invalid params; after a switch from such a model to one
  with images the flag stays false, though images would be accepted.
- `IMAGE_ADAPTERS` restates, in the host, which provider adapters carry images; a provider that
  gains an image wire must be added there too, or the router will not advertise it.
- An image refused after earlier images of the same prompt were stored leaves those objects in
  the store, unreferenced: the store is content-addressed and never deletes (ADR-0180).
- Replay sends image bytes back over stdio: a long session with many images replays them all.

## Alternatives considered

- Building the selected provider in the router to read its `RouteDescription`: exact, but the
  router assembles nothing today (ADR-0156) and would activate a provider module per start.
- Advertising from the profile alone: a Claude profile bound to an OpenAI route would advertise
  images its wire turns into placeholders.
- Advertising `false` and accepting images anyway: hides a capability the dsh floor requires.
- A placeholder instead of the bytes in replay: simpler, but the client loses the image it sent;
  kept only as the fallback when the store cannot read it.
- A new `TurnEnd` variant for a refused prompt: changes an exhaustive contract enum; the refusal
  happens before the turn instead.

## Evidence

`cargo test -p p1-host --test acp_fixtures image` (fixture `docs/acp/fixtures/image-prompt.jsonl`:
advertised capability, stored and stripped image, reference-only request, image replay; refusal
on a text-only route and for bytes that are no image, with nothing stored or sent);
`cargo test -p p1-acp --test driver image` (mapping order, refusals, steering, placeholder
replay); `cargo test -p p1-acp --test router advertises`; `cargo test -p p1-core --test impl_core
content_turn`; `cargo test -p p1-host --lib image_prompts` (shipped Claude and DeepSeek
selections, a Claude profile on an OpenAI Chat route); `cargo test -p p1-contracts base64`.

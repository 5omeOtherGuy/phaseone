# Iris TUI disposition manifest — staged, not complete

Tracking: #91; first slice #93. Donor **only**
`5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`.
Every path below is relative to donor `src/ui/`. Lead independently verified the
53-file tree and licence headers. Initial inventory was authored by wf1/w2
(`openai-chat/opencode-zen-free/space-bunny-free`) and checked against pinned source.

This is a **file-complete inventory, not a symbol-complete audit**. Named symbols
are audit anchors, not an assertion that other private helpers were reviewed.
Dependencies list direct audit leads, not recursively closed dependency graphs.
Unlisted symbols in every file remain **Blocked** pending detailed audit.
No row marked Blocked is implemented, proven equivalent or approved for exclusion.
External source reach must be expanded before programme closure.

For every Blocked row: issue = #91; acceptance = not yet authored; destination and
consumer = **candidate only** in the final column. No donor runtime dependency is
authorized. Pure value/render code belongs in p1-tui; effects belong in p1-host.
All MIT rows use donor root LICENSE (copyright 2026 5omeOtherGuy).
`Apache` means Apache-2.0 SPDX header plus NOTICE and LICENSE-APACHE obligations;
none of those files is copied in #93.

## Selected symbol dispositions

| File / symbol | Licence; dependencies | Disposition / adaptation | p1 destination and real consumer | Issue / acceptance |
|---|---|---|---|---|
| `textengine.rs::transform` | MIT; std iterator/string only for selected logic | **Adapt**, pending verification: single-line sanitizer; tabs one space instead of donor eight-column stops; always drop other controls | private p1-tui text helper; `Band::render` both sides before layout | #93; independent Band text safety cases, donor parser cases, existing SLAB oracles |
| `textengine.rs::consume_csi` | MIT; std | **Adapt**, pending verification: retain final-byte scan and consume incomplete sequences to end; preserve styles across segment boundaries | same Band consumer | #93; ESC/C1 CSI and incomplete cases |
| `textengine.rs::consume_string_control` | MIT; std | **Adapt**, pending verification: retain BEL, ESC-backslash and C1 ST termination | same Band consumer | #93; OSC/DCS/SOS/PM/APC termination cases |
| `textengine.rs::tests::{strip_ansi_keeps_controls_clean_text_drops_them,strip_ansi_handles_osc_with_st_and_bel}` | MIT; selected helpers | **Adapt**, pending verification: distinguish display policy from raw stripping/tab expansion | p1-tui sanitizer unit tests | #93; migrated expectations with explicit tab-policy change |
| `textengine.rs::{clean_text,expand_tabs}` | MIT; unicode-segmentation/width for tab stops | **Blocked** as whole implementations: #93 adapts the parser, not the tab-stop engine | later text foundation consumer audit | #91; Unicode/tab cases pending |
| `textengine.rs::{display_width,cluster_width,cluster_advance,truncate_to_width,wrap_to_width}` | MIT; unicode-segmentation, unicode-width | **Adapt** the bounded-progress wrapping policy: cell-width rows, oversized clusters as styled ellipses, zero-width/ZWJ word units retained when they fit; other width/truncate symbols Blocked | `crates/p1-tui/src/wrap.rs::{cell_width,wrap,wrap_len,wrap_styled}`, consumed by transcript measurement/render and ledger goals | #94 accepted; bounded progress, grapheme/ZWJ and rendered-bound cases |
| `textengine.rs::{ZWJ_SHAPING,set_zwj_shaping,zwj_shaping,normalize_zwj_with,substitute_zwj}` | MIT; OnceLock, segmentation; probe coupling | **Blocked**: no global shaping state/probe port authorized | host-owned capabilities, injected pure rendering only if a consumer needs it | #91; capability decisions/tests pending |
| remaining `textengine.rs` symbols/tests | MIT; see file inventory | **Blocked** pending individual audit, not silently excluded | undecided | #91 |

The first slice does not sanitize every renderer, fix pre-Band clipping elsewhere,
alter stored/canonical text, activate links, interpret ANSI styles or add dependencies.

## All donor UI files

| Path | Key symbols | Licence | Dependency / external-reach audit leads | Disposition; candidate destination / consumer |
|---|---|---|---|---|
| `ask_user_question.rs` | AskUserDialog, AskUserDialogOutcome, choice_line | MIT | anyhow, ratatui, serde_json; nexus, tools::ask_user_question, modal | Blocked; forms with host-owned answers |
| `clipboard.rs` | CopyMethod, copy, emit_osc52, osc52_sequence | MIT | anyhow, base64; subprocess/terminal I/O | Blocked; host clipboard seam |
| `delegation_dashboard.rs` | DelegationDashboard, DelegationRequest/Response, execute_request | MIT | iris-subagent-runtime, ratatui; tools previews, wayland::subagents | Blocked; worker views after #89 seam acceptance |
| `harness_actor.rs` | HarnessActor, HarnessCommand/Event, ActorState | MIT | anyhow, tokio, tokio-util; cli, goal, mimir, nexus, signals, wayland | Blocked; host responsiveness equivalence audit |
| `highlight.rs` | highlight, is_known_syntax, code_highlighter, scope_style | MIT | ratatui, syntect; markdown, palette | Blocked; transcript code/output, dependency decision needed |
| `hyperlink.rs` | sanitized_link_uri, LinkRegion, find_file_refs, linkify_file_refs | MIT | ratatui; textengine, terminal_surface | Blocked; pure link display, host activation |
| `login.rs` | LoginUpdate/Outcome, LoginBackend | MIT | anyhow, reqwest, tokio-util; config, mimir auth/selection | Blocked; generic forms only, host auth effects |
| `markdown.rs` | render_markdown, MarkdownTheme, LineClass, HighlightFn | MIT | pulldown-cmark, ratatui, unicode-segmentation; highlight, hyperlink | Blocked; transcript, parser approval/equivalence needed |
| `mod.rs` | Ui, UiEvent, UiBridge, TurnErrorKind | MIT | anyhow; nexus events and all UI wiring | Blocked; audit wiring, retain p1 contracts |
| `modal.rs` | Modal, ModalKey/Action/Outcome | MIT | ratatui; mimir, nexus, selector, wayland skills/trust | Blocked; pure input/focus mechanisms only |
| `palette.rs` | ColorDepth, depth detection/adaptation | MIT | ratatui; theme, environment | Blocked; retain SLAB palette; audit capability adaptation |
| `picker.rs` | model_command, apply_action, resume/tasks/settings views | MIT | anyhow; cli/config/git/mimir/nexus/session/wayland | Blocked; existing p1 picker/host seams |
| `screen_mode.rs` | ScreenMode, AltScreenConfig, Resolution, resolve_for_startup | MIT | config flags, terminal_env | Blocked; host single terminal lifecycle |
| `selector.rs` | Selector, SelectorItem, fuzzy_match, scroll_offset | MIT | std; slash-policy references | Blocked; existing picker, substring contract must remain |
| `settings_menu.rs` | SettingsPanel, Field, RowId, PanelRow/View | MIT | ratatui; config, mimir, wayland trust | Blocked; pure form navigation, host effects |
| `slash.rs` | SlashAction, SlashCommand, COMMANDS, Palette | MIT | cli and tui_loop action references | Blocked; retain p1 command definitions |
| `steering.rs` | SteeringQueue | MIT | nexus::SteeringSource | Blocked; existing host/input queue equivalence |
| `symbols.rs` | glyph/status constants, FLOW_FILL | MIT | std | Blocked; retain SLAB vocabulary, audit capabilities |
| `task_view.rs` | TaskKind, TaskCard, body_preview | MIT | session, wayland::git_safety | Blocked; no task engine port |
| `terminal_doctor.rs` | DoctorEnv, detect, report | MIT | terminal_env, textengine, symbols | Blocked; host diagnostics |
| `terminal_env.rs` | TerminalEnv, ControlModeProbe, tmux_probe | MIT | std environment/subprocess | Blocked; host capabilities |
| `terminal_surface.rs` | TerminalSurface, RenderState/Stats/Line | MIT | ratatui, unicode-width; hyperlink, terminal I/O | Blocked; audit restore helpers, no second renderer |
| `text.rs` | TextUi, paste/approval/input helpers | MIT | anyhow, unicode-segmentation; approval, nexus, tool_display | Blocked; preserve existing line frontend |
| `textengine.rs` | parser, width, ZWJ, wrap/truncate families | MIT | unicode-segmentation, unicode-width; zwj_probe reference | **Adapted** bounded-progress, cell-width wrapping with oversized clusters rendered as ellipses, and the sanitizer applied before transcript measurement/wrapping; named width/ZWJ families and remainder Blocked; `crates/p1-tui/src/wrap.rs::{cell_width,wrap,wrap_len,wrap_paragraphs,wrap_styled}`, consumed by transcript measurement/render and ledger goals; p1-tui pure `text` sanitizer, consumed by `render::transcript`; #94 and #135 accepted |
| `theme.rs` | Theme, available, resolve, active global | MIT | ratatui; palette | Blocked; no donor theme framework port |
| `tui.rs` | TuiUi, composition, input/render driver | MIT | anyhow, ratatui; git, metrics, mimir, nexus, signals, telemetry | Blocked; split pure helpers from host lifetime |
| `tui/activity.rs` | WorkPhase | MIT | nexus::ToolCall, tool_display, UiEvent | Blocked; activity from p1 events/injected time |
| `tui/component.rs` | Component, Container, cursor markers | MIT | ratatui; terminal_surface, wrap | Blocked; no component registry |
| `tui/frame_stats.rs` | FrameStats, timing summaries | MIT | std timing | Blocked; deterministic work counters if needed |
| `tui/overlay.rs` | FocusTarget, overlay_menu | MIT | ratatui; palette, selector, slash, component | Blocked; existing picker/focus |
| `tui/pager.rs` | PagerSurface, frame/search/follow/lifecycle | MIT | ratatui; nexus, signals, terminal_surface, textengine | **Adapted**: clamp a stale transcript scroll anchor to current rendered bounds; remainder Blocked; p1-tui `Screen::scroll_mark`/`scroll_by`, consumed by transcript scroll/page input; #140 accepted |
| `tui/pane.rs` | assistant/user markdown rows | MIT | ratatui; markdown, symbols, panel, rows, wrap | Blocked; transcript consumers |
| `tui/panel.rs` | PanelState, header/footer/body/diff layout | MIT | ratatui, similar; tool_display, highlight, textengine | Blocked; generic tool-neutral blocks |
| `tui/rows.rs` | TranscriptRow, ChromeRow, overflow/rules | MIT | ratatui; component, panel, wrap | Blocked; transcript geometry/cache audit |
| `tui/screen.rs` | Screen, SessionMeter, flow meter, editor | MIT | iris-subagent-runtime, ratatui, ratatui-textarea; config/git/goal/metrics/mimir/nexus | Blocked; composer dependency approval and pure state extraction |
| `tui/session_menu/git_menu.rs` | GitMenu, valid_branch_name | MIT | ratatui; git::status, symbols, wrap | Blocked; pure menu, host discovery/actions |
| `tui/session_menu/jj_menu.rs` | JjMenu | MIT | ratatui; git::status, symbols | Blocked; missing p1 service/consumer audit |
| `tui/session_menu/mod.rs` | SessionMenu, MenuKey/Action/Outcome | MIT | ratatui; git, palette, selector, slash | Blocked; generic windowing only |
| `tui/session_menu/tree_menu.rs` | TreeMenu | MIT | ratatui; git, wayland::git_safety | Blocked; host traversal, pure rows |
| `tui/shell_command.rs` | ShellCommand, format_payload, heredoc parsing | MIT | tool_display::shorten_paths_in_text | Blocked; neutral tool face only, no name dispatch |
| `tui/startup.rs` | StartPage, StartAction | MIT | ratatui; palette, symbols, component, wrap | Blocked; retain SLAB home behavior |
| `tui/streaming/chunking.rs` | AdaptiveChunkingPolicy, QueueSnapshot, DrainPlan | Apache | std timing | Blocked; pure policy only with injected time |
| `tui/streaming/collector.rs` | MarkdownStreamCollector | Apache | std | Blocked; stable visible prefix, not canonical history |
| `tui/streaming/controller.rs` | StreamController | Apache | pane, rows, other streaming modules | Blocked; transcript pacing/cache seam |
| `tui/streaming/escapement.rs` | Escapement | MIT | std timing | Blocked; injected-time policy if consumer needs it |
| `tui/streaming/mod.rs` | module exports | MIT | streaming child modules; provenance documentation | Blocked; audit wiring |
| `tui/streaming/table_holdback.rs` | safe_commit_end, fence/list parsing | Apache | std | Blocked; Markdown boundary tests needed |
| `tui/text.rs` | ansi_spans, strip_ansi_for_text, ansi_spans_shaped | MIT | ansi-to-tui, ratatui; textengine | **Adapted** strip-before-wrap behavior for transcript strings while preserving stored line breaks; ansi span rendering remainder Blocked; p1-tui pure `text::sanitize_text`, consumed by transcript measurement/wrapping; #135 accepted |
| `tui/tool_render.rs` | ToolRenderer, contexts/outcomes | MIT | ratatui; nexus, tool_display/summary, delegation_dashboard | Blocked; reuse layout, reject tool-name dispatch |
| `tui/transcript.rs` | Transcript, TranscriptRender, caches | MIT | ratatui; metrics, nexus, UiEvent, streaming/render modules | Blocked; p1 event identity/retention preserved |
| `tui/wrap.rs` | styled wrap/truncate, clamps | MIT | ratatui, unicode-segmentation; hyperlink, textengine | **Adapted** bounded-progress styled wrapping and measurement without the donor runtime; other helpers Blocked; `crates/p1-tui/src/wrap.rs::{wrap_styled,wrap_len,wrap_paragraphs_len}`, consumed by transcript rendering/measurement; #94 accepted |
| `tui_loop.rs` | run, input/event phases, deferred commands | MIT | anyhow, ratatui-textarea, tokio; cli/config/git/goal/metrics/mimir/nexus/session/signals/tools/wayland | **Adapted** approval-key gating so unavailable session/project grants are ignored; remainder Blocked; `crates/p1-tui/src/input.rs::decide`, consumed by the p1-host TUI key handler; #122 accepted |
| `zwj_probe.rs` | CursorProbe, probe_shaping, run_startup_probe | MIT | terminal_env, textengine, tui; terminal I/O | Blocked; host-only capability probing |

## External reach still open

Outside `src/ui/`, direct references reach `approval`, `cli`, `config`, `errors`,
`git`, `goal`, `metrics`, `mimir`, `nexus`, `session`, `signals`, `telemetry`,
`tool_display`, `tool_summary`, `tools`, `wayland`, and root helpers. These are
**Blocked pending per-file/symbol traversal**, not dependencies approved for p1.
For #93's selected parser functions, the closed dependency set is std only;
no external donor module is needed.

## Closure rule

Before claiming #91 complete, expand all remaining symbols and external reach,
replace provisional destinations with real consumers, independently review all
exclusions/equivalence claims, and resolve blockers or obtain explicit scope narrowing.
The 53-row count alone is not acceptance.

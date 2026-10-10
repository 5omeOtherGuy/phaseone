# `p1 acp`

`p1 acp` serves p1 sessions as an [Agent Client Protocol](https://agentclientprotocol.com) agent: ACP v1, newline-delimited JSON-RPC 2.0 over stdin and stdout (ADR-0154, ADR-0156). Any ACP client can drive it, such as an editor, a terminal UI or a headless driver. What p1 sends and accepts is listed in [`p1-extensions.md`](p1-extensions.md). Recorded exchanges are in [`fixtures/`](fixtures/).

## Running it

```sh
p1 acp [--env NAME] [--model REF] [--effort LEVEL] [--workspace DIR] [--ask] [--sandbox MODE]
```

`p1 acp` takes the run options of `p1`. It refuses three things:

- `--tui` and a prompt, because the client is the front end and sends the prompts;
- `--session` and `--resume`, because every session is kept in memory (session history is #62).

- **stdout** carries JSON-RPC only. Every line the host would print goes to stderr, and so do the workers' activity lines (`[w1] read`).
- **Each session works in the folder the client names.** That is the `cwd` of `session/new`, which must be an existing absolute directory. p1's sandbox, access and approval rules apply to that folder exactly as with `p1 --workspace <cwd>`. A `session/new` without `cwd` takes `--workspace`; without `--workspace` it gets invalid params.
- **Several sessions per process.** Each `session/new` starts a session process: this binary with the same run options, `--workspace <cwd>` and the internal `--serve-session`. Each process has its own agent, cancel token, hold, approvals, workers and workflow runs. Its stderr is `p1 acp`'s stderr.
- **`session/close`** ends one session: its running prompt answers `cancelled`, its workflow runs and workers stop, and its process exits before the close answers `{}`.
- **EOF on stdin** closes every session the same way, then `p1 acp` exits.
- **Model and effort** are ACP config options (`model`, `thought_level`) on `session/new`. `session/set_config_option` runs the same switch as the line mode's `/model` and `/effort`; during a prompt it takes effect before the next turn. See [p1-extensions.md](p1-extensions.md#config-options).
- **Slash commands**: `/model`, `/effort`, `/compact`, `/status`, `/access`, `/modules reload` and one command per skill, published with `available_commands_update` after `session/new`. See [p1-extensions.md](p1-extensions.md#slash-commands).

A client launches the agent as a subprocess. Pass the whole command line as the client's agent command, for example `p1 acp --env deepseek`.

## Proof clients

These are the commands of the #673 and #690 proof runs. Both clients run in user space through `npx`; Node 22 is required.

Headless, with [acpx](https://github.com/openclaw/acpx) 0.19.4. `--approve-all` answers every permission request `allow_once`; the NDJSON transcript goes to stdout:

```sh
npx acpx@0.19.4 --agent 'p1 acp --env deepseek' --approve-all --format json \
  exec 'read README.md and say in two lines what p1 is'
```

acpx keeps named sessions per folder. It starts the agent for each prompt, so each run opens a new p1 session:

```sh
npx acpx@0.19.4 --agent 'p1 acp --env deepseek' sessions new --name one
npx acpx@0.19.4 --agent 'p1 acp --env deepseek' --approve-all --format json prompt -s one '…'
```

Terminal UI, with [Martty](https://github.com/openma-ai/Martty) 0.3.1:

```sh
DSH_TUI_AGENT='p1 acp --env deepseek' npx --yes martty@0.3.1
```

In Martty's approval dialog, Enter takes the highlighted choice (`allow_once` first). Esc cancels the dialog, which answers the request `cancelled`, and p1 denies the call. Esc outside a dialog interrupts the turn, which sends `session/cancel`.

## Conformance kit

The [ACP TCK](https://github.com/agentclientprotocol/acp-tck) is experimental. It runs locally only and is not part of CI. It needs `uv`, which fetches the Python it needs into its own cache:

```sh
git clone https://github.com/agentclientprotocol/acp-tck && cd acp-tck
uv run acp-tck --agent-cwd <scratch dir> --timeout 90 --test-timeout 300 \
  --report-json <path> -- p1 acp --env <env>
```

Do not pass `--workspace`: the TCK checks that a `session/new` without `cwd` gets invalid params. Its verdict for each slice is in that slice's pull request (#673, #690).

## Fixtures

Each file under `fixtures/` holds one exchange, one JSON object per line: `{"dir": "c2a" | "a2c", "msg": {...}}`. `c2a` is client to agent, and `a2c` is agent to client. Three values are normalised: the session id becomes `<session>`, the workspace path becomes `<workspace>`, and p1's version in `agentInfo` becomes `<version>`.

`crates/p1-host/tests/acp_fixtures.rs` replays a fixture against the real host on scripted providers, with no network:

1. It sends the `c2a` lines.
2. After each request it reads until that request is answered or the agent sends a request of its own.
3. It compares the whole transcript.

To re-record a fixture after a deliberate wire change, run:

```sh
P1_ACP_RECORD=1 cargo test -p p1-host --test acp_fixtures
```

That writes the transcript back. The `c2a` lines are the script, so change those first. Review the diff before you commit it: the fixture is the change detector for the published wire.

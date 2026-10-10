# `p1 acp`

`p1 acp` serves one p1 session as an [Agent Client Protocol](https://agentclientprotocol.com) agent: ACP v1, newline-delimited JSON-RPC 2.0 over stdin and stdout (ADR-0154). Any ACP client can drive it, such as an editor, a terminal UI or a headless driver. What p1 sends and accepts is listed in [`p1-extensions.md`](p1-extensions.md). Recorded exchanges are in [`fixtures/`](fixtures/).

## Running it

```sh
p1 acp [--env NAME] [--model REF] [--effort LEVEL] [--workspace DIR] [--ask] [--sandbox MODE]
```

`p1 acp` takes the run options of `p1`. It refuses `--tui` and a prompt, because the client is the front end and sends the prompts.

- **stdout** carries JSON-RPC only. Every line the host would print goes to stderr, and so do the workers' activity lines (`[w1] read`).
- **The workspace** is `--workspace`, or the directory `p1 acp` starts in. The client's `session/new` must name the same directory as `cwd`, or it gets invalid params naming both.
- **One session per process.** A second `session/new` is an error.
- **EOF on stdin** ends the session: the running prompt is cancelled with its workflow runs and workers, queued prompts are refused, and the process exits.

A client launches the agent as a subprocess. Pass the whole command line as the client's agent command, for example `p1 acp --env deepseek`.

## Proof clients

These are the commands of the #673 proof runs. Both clients run in user space through `npx`; Node 22 is required.

Headless, with [acpx](https://github.com/openclaw/acpx) 0.19.4. `--approve-all` answers every permission request `allow_once`; the NDJSON transcript goes to stdout:

```sh
npx acpx@0.19.4 --agent 'p1 acp --env deepseek' --approve-all --format json \
  exec 'read README.md and say in two lines what p1 is'
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
  --report-json <path> -- p1 acp --env <env> --workspace <scratch dir>
```

The TCK sends every `session/new` with a fresh temporary `cwd`, which p1's workspace rule refuses. It also opens two sessions on one connection. Its verdict for this slice, and why, is in the #673 pull request.

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

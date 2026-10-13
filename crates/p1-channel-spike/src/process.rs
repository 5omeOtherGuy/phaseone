//! E5: the `process` capability over the channel, as an adapter over the runtime's own
//! process service. No WIT: a started process is a number in JSON, its events come from
//! `call("process", "next", ...)`, and output bytes travel as a JSON array of numbers.
//!
//! - `spawn` `{"script", "timeout_ms"}` -> `{"process": <id>}`
//! - `next` `{"process": <id>}` -> `{"output": [<byte>, ...]}`, `{"exited": <status>}` or `null`
//! - `close` `{"process": <id>}` -> `null`, dropping (and so ending) the process
//!
//! A process the guest never closes is dropped with the call's Store, which ends its group
//! (the `RunningProcess` contract).

use std::sync::Arc;

use p1_contracts::CancellationToken;
use p1_module_runtime::{ExitStatus, ProcessCommand, ProcessEvent, ProcessService, RunningProcess};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{BoxFuture, Capability, Handles};

pub struct ProcessChannel(pub Arc<dyn ProcessService>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spawn {
    script: String,
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    process: u64,
}

impl Capability for ProcessChannel {
    fn call<'a>(
        &'a self,
        handles: &'a mut Handles,
        method: &'a str,
        payload: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match method {
                "spawn" => {
                    let spawn: Spawn = parse(payload)?;
                    let running = self
                        .0
                        .spawn(
                            ProcessCommand {
                                script: spawn.script,
                                timeout_ms: spawn.timeout_ms,
                            },
                            CancellationToken::new(),
                        )
                        .await?;
                    let id = handles.insert(Box::new(running));
                    Ok(json!({ "process": id }).to_string())
                }
                "next" => {
                    let Id { process } = parse(payload)?;
                    let running = handles
                        .get_mut::<Box<dyn RunningProcess>>(process)
                        .ok_or_else(|| format!("no process {process}"))?;
                    let event = match running.next().await {
                        Some(ProcessEvent::Output(bytes)) => json!({ "output": bytes }),
                        Some(ProcessEvent::Exited(status)) => json!({ "exited": exit(status) }),
                        None => Value::Null,
                    };
                    Ok(event.to_string())
                }
                "close" => {
                    let Id { process } = parse(payload)?;
                    handles.remove(process);
                    Ok("null".to_owned())
                }
                other => Err(format!("process has no method {other:?}")),
            }
        })
    }
}

fn parse<'a, T: Deserialize<'a>>(payload: &'a str) -> Result<T, String> {
    serde_json::from_str(payload).map_err(|error| format!("invalid payload: {error}"))
}

fn exit(status: ExitStatus) -> Value {
    match status {
        ExitStatus::Code(code) => json!({ "code": code }),
        ExitStatus::Signal(signal) => json!({ "signal": signal }),
        ExitStatus::UnknownSignal => json!("unknown_signal"),
        ExitStatus::TimedOut => json!("timed_out"),
        ExitStatus::Cancelled => json!("cancelled"),
    }
}

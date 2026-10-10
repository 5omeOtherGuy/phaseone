//! Targeted hooks reached over the actual byte stream, also during a parent turn.
mod common;

use common::{Client, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{FrontEndPort, SessionHandle};
use p1_contracts::{BoxFuture, CancellationToken, TurnEnd};
use p1_contracts::{Launched, SessionLauncher};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

struct Session(mpsc::UnboundedSender<String>);

struct Launcher(mpsc::UnboundedSender<String>);

impl SessionLauncher for Launcher {
    fn launch(&self, workspace: &std::path::Path) -> std::io::Result<Launched> {
        let (agent, router) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(agent);
        let (rr, rw) = tokio::io::split(router);
        let front = AcpFrontEnd::new(Box::new(ar), Box::new(aw), workspace.to_path_buf());
        let session = Session(self.0.clone());
        let served = tokio::spawn(async move { front.run(&session).await });
        Ok(Launched {
            reader: Box::new(rr),
            writer: Box::new(rw),
            exited: Box::pin(async move {
                assert_eq!(served.await.unwrap(), 0);
            }),
        })
    }
}

impl SessionHandle for Session {
    fn prompt<'a>(&'a self, _: String, cancel: CancellationToken) -> BoxFuture<'a, TurnEnd> {
        Box::pin(async move {
            self.0.send("prompt".into()).unwrap();
            cancel.cancelled().await;
            TurnEnd::Cancelled
        })
    }
    fn cancel_runs<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn stop_workers<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn cancel_run<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.send(format!("run {id}")).unwrap();
            id == "wf7"
        })
    }
    fn stop_worker<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.send(format!("worker {id}")).unwrap();
            id == "w9"
        })
    }
    fn drain_inbox<'a>(&'a self, _: CancellationToken) -> BoxFuture<'a, Option<TurnEnd>> {
        Box::pin(async { None })
    }
    fn inbox_ready<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}

async fn check(declaration: Value, run: bool, worker: bool, router: bool) {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(agent);
    let (ur, uw) = tokio::io::split(user);
    let (tx, mut hooks) = mpsc::unbounded_channel();
    let session = Session(tx.clone());
    let front;
    let served: BoxFuture<'_, i32> = if router {
        Box::pin(p1_acp::router::serve(
            Box::new(ar),
            Box::new(aw),
            std::sync::Arc::new(Launcher(tx)),
            None,
        ))
    } else {
        front = AcpFrontEnd::new(Box::new(ar), Box::new(aw), workspace());
        front.run(&session)
    };
    let mut client = Client {
        lines: BufReader::new(ur).lines(),
        writer: uw,
        transcript: Vec::new(),
    };
    let script = async {
        client
            .request(
                0,
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":declaration}),
            )
            .await;
        let (_, init) = client.until_response(0).await;
        if run || worker {
            let mut expected = Vec::new();
            if run {
                expected.push("run_cancel");
            }
            if worker {
                expected.push("worker_stop");
            }
            assert_eq!(
                init["result"]["agentCapabilities"]["_meta"]["p1.dev"]["extensions"],
                json!(expected)
            );
        }
        client
            .request(1, "session/new", json!({"cwd":workspace()}))
            .await;
        let (_, opened) = client.until_response(1).await;
        let id = opened["result"]["sessionId"].clone();
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"stay running"}]}),
            )
            .await;
        assert_eq!(hooks.recv().await.unwrap(), "prompt");
        for (n, method, key, target, enabled, result, hook) in [
            (3, "_p1/run/cancel", "runId", "wf7", run, true, "run wf7"),
            (
                4,
                "_p1/worker/stop",
                "workerId",
                "w9",
                worker,
                true,
                "worker w9",
            ),
            (5, "_p1/run/cancel", "runId", "w9", run, false, "run w9"),
            (
                6,
                "_p1/worker/stop",
                "workerId",
                "wf7",
                worker,
                false,
                "worker wf7",
            ),
        ] {
            client
                .request(n, method, json!({"sessionId":id,key:target}))
                .await;
            let (before, answer) = client.until_response(n).await;
            assert!(
                !before.iter().any(|m| m["id"] == 2),
                "targeted cancel ended the parent"
            );
            if enabled {
                assert_eq!(
                    answer["result"],
                    if method == "_p1/run/cancel" {
                        json!({"cancelled":result})
                    } else {
                        json!({"stopped":result})
                    }
                );
                assert_eq!(hooks.recv().await.unwrap(), hook);
            } else {
                assert_eq!(answer["error"]["code"], -32601);
                assert!(hooks.try_recv().is_err());
            }
        }
        client
            .request(7, "_p1/run/cancel", json!({"sessionId":id,"runId":5}))
            .await;
        let (_, invalid) = client.until_response(7).await;
        assert_eq!(invalid["error"]["code"], if run { -32602 } else { -32601 });
        assert!(hooks.try_recv().is_err());
        client
            .request(
                8,
                "_p1/run/cancel",
                json!({"sessionId":"unknown-session","runId":"wf7"}),
            )
            .await;
        let (_, unknown_session) = client.until_response(8).await;
        assert_eq!(
            unknown_session["error"]["code"],
            if run { -32602 } else { -32601 }
        );
        assert!(hooks.try_recv().is_err());
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let (_, cancelled) = client.until_response(2).await;
        assert_eq!(cancelled["result"]["stopReason"], "cancelled");
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(served, script)
    })
    .await
    .expect("driver hung");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn targeted_cancel_hooks_are_independently_negotiated() {
    for router in [false, true] {
        for (names, run, worker) in [
            (vec!["run_cancel", "worker_stop", "run_cancel"], true, true),
            (vec!["run_cancel"], true, false),
            (vec!["worker_stop"], false, true),
            (vec!["workflow_update"], false, false),
            (vec![], false, false),
        ] {
            check(
                json!({"_meta":{"p1.dev":{"version":1,"capabilities":names}}}),
                run,
                worker,
                router,
            )
            .await;
        }
        check(json!({}), false, false, router).await;
        check(
            json!({"_meta":{"p1.dev":{"version":2,"capabilities":["run_cancel","worker_stop"]}}}),
            false,
            false,
            router,
        )
        .await;
        check(
            json!({"_meta":{"p1.dev":{"version":1,"capabilities":["run_cancel",3]}}}),
            false,
            false,
            router,
        )
        .await;
    }
}

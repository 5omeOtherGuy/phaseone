use p1_acp::driver::io::{self as rpc, Handler, MAX_MESSAGE_BYTES, Peer, RpcError};
use p1_contracts::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter, DuplexStream, ReadHalf,
    WriteHalf,
};
use tokio::sync::mpsc;

type ClientRead = BufReader<ReadHalf<DuplexStream>>;
type ClientWrite = WriteHalf<DuplexStream>;

struct Echo(mpsc::UnboundedSender<(String, Value)>);

impl Handler for Echo {
    fn request(
        &self,
        peer: Peer,
        method: String,
        params: Value,
    ) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match method.as_str() {
                "echo" => Ok(params),
                "reverse_call" => peer.request("echo", params).await,
                "fail" => Err(RpcError {
                    code: -32042,
                    message: "fixture refused".into(),
                    data: Some(params),
                }),
                _ => Err(RpcError::method_not_found()),
            }
        })
    }

    fn notification(&self, _peer: Peer, method: String, params: Value) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if method == "notice" || method == "$/cancel_request" {
                self.0.send((method, params)).unwrap();
            }
        })
    }
}

fn echo() -> (Arc<dyn Handler>, mpsc::UnboundedReceiver<(String, Value)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(Echo(tx)), rx)
}

fn raw(
    handler: Arc<dyn Handler>,
) -> (
    Peer,
    tokio::task::JoinHandle<std::io::Result<()>>,
    ClientRead,
    ClientWrite,
) {
    let (server, client) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(server);
    // Receiving a short reply before closing proves per-message flush,
    // not merely that duplex's unbuffered writes happen to be visible.
    let (peer, task) = rpc::spawn(read, BufWriter::new(write), handler);
    let (read, write) = tokio::io::split(client);
    (peer, task, BufReader::new(read), write)
}

async fn send(write: &mut ClientWrite, message: Value) {
    let mut line = serde_json::to_vec(&message).unwrap();
    line.push(b'\n');
    write.write_all(&line).await.unwrap();
    write.flush().await.unwrap();
}

async fn receive(read: &mut ClientRead) -> Value {
    let mut line = String::new();
    assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
    assert!(line.ends_with('\n'));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["jsonrpc"], "2.0");
    assert!(value.is_object());
    value
}

#[tokio::test]
async fn io_request_response_and_notifications_both_ways() {
    let (left, right) = tokio::io::duplex(128);
    let (left_handler, mut left_notices) = echo();
    let (right_handler, mut right_notices) = echo();
    let (read, write) = tokio::io::split(left);
    let (a, a_task) = rpc::spawn(read, write, left_handler);
    let (read, write) = tokio::io::split(right);
    let (b, b_task) = rpc::spawn(read, write, right_handler);
    assert_eq!(
        a.request("echo", json!({"a":7,"b":19})).await.unwrap(),
        json!({"a":7,"b":19})
    );
    assert_eq!(
        b.request("echo", json!(["reverse", 3])).await.unwrap(),
        json!(["reverse", 3])
    );
    a.notify("notice", json!({"from":"a"})).unwrap();
    assert_eq!(
        right_notices.recv().await.unwrap(),
        ("notice".into(), json!({"from":"a"}))
    );
    b.notify("notice", json!({"from":"b"})).unwrap();
    assert_eq!(
        left_notices.recv().await.unwrap(),
        ("notice".into(), json!({"from":"b"}))
    );
    a.notify("$/cancel_request", json!({"id":17})).unwrap();
    assert_eq!(
        right_notices.recv().await.unwrap(),
        ("$/cancel_request".into(), json!({"id":17}))
    );
    let error = a
        .request("fail", json!({"reason":"fixture"}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        RpcError {
            code: -32042,
            message: "fixture refused".into(),
            data: Some(json!({"reason":"fixture"}))
        }
    );
    assert_eq!(
        a.request("reverse_call", json!([42])).await.unwrap(),
        json!([42])
    );
    a.close();
    a_task.await.unwrap().unwrap();
    b_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_concurrent_requests_match_ids_not_response_order() {
    let (peer, task, mut read, mut write) = raw(Arc::new(()));
    let first = peer.request("first", json!({"value":11}));
    let second = peer.request("second", json!({"value":29}));
    let one = receive(&mut read).await;
    let two = receive(&mut read).await;
    assert_eq!(one["method"], "first");
    assert_eq!(two["method"], "second");
    assert_ne!(one["id"], two["id"]);
    // A string with the same digits must not match a numeric request id.
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":one["id"].to_string(),"result":"wrong"}),
    )
    .await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":two["id"],"result":29,"extra":true}),
    )
    .await;
    assert_eq!(second.await.unwrap(), 29);
    // A second producer remains usable while first is still awaiting its reply.
    peer.clone()
        .notify("notice", json!(["outside request"]))
        .unwrap();
    assert_eq!(
        receive(&mut read).await,
        json!({"jsonrpc":"2.0","method":"notice","params":["outside request"]})
    );
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":one["id"],"result":11}),
    )
    .await;
    assert_eq!(first.await.unwrap(), 11);
    peer.close();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_parse_and_invalid_request_errors_recover_at_next_line() {
    let (handler, _notices) = echo();
    let (peer, task, mut read, mut write) = raw(handler);
    write.write_all(b"{broken}\n").await.unwrap();
    assert_eq!(
        receive(&mut read).await,
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
    );
    for invalid in [
        json!({"jsonrpc":"1.0","method":"echo","id":1}),
        json!({"jsonrpc":"2.0","method":3}),
        json!({"jsonrpc":"2.0","method":"echo","id":true}),
        json!({"jsonrpc":"2.0","method":"echo","params":false}),
        json!({"jsonrpc":"2.0","id":1,"result":7,"error":{"code":-32000,"message":"bad"}}),
        json!([]),
        json!(null),
        json!({}),
    ] {
        send(&mut write, invalid).await;
        assert_eq!(
            receive(&mut read).await,
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}})
        );
    }
    for id in [json!("client-id"), json!(null), json!(1.5)] {
        send(&mut write, json!({"jsonrpc":"2.0","id":id,"method":"echo","params":[13],"unknown":{"anything":true}})).await;
        assert_eq!(
            receive(&mut read).await,
            json!({"jsonrpc":"2.0","id":id,"result":[13]})
        );
    }
    peer.close();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_method_not_found_and_unknown_notification_ignored() {
    let (peer, task, mut read, mut write) = raw(Arc::new(()));
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"unknown_notice"}),
    )
    .await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"missing","method":"unknown_request"}),
    )
    .await;
    assert_eq!(
        receive(&mut read).await,
        json!({"jsonrpc":"2.0","id":"missing","error":{"code":-32601,"message":"Method not found"}})
    );
    peer.close();
    task.await.unwrap().unwrap();
    let mut remaining = String::new();
    assert_eq!(read.read_line(&mut remaining).await.unwrap(), 0);
}

#[tokio::test]
async fn io_close_fails_all_pending_and_future_sends() {
    let (peer, task, mut read, write) = raw(Arc::new(()));
    let first = peer.request("one", Value::Null);
    let second = peer.request("two", Value::Null);
    receive(&mut read).await;
    receive(&mut read).await;
    drop(read);
    drop(write);
    for error in [
        first.await.unwrap_err(),
        second.await.unwrap_err(),
        peer.request("after", Value::Null).await.unwrap_err(),
        peer.notify("after", Value::Null).unwrap_err(),
    ] {
        assert_eq!(error.code, -32000);
        assert_eq!(error.message, "connection closed");
    }
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_writer_serializes_producers_and_preserves_each_send_order() {
    let (peer, task, mut read, _write) = raw(Arc::new(()));
    let mut producers = Vec::new();
    for producer in 0..3 {
        let peer = peer.clone();
        producers.push(tokio::spawn(async move {
            for sequence in 0..20 {
                peer.notify(
                    "notice",
                    json!({"producer":producer,"sequence":sequence,"text":"line one\nline two"}),
                )
                .unwrap();
            }
        }));
    }
    let mut next = [0; 3];
    for _ in 0..60 {
        let message = receive(&mut read).await;
        assert_eq!(message["method"], "notice");
        assert!(message.get("id").is_none());
        let producer = message["params"]["producer"].as_u64().unwrap() as usize;
        assert_eq!(message["params"]["sequence"], next[producer]);
        assert_eq!(message["params"]["text"], "line one\nline two");
        next[producer] += 1;
    }
    assert_eq!(next, [20; 3]);
    for producer in producers {
        producer.await.unwrap();
    }
    peer.close();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_line_limit_accepts_boundary_rejects_next_byte_and_recovers() {
    let (handler, _notices) = echo();
    let (peer, task, mut read, mut write) = raw(handler);
    let message = serde_json::to_vec(
        &json!({"jsonrpc":"2.0","id":"large","method":"echo","params":["fixture"]}),
    )
    .unwrap();
    let mut at_limit = message;
    at_limit.resize(MAX_MESSAGE_BYTES, b' ');
    at_limit.push(b'\n');
    write.write_all(&at_limit).await.unwrap();
    assert_eq!(
        receive(&mut read).await,
        json!({"jsonrpc":"2.0","id":"large","result":["fixture"]})
    );
    at_limit.pop();
    at_limit.extend_from_slice(b" \n");
    write.write_all(&at_limit).await.unwrap();
    let error = receive(&mut read).await;
    assert_eq!(error["id"], Value::Null);
    assert_eq!(error["error"]["code"], -32600);
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":9,"method":"echo","params":["after oversized"]}),
    )
    .await;
    assert_eq!(
        receive(&mut read).await,
        json!({"jsonrpc":"2.0","id":9,"result":["after oversized"]})
    );
    peer.close();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_eof_flushes_final_unterminated_line_error() {
    let (_peer, task, mut read, mut write) = raw(Arc::new(()));
    write.write_all(b"not json").await.unwrap();
    write.shutdown().await.unwrap();
    assert_eq!(receive(&mut read).await["error"]["code"], -32700);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_eof_delivers_final_calls_and_fails_outbound_waits_before_draining() {
    for newline in [false, true] {
        for method in ["echo", "reverse_call"] {
            let (handler, mut notices) = echo();
            let (peer, task, mut read, mut write) = raw(handler);
            let pending = peer.request("unanswered", Value::Null);
            receive(&mut read).await;
            send(
                &mut write,
                json!({"jsonrpc":"2.0","method":"notice","params":["final"]}),
            )
            .await;
            let mut final_call = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":"final","method":method,"params":[17]}),
            )
            .unwrap();
            if newline {
                final_call.push(b'\n');
            }
            write.write_all(&final_call).await.unwrap();
            write.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
            assert_eq!(pending.await.unwrap_err().message, "connection closed");
            assert_eq!(
                notices.try_recv().unwrap(),
                ("notice".into(), json!(["final"]))
            );
            let expected = if method == "echo" {
                json!({"jsonrpc":"2.0","id":"final","result":[17]})
            } else {
                json!({"jsonrpc":"2.0","id":"final","error":{"code":-32000,"message":"connection closed"}})
            };
            assert_eq!(receive(&mut read).await, expected);
        }
    }
}

#[tokio::test]
async fn io_explicit_close_stops_dispatch_even_when_output_is_blocked() {
    let (handler, mut notices) = echo();
    let (peer, task, mut read, mut write) = raw(handler);
    peer.notify("backpressured", json!(["x".repeat(20000)]))
        .unwrap();
    read.read_exact(&mut [0]).await.unwrap();
    peer.close();
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"notice","params":["after close"]}),
    )
    .await;
    write.shutdown().await.unwrap();
    assert!(notices.recv().await.is_none());
    let mut queued = Vec::new();
    read.read_to_end(&mut queued).await.unwrap();
    assert!(queued.len() > 20000);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn io_aborting_connection_closes_pending_before_or_after_start() {
    for started in [false, true] {
        let (peer, task, mut read, _write) = raw(Arc::new(()));
        if started {
            peer.notify("backpressured", json!(["x".repeat(20000)]))
                .unwrap();
        }
        let pending = peer.request("waiting", Value::Null);
        if started {
            // First byte proves writer started; remaining output exceeds the
            // duplex capacity, so abort must stop a blocked write as well.
            read.read_exact(&mut [0]).await.unwrap();
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(pending.await.unwrap_err().message, "connection closed");
        let mut remaining = Vec::new();
        read.read_to_end(&mut remaining).await.unwrap();
        assert!(remaining.len() < 20000);
    }
}

//! The prompt loop drops a pending line read whenever the agent must run an inbox
//! turn (review finding R3). What the user had typed by then must not be lost.

use std::task::{Context, Poll};

use p1_host::{LineSource, ReaderLines};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn a_dropped_read_keeps_the_half_typed_line() {
    let (mut typist, terminal) = tokio::io::duplex(64);
    let lines = ReaderLines::from_reader(terminal);

    typist.write_all(b"hel").await.unwrap();
    {
        let mut pending = lines.next_line();
        let mut context = Context::from_waker(std::task::Waker::noop());
        // Polled once: it has consumed "hel" and is waiting for the newline.
        assert!(matches!(pending.as_mut().poll(&mut context), Poll::Pending));
    } // dropped here, as the prompt loop does when the inbox wakes it

    typist.write_all(b"lo\r\nnext\n").await.unwrap();
    assert_eq!(lines.next_line().await.as_deref(), Some("hello"));
    assert_eq!(lines.next_line().await.as_deref(), Some("next"));

    // A last line without a newline still arrives; then EOF.
    typist.write_all(b"tail").await.unwrap();
    drop(typist);
    assert_eq!(lines.next_line().await.as_deref(), Some("tail"));
    assert_eq!(lines.next_line().await, None);
}

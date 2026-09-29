//! One cancellation race shared by the HTTP and WebSocket drivers.

use std::future::Future;

use futures_util::future::{Either, select};
use p1_contracts::CancellationToken;

pub(crate) enum Raced<T> {
    Done(T),
    Cancelled,
}

pub(crate) async fn race<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = T>,
) -> Raced<T> {
    let future = std::pin::pin!(future);
    let cancelled = std::pin::pin!(cancel.cancelled());
    match select(future, cancelled).await {
        Either::Left((value, _)) => Raced::Done(value),
        Either::Right(((), _)) => Raced::Cancelled,
    }
}

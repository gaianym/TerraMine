use futures::stream::{FuturesUnordered, StreamExt};
use futures::Future;

pub async fn drain_in_flight<F>(in_flight: &mut FuturesUnordered<F>)
where
    F: Future + Unpin,
{
    while in_flight.next().await.is_some() {}
}

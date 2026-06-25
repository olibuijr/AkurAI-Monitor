use std::convert::Infallible;
use std::sync::OnceLock;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

#[derive(Clone)]
pub struct Msg {
    pub event: &'static str,
    pub data: String,
}

static TX: OnceLock<broadcast::Sender<Msg>> = OnceLock::new();

/// Initialize the live broadcast channel. Call once at startup.
pub fn init() {
    let (tx, _rx) = broadcast::channel(64);
    let _ = TX.set(tx);
}

/// Publish a live event to all connected SSE subscribers. No-op if there are none.
pub fn publish(event: &'static str, data: String) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg { event, data });
    }
}

/// GET /api/stream — Server-Sent Events of live status + log updates.
pub async fn sse_handler() -> impl IntoResponse {
    let rx = TX
        .get()
        .expect("stream channel not initialized")
        .subscribe();

    // Drop lagged messages (slow client) instead of erroring the stream.
    let stream = BroadcastStream::new(rx).filter_map(|res| {
        res.ok()
            .map(|msg| Ok::<Event, Infallible>(Event::default().event(msg.event).data(msg.data)))
    });

    (
        // Tell nginx not to buffer this response, so events flush immediately.
        [("X-Accel-Buffering", "no")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

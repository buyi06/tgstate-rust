use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use axum::Router;
use tokio_stream::Stream;

use crate::state::AppState;

async fn file_updates(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.event_bus.subscribe();

    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.tick().await; // Skip first immediate tick

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(data) => {
                            yield Ok(Event::default().data(data));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    yield Ok(Event::default().comment("keepalive"));
                }
            }
        }
    };

    Sse::new(stream)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/file-updates", get(file_updates))
}

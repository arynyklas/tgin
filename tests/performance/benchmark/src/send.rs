//! Hands updates to the system under test: a webhook POST or a long-poll
//! stream on the fake Bot API.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::Bytes;
use reqwest::{header::CONTENT_TYPE, Client, RequestBuilder};
use serde_json::json;
use tokio::{sync::Semaphore, time::Instant};

use crate::{
    mock::UpdateStream,
    track::{Failure, Tracker, WARMUP_TEXT},
};

/// A Telegram `Update` carrying a text message; bots echo `text` back.
pub fn update_payload(update_id: u64, text: &str) -> Bytes {
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let update = json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id,
            "date": date,
            "chat": { "id": 1, "type": "private" },
            "from": { "id": 1, "is_bot": false, "first_name": "Bench" },
            "text": text
        }
    });
    Bytes::from(serde_json::to_vec(&update).expect("a JSON value serializes"))
}

pub enum Sender {
    /// POST each update to a webhook URL, at most `budget` in flight. Sends
    /// that find the budget spent are dropped and counted, never queued, so
    /// the schedule does not wait for responses.
    Webhook {
        client: Client,
        target: Arc<str>,
        budget: Arc<Semaphore>,
        tracker: Arc<Tracker>,
    },
    /// Append each update to the stream a long-poll consumer reads.
    LongPoll {
        stream: Arc<UpdateStream>,
        tracker: Arc<Tracker>,
    },
}

impl Sender {
    pub fn webhook(
        client: Client,
        target: &str,
        max_in_flight: usize,
        tracker: Arc<Tracker>,
    ) -> Self {
        Self::Webhook {
            client,
            target: target.into(),
            budget: Arc::new(Semaphore::new(max_in_flight.max(1))),
            tracker,
        }
    }

    /// Send measured update `update_id`, due at `due`. Its reply text is the
    /// id, which is how the reply finds its scheduled time.
    pub fn send(&self, update_id: u64, due: Instant) {
        let payload = || update_payload(update_id, &update_id.to_string());
        match self {
            Self::Webhook {
                client,
                target,
                budget,
                tracker,
            } => {
                let Ok(permit) = budget.clone().try_acquire_owned() else {
                    tracker.dropped();
                    return;
                };
                tracker.issued(update_id, due);
                let request = post_json(client, target, payload());
                let tracker = tracker.clone();
                tokio::spawn(async move {
                    let delivered = deliver(request).await;
                    drop(permit);
                    if let Err((failure, _)) = delivered {
                        tracker.failed(update_id, failure);
                    }
                });
            }
            Self::LongPoll { stream, tracker } => {
                tracker.issued(update_id, due);
                stream.push(update_id as i64, payload());
            }
        }
    }

    /// Send a warm-up update; its reply only proves the path is live.
    pub fn send_warmup(&self, update_id: u64) {
        let payload = update_payload(update_id, WARMUP_TEXT);
        match self {
            Self::Webhook {
                client,
                target,
                tracker,
                ..
            } => {
                tracker.warmup_issued();
                let request = post_json(client, target, payload);
                tokio::spawn(async move {
                    if let Err((_, reason)) = deliver(request).await {
                        eprintln!("warm-up update {update_id} failed: {reason}");
                    }
                });
            }
            Self::LongPoll { stream, tracker } => {
                tracker.warmup_issued();
                stream.push(update_id as i64, payload);
            }
        }
    }
}

fn post_json(client: &Client, target: &str, payload: Bytes) -> RequestBuilder {
    client
        .post(target)
        .header(CONTENT_TYPE, "application/json")
        .body(payload)
}

/// POST an update; a 2xx answer means the system under test accepted it.
async fn deliver(request: RequestBuilder) -> Result<(), (Failure, String)> {
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            // Reading the body returns the connection to the pool.
            let _ = response.bytes().await;
            if status.is_success() {
                Ok(())
            } else {
                Err((Failure::Status, format!("HTTP {status}")))
            }
        }
        Err(error) => Err((Failure::Transport, error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::Duration;

    #[test]
    fn payload_is_a_message_update_echoing_text() {
        let payload: Value = serde_json::from_slice(&update_payload(42, "42")).unwrap();

        assert_eq!(payload["update_id"], 42);
        assert_eq!(payload["message"]["text"], "42");
        assert_eq!(payload["message"]["chat"]["id"], 1);
    }

    #[tokio::test]
    async fn webhook_drops_sends_once_the_in_flight_budget_is_spent() {
        // A server that accepts connections and never answers keeps every
        // issued send in flight.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let tracker = Arc::new(Tracker::new());
        let sender = Sender::webhook(
            Client::new(),
            &format!("http://{addr}/webhook"),
            2,
            tracker.clone(),
        );

        let started = Instant::now();
        for id in 1..=5 {
            sender.send(id, Instant::now());
        }

        assert!(started.elapsed() < Duration::from_secs(1), "send blocked");
        let counts = tracker.counts();
        assert_eq!(counts.sent, 2);
        assert_eq!(counts.dropped_by_generator, 3);
    }

    #[tokio::test]
    async fn webhook_counts_error_statuses_as_failed() {
        let app = axum::Router::new().route(
            "/webhook",
            axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let tracker = Arc::new(Tracker::new());
        let sender = Sender::webhook(
            Client::new(),
            &format!("http://{addr}/webhook"),
            10,
            tracker.clone(),
        );

        sender.send(1, Instant::now());
        let settled = crate::load::wait_until(Duration::from_secs(5), || tracker.settled()).await;

        assert!(settled < Duration::from_secs(5));
        let counts = tracker.counts();
        assert_eq!(
            (counts.sent, counts.http_errors, counts.unreceived()),
            (1, 1, 1)
        );
    }

    #[tokio::test]
    async fn long_poll_appends_to_the_stream() {
        let tracker = Arc::new(Tracker::new());
        let stream = Arc::new(UpdateStream::new());
        let sender = Sender::LongPoll {
            stream: stream.clone(),
            tracker: tracker.clone(),
        };

        sender.send(5, Instant::now());
        sender.send_warmup(6);

        assert_eq!(stream.update_ids(), vec![5, 6]);
        assert_eq!(tracker.counts().sent, 1);
        assert_eq!(tracker.counts().warmup_sent, 1);
    }
}

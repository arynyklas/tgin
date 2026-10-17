//! Webhook mode: POST each queued update to the bot's webhook, and run a
//! method named in a 2xx response like Telegram does (`WebhookActor.cpp`).

use std::{pin::pin, sync::Arc, time::Duration};

use axum::http::header::{CONTENT_TYPE, TRANSFER_ENCODING};
use bytes::Bytes;

use crate::{
    api::{self, Call},
    journal::{Entry, Kind, ReplyOutcome, Via, unix_seconds},
    params::Params,
    state::{Bot, Shared},
};

const SECRET_HEADER: &str = "X-Telegram-Bot-Api-Secret-Token";
/// Retry delays after a failed delivery: 1 s, doubling up to this.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);

struct Job {
    url: String,
    secret_token: Option<String>,
    update_id: i64,
    payload: Bytes,
}

/// What came back from one delivery.
struct Delivered {
    status: u16,
    reason: &'static str,
    content_type: Option<String>,
    transfer_encoding: Option<String>,
    body: Bytes,
}

/// The delivery loop of one bot; idle while no webhook is set.
pub(crate) async fn deliver(shared: Arc<Shared>, bot: Arc<Bot>) {
    let mut failures: u32 = 0;
    loop {
        let job = loop {
            let mut notified = pin!(bot.changed.notified());
            notified.as_mut().enable();
            if let Some(job) = next_job(&bot) {
                break job;
            }
            tokio::select! {
                () = notified => {}
                () = shared.cancel.cancelled() => return,
            }
        };
        let succeeded = match post(&shared, &job).await {
            Ok(delivered) if (200..300).contains(&delivered.status) => {
                failures = 0;
                delivered_ok(&shared, &bot, &job, delivered).await;
                true
            }
            Ok(delivered) => {
                let message = format!(
                    "Wrong response from the webhook: {} {}",
                    delivered.status, delivered.reason
                );
                let mut entry = delivery_entry(&bot, &job, &delivered);
                entry.status = Some(delivered.status);
                shared.journal.record(entry);
                bot.state.lock().last_error = Some((unix_seconds(), message));
                false
            }
            Err(error) => {
                let mut entry = Entry::new(&bot.token, bot.id, Kind::Webhook, "POST");
                entry.update_id = Some(job.update_id);
                entry.error = Some(error.clone());
                shared.journal.record(entry);
                bot.state.lock().last_error = Some((unix_seconds(), error));
                false
            }
        };
        if !succeeded {
            let delay = Duration::from_secs(1u64 << failures.min(3)).min(MAX_RETRY_DELAY);
            failures = failures.saturating_add(1);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shared.cancel.cancelled() => return,
            }
        }
    }
}

/// The oldest pending update, while a webhook is set.
fn next_job(bot: &Bot) -> Option<Job> {
    let state = bot.state.lock();
    let webhook = state.webhook.as_ref()?;
    let update = state.queue.front()?;
    Some(Job {
        url: webhook.url.clone(),
        secret_token: webhook.secret_token.clone(),
        update_id: update.update_id,
        payload: update.payload.clone(),
    })
}

async fn post(shared: &Shared, job: &Job) -> Result<Delivered, String> {
    let mut request = shared
        .client
        .post(&job.url)
        .header(CONTENT_TYPE, "application/json")
        .body(job.payload.clone());
    if let Some(secret) = &job.secret_token {
        request = request.header(SECRET_HEADER, secret);
    }
    let response = request.send().await.map_err(describe)?;
    let status = response.status();
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = header(CONTENT_TYPE);
    let transfer_encoding = header(TRANSFER_ENCODING);
    let body = response.bytes().await.map_err(describe)?;
    Ok(Delivered {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or(""),
        content_type,
        transfer_encoding,
        body,
    })
}

/// A delivery failure without the URL.
fn describe(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Timeout expired".to_owned()
    } else if error.is_connect() {
        "Connection failed".to_owned()
    } else {
        "Failed to read the webhook response".to_owned()
    }
}

fn delivery_entry(bot: &Bot, job: &Job, delivered: &Delivered) -> Entry {
    let mut entry = Entry::new(&bot.token, bot.id, Kind::Webhook, "POST");
    entry.update_id = Some(job.update_id);
    entry.content_type.clone_from(&delivered.content_type);
    entry
        .transfer_encoding
        .clone_from(&delivered.transfer_encoding);
    entry
}

/// Telegram ignores a reply naming no method, `getUpdates`-style `get*`
/// methods, and the webhook and session methods.
fn refused(method: &str) -> bool {
    let method = method.to_ascii_lowercase();
    method.starts_with("get")
        || matches!(
            method.as_str(),
            "setwebhook" | "deletewebhook" | "close" | "logout"
        )
}

async fn delivered_ok(shared: &Arc<Shared>, bot: &Arc<Bot>, job: &Job, delivered: Delivered) {
    {
        let mut state = bot.state.lock();
        if let Some(index) = state
            .queue
            .iter()
            .position(|u| u.update_id == job.update_id)
        {
            state.queue.remove(index);
        }
        state.last_error = None;
    }

    // The response is read like a request (`HttpReader`); a body that does
    // not parse yields no arguments, hence no method.
    let mut params = Params::default();
    let content_type = delivered.content_type.clone().unwrap_or_default();
    if params
        .add_body(&content_type, delivered.body.clone())
        .await
        .is_err()
    {
        params = Params::default();
    }
    let method = params
        .get("method")
        .filter(|method| !method.is_empty())
        .map(str::to_owned);
    let outcome = method.as_deref().map(|method| {
        if refused(method) {
            ReplyOutcome::Refused
        } else {
            ReplyOutcome::Executed
        }
    });

    let mut entry = delivery_entry(bot, job, &delivered).with_params(&params);
    entry.status = Some(delivered.status);
    entry.reply_method.clone_from(&method);
    entry.reply = outcome;
    shared.journal.record(entry);

    if let (Some(method), Some(ReplyOutcome::Executed)) = (method, outcome) {
        let call = Call {
            method,
            http_method: "POST".to_owned(),
            content_type: delivered.content_type,
            transfer_encoding: delivered.transfer_encoding,
            params,
            via: Via::WebhookReply,
        };
        // Like Telegram, the result goes nowhere.
        api::execute(shared, bot, call).await;
    }
}

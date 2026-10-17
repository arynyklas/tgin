//! `/bot{token}/{method}` and `/file/bot{token}/{path}`, answered the way the
//! official server answers them.

use std::{pin::pin, sync::Arc, time::Duration};

use axum::{
    body::to_bytes,
    extract::Request,
    http::{
        HeaderMap, StatusCode,
        header::{CONTENT_TYPE, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
    journal::{Entry, Kind, Via, unix_seconds},
    params::{FilePart, Params},
    state::{Bot, BotState, Fault, Shared, StoredFile, Webhook, bot_id},
};

/// Largest request body read (Telegram's upload limit is 50 MB).
const BODY_LIMIT: usize = 64 << 20;
/// `getUpdates` `timeout` cap, seconds.
const LONG_POLL_MAX: i64 = 50;
/// `getUpdates` `limit` cap.
const LIMIT_MAX: i64 = 100;

const INVALID_TOKEN: &str = "Unauthorized: invalid token specified";
const METHOD_NOT_FOUND: &str = "Not Found: method not found";
const WEBHOOK_ACTIVE: &str = "Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first";
const TERMINATED_BY_POLL: &str = "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running";
const TERMINATED_BY_SET_WEBHOOK: &str = "Conflict: terminated by setWebhook request";

/// A Bot API response: status and JSON body.
pub(crate) struct Answer {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Answer {
    fn ok(result: &Value) -> Self {
        Self::success(format!(r#"{{"ok":true,"result":{result}}}"#))
    }

    fn ok_described(result: &Value, description: &str) -> Self {
        let description = Value::from(description);
        Self::success(format!(
            r#"{{"ok":true,"result":{result},"description":{description}}}"#
        ))
    }

    fn success(body: String) -> Self {
        Self {
            status: StatusCode::OK,
            body: Bytes::from(body),
        }
    }

    /// The server's key order (`Query.cpp`, `JsonQueryError::store`): `ok`,
    /// `error_code`, `description`, then `parameters` when set.
    pub fn error(code: u16, description: &str, retry_after: Option<u64>) -> Self {
        let description = Value::from(description);
        let mut body = format!(r#"{{"ok":false,"error_code":{code},"description":{description}"#);
        if let Some(seconds) = retry_after {
            body.push_str(&format!(r#","parameters":{{"retry_after":{seconds}}}"#));
        }
        body.push('}');
        Self {
            status: StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body: Bytes::from(body),
        }
    }

    fn bad_request(description: &str) -> Self {
        Self::error(400, description, None)
    }

    /// `{"ok":true,"result":[…]}` around stored updates, which are not
    /// parsed again.
    fn batch<'a>(updates: impl Iterator<Item = &'a Bytes>) -> Self {
        let mut body = b"{\"ok\":true,\"result\":[".to_vec();
        for (index, update) in updates.enumerate() {
            if index > 0 {
                body.push(b',');
            }
            body.extend_from_slice(update);
        }
        body.extend_from_slice(b"]}");
        Self {
            status: StatusCode::OK,
            body: Bytes::from(body),
        }
    }
}

impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        (self.status, [(CONTENT_TYPE, "application/json")], self.body).into_response()
    }
}

/// One method call, from HTTP or from a webhook response.
pub(crate) struct Call {
    pub method: String,
    pub http_method: String,
    pub content_type: Option<String>,
    pub transfer_encoding: Option<String>,
    pub params: Params,
    pub via: Via,
}

impl Call {
    fn entry(&self, bot: &Bot) -> Entry {
        let mut entry =
            Entry::new(&bot.token, bot.id, Kind::Call, &self.http_method).with_params(&self.params);
        entry.method.clone_from(&self.method);
        entry.via = Some(self.via);
        entry.content_type.clone_from(&self.content_type);
        entry.transfer_encoding.clone_from(&self.transfer_encoding);
        entry
    }
}

fn header(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// A percent-decoded token and its bot id; `None` for an invalid token.
fn decode_token(raw: &str) -> Option<(String, i64)> {
    let token = percent_decode_str(raw).decode_utf8().ok()?.into_owned();
    let id = bot_id(&token)?;
    Some((token, id))
}

/// Everything outside the control API.
pub(crate) async fn handle(shared: Arc<Shared>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if let Some(rest) = path.strip_prefix("/file/bot") {
        return download(&shared, request.method().as_str(), rest);
    }
    let Some(rest) = path.strip_prefix("/bot") else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    let (raw_token, raw_method) = rest.split_once('/').unwrap_or((rest, ""));
    let Some((token, id)) = decode_token(raw_token) else {
        return Answer::error(401, INVALID_TOKEN, None).into_response();
    };
    let method = percent_decode_str(raw_method)
        .decode_utf8_lossy()
        .into_owned();
    if method.is_empty() {
        return Answer::error(404, METHOD_NOT_FOUND, None).into_response();
    }
    let bot = shared.bot(&token, id);

    let (parts, body) = request.into_parts();
    let content_type = header(&parts.headers, CONTENT_TYPE);
    let transfer_encoding = header(&parts.headers, TRANSFER_ENCODING);
    let Ok(body) = to_bytes(body, BODY_LIMIT).await else {
        return Answer::error(413, "Request Entity Too Large", None).into_response();
    };
    let mut params = Params::default();
    if let Some(query) = parts.uri.query() {
        params.add_form(query.as_bytes());
    }
    if let Err(error) = params
        .add_body(content_type.as_deref().unwrap_or_default(), body)
        .await
    {
        return Answer::bad_request(error.description()).into_response();
    }
    let call = Call {
        method,
        http_method: parts.method.as_str().to_owned(),
        content_type,
        transfer_encoding,
        params,
        via: Via::Http,
    };
    execute(&shared, &bot, call).await.into_response()
}

/// Run one call: journal it, apply an injected fault, else answer it.
pub(crate) async fn execute(shared: &Arc<Shared>, bot: &Arc<Bot>, call: Call) -> Answer {
    let method = call.method.to_ascii_lowercase();
    let fault = shared.take_fault(&bot.token, &method);
    if method == "getupdates" && fault.is_none() {
        return get_updates(shared, bot, &call).await;
    }
    let seq = shared.journal.record(call.entry(bot));
    let answer = match fault {
        Some(fault) => {
            if let Some(delay) = fault.delay_ms {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    () = shared.cancel.cancelled() => {}
                }
            }
            fault_answer(&fault)
        }
        None => dispatch(shared, bot, &method, &call.params),
    };
    shared.journal.finish(seq, answer.status.as_u16());
    answer
}

fn fault_answer(fault: &Fault) -> Answer {
    if fault.status == 429 {
        let seconds = fault.retry_after.unwrap_or(1);
        let description = fault
            .description
            .clone()
            .unwrap_or_else(|| format!("Too Many Requests: retry after {seconds}"));
        return Answer::error(429, &description, Some(seconds));
    }
    let description = fault.description.clone().unwrap_or_else(|| {
        StatusCode::from_u16(fault.status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or("Error")
            .to_owned()
    });
    Answer::error(fault.status, &description, None)
}

fn dispatch(shared: &Shared, bot: &Bot, method: &str, params: &Params) -> Answer {
    let result = match method {
        "getme" => Ok(bot.user()),
        "setwebhook" => return set_webhook(bot, params),
        "deletewebhook" => return delete_webhook(bot, params),
        "getwebhookinfo" => Ok(webhook_info(&bot.state.lock())),
        "close" | "logout" => Ok(Value::Bool(true)),
        "getfile" => get_file(&bot.state.lock(), params),
        "senddocument" => send_document(shared, bot, params),
        "sendmediagroup" => send_media_group(bot, params),
        // `send*` methods that create no message and return True.
        "sendchataction"
        | "sendmessagedraft"
        | "sendrichmessagedraft"
        | "sendgift"
        | "sendchatjoinrequestwebapp" => Ok(Value::Bool(true)),
        "copymessage" => copy_message(bot, params),
        "copymessages" | "forwardmessages" => message_ids(bot, params),
        _ if ["send", "copy", "forward"]
            .iter()
            .any(|prefix| method.starts_with(prefix)) =>
        {
            let mut state = bot.state.lock();
            chat(params).map(|chat| message(bot, &mut state, chat, params))
        }
        _ => return Answer::error(404, METHOD_NOT_FOUND, None),
    };
    match result {
        Ok(result) => Answer::ok(&result),
        Err(answer) => answer,
    }
}

/// The `chat` of a message sent to `chat_id`.
fn chat(params: &Params) -> Result<Value, Answer> {
    let Some(raw) = params
        .get("chat_id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Err(Answer::bad_request("Bad Request: chat_id is empty"));
    };
    if let Ok(id) = raw.parse::<i64>() {
        let kind = match id {
            0 => return Err(Answer::bad_request("Bad Request: chat not found")),
            1.. => "private",
            ..-1_000_000_000_000 => "supergroup",
            _ => "group",
        };
        return Ok(json!({"id": id, "type": kind}));
    }
    match raw.strip_prefix('@') {
        Some(username) if !username.is_empty() => Ok(json!({
            "id": -1_000_000_000_001_i64,
            "type": "channel",
            "username": username,
        })),
        _ => Err(Answer::bad_request("Bad Request: chat not found")),
    }
}

fn next_message_id(state: &mut BotState) -> i64 {
    let id = state.next_message_id;
    state.next_message_id += 1;
    id
}

/// A sent message echoing `chat` and the text or caption.
fn message(bot: &Bot, state: &mut BotState, chat: Value, params: &Params) -> Value {
    let mut message = json!({
        "message_id": next_message_id(state),
        "from": bot.user(),
        "chat": chat,
        "date": unix_seconds(),
    });
    for field in ["text", "caption"] {
        if let Some(value) = params.get(field) {
            message[field] = Value::from(value);
        }
    }
    message
}

fn send_document(shared: &Shared, bot: &Bot, params: &Params) -> Result<Value, Answer> {
    let chat = chat(params)?;
    let mut state = bot.state.lock();
    let file_id = match params.file("document") {
        Some(part) => store(shared, &mut state, part),
        None => match params.get("document") {
            Some(id) if state.files.contains_key(id) => id.to_owned(),
            Some(_) => {
                return Err(Answer::bad_request(
                    "Bad Request: wrong file identifier/HTTP URL specified",
                ));
            }
            None => {
                return Err(Answer::bad_request(
                    "Bad Request: there is no document in the request",
                ));
            }
        },
    };
    let mut message = message(bot, &mut state, chat, params);
    if let Some(file) = state.files.get(&file_id) {
        let mut document = json!({
            "file_id": file.file_id,
            "file_unique_id": file.file_unique_id,
            "file_name": file.file_name,
            "file_size": file.data.len(),
        });
        if let Some(mime_type) = &file.mime_type {
            document["mime_type"] = Value::from(mime_type.as_str());
        }
        message["document"] = document;
    }
    Ok(message)
}

/// Keep an uploaded part under a new `file_id`; returns the id.
fn store(shared: &Shared, state: &mut BotState, part: &FilePart) -> String {
    let number = shared.next_file();
    let extension = part
        .filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    let file = StoredFile {
        file_id: format!("FAKEDOC{number}"),
        file_unique_id: format!("FAKEUNIQUE{number}"),
        file_path: format!("documents/file_{number}{extension}"),
        file_name: part.filename.clone(),
        mime_type: part.content_type.clone(),
        data: part.data.clone(),
    };
    let file_id = file.file_id.clone();
    state.files.insert(file_id.clone(), file);
    file_id
}

fn send_media_group(bot: &Bot, params: &Params) -> Result<Value, Answer> {
    let chat = chat(params)?;
    let count = params
        .get("media")
        .and_then(|media| serde_json::from_str::<Vec<Value>>(media).ok())
        .map_or(0, |media| media.len());
    if !(1..=10).contains(&count) {
        return Err(Answer::bad_request(
            "Bad Request: media must be an array of 1-10 items",
        ));
    }
    let mut state = bot.state.lock();
    let group = format!("fake-group-{}", state.next_message_id);
    let messages = (0..count)
        .map(|_| {
            let mut message = message(bot, &mut state, chat.clone(), params);
            message["media_group_id"] = Value::from(group.as_str());
            message
        })
        .collect();
    Ok(Value::Array(messages))
}

/// `copyMessage` returns a `MessageId`.
fn copy_message(bot: &Bot, params: &Params) -> Result<Value, Answer> {
    chat(params)?;
    let mut state = bot.state.lock();
    Ok(json!({"message_id": next_message_id(&mut state)}))
}

/// `copyMessages` and `forwardMessages` return one `MessageId` per source
/// message.
fn message_ids(bot: &Bot, params: &Params) -> Result<Value, Answer> {
    chat(params)?;
    let count = params
        .get("message_ids")
        .and_then(|ids| serde_json::from_str::<Vec<Value>>(ids).ok())
        .map_or(0, |ids| ids.len());
    if count == 0 {
        return Err(Answer::bad_request(
            "Bad Request: message identifiers are not specified",
        ));
    }
    let mut state = bot.state.lock();
    Ok(Value::Array(
        (0..count)
            .map(|_| json!({"message_id": next_message_id(&mut state)}))
            .collect(),
    ))
}

fn get_file(state: &BotState, params: &Params) -> Result<Value, Answer> {
    let file = params
        .get("file_id")
        .and_then(|id| state.files.get(id))
        .ok_or_else(|| Answer::bad_request("Bad Request: invalid file_id"))?;
    Ok(json!({
        "file_id": file.file_id,
        "file_unique_id": file.file_unique_id,
        "file_size": file.data.len(),
        "file_path": file.file_path,
    }))
}

fn set_webhook(bot: &Bot, params: &Params) -> Answer {
    let url = params.get("url").map_or("", str::trim);
    if url.is_empty() {
        return delete_webhook(bot, params);
    }
    let valid_url = reqwest::Url::parse(url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
    if !valid_url {
        return Answer::bad_request("Bad Request: invalid webhook URL specified");
    }
    let secret_token = params
        .get("secret_token")
        .filter(|secret| !secret.is_empty());
    if let Some(secret) = secret_token {
        if secret.len() > 256 {
            return Answer::bad_request("Bad Request: secret token is too long");
        }
        if !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Answer::bad_request("Bad Request: secret token contains unallowed characters");
        }
    }
    let webhook = Webhook {
        url: url.to_owned(),
        secret_token: secret_token.map(str::to_owned),
        max_connections: params.integer("max_connections", 40, 1, 100),
        allowed_updates: params
            .get("allowed_updates")
            .and_then(|updates| serde_json::from_str(updates).ok()),
    };
    let mut state = bot.state.lock();
    if params.flag("drop_pending_updates") {
        state.queue.clear();
    }
    let unchanged = state.webhook.as_ref().is_some_and(|current| {
        current.url == webhook.url && current.secret_token == webhook.secret_token
    });
    state.webhook = Some(webhook);
    state.last_error = None;
    drop(state);
    bot.changed.notify_waiters();
    let description = if unchanged {
        "Webhook is already set"
    } else {
        "Webhook was set"
    };
    Answer::ok_described(&Value::Bool(true), description)
}

fn delete_webhook(bot: &Bot, params: &Params) -> Answer {
    let mut state = bot.state.lock();
    if params.flag("drop_pending_updates") {
        state.queue.clear();
    }
    let was_set = state.webhook.take().is_some();
    state.last_error = None;
    drop(state);
    bot.changed.notify_waiters();
    let description = if was_set {
        "Webhook was deleted"
    } else {
        "Webhook is already deleted"
    };
    Answer::ok_described(&Value::Bool(true), description)
}

fn webhook_info(state: &BotState) -> Value {
    let mut info = json!({
        "url": "",
        "has_custom_certificate": false,
        "pending_update_count": state.queue.len(),
    });
    if let Some(webhook) = &state.webhook {
        info["url"] = Value::from(webhook.url.as_str());
        info["max_connections"] = Value::from(webhook.max_connections);
        if let Some(allowed) = &webhook.allowed_updates {
            info["allowed_updates"] = allowed.clone();
        }
    }
    if let Some((date, message)) = &state.last_error {
        info["last_error_date"] = Value::from(*date);
        info["last_error_message"] = Value::from(message.as_str());
    }
    info
}

/// `getUpdates`: refused while a webhook is set; otherwise it becomes the
/// bot's current poll (ending an older pending one with 409), confirms by
/// `offset` and returns at once or parks up to `timeout`.
async fn get_updates(shared: &Shared, bot: &Bot, call: &Call) -> Answer {
    let params = &call.params;
    let offset = params.integer("offset", 0, i64::MIN, i64::MAX);
    let limit = usize::try_from(params.integer("limit", LIMIT_MAX, 1, LIMIT_MAX)).unwrap_or(1);
    let timeout = params.integer("timeout", 0, 0, LONG_POLL_MAX);
    let timeout = Duration::from_secs(timeout.unsigned_abs());

    // Register before journaling: once a test sees the call, it is current.
    let poll = {
        let mut state = bot.state.lock();
        if state.webhook.is_some() {
            None
        } else {
            state.poll_seq += 1;
            state.confirm(offset);
            Some(state.poll_seq)
        }
    };
    let seq = shared.journal.record(call.entry(bot));
    let answer = match poll {
        None => Answer::error(409, WEBHOOK_ACTIVE, None),
        Some(poll) => {
            bot.changed.notify_waiters();
            long_poll(shared, bot, poll, limit, timeout).await
        }
    };
    shared.journal.finish(seq, answer.status.as_u16());
    answer
}

async fn long_poll(
    shared: &Shared,
    bot: &Bot,
    poll: u64,
    limit: usize,
    timeout: Duration,
) -> Answer {
    let deadline = Instant::now() + timeout;
    loop {
        // Register for the wake-up before looking, so a change between the
        // look and the wait is not missed.
        let mut notified = pin!(bot.changed.notified());
        notified.as_mut().enable();
        {
            let state = bot.state.lock();
            if state.webhook.is_some() {
                return Answer::error(409, TERMINATED_BY_SET_WEBHOOK, None);
            }
            if state.poll_seq != poll {
                return Answer::error(409, TERMINATED_BY_POLL, None);
            }
            if !state.queue.is_empty() {
                return Answer::batch(state.queue.iter().take(limit).map(|u| &u.payload));
            }
        }
        if Instant::now() >= deadline {
            return Answer::batch(std::iter::empty());
        }
        tokio::select! {
            () = notified => {}
            () = tokio::time::sleep_until(deadline) => {}
            () = shared.cancel.cancelled() => return Answer::batch(std::iter::empty()),
        }
    }
}

/// `GET /file/bot{token}/{file_path}`, token and path percent-decoded.
fn download(shared: &Arc<Shared>, http_method: &str, rest: &str) -> Response {
    let (raw_token, raw_path) = rest.split_once('/').unwrap_or((rest, ""));
    let Some((token, id)) = decode_token(raw_token) else {
        return Answer::error(401, INVALID_TOKEN, None).into_response();
    };
    let path = percent_decode_str(raw_path)
        .decode_utf8_lossy()
        .into_owned();
    let bot = shared.bot(&token, id);
    let mut entry = Entry::new(&token, id, Kind::File, http_method);
    entry.args.insert("file_path".to_owned(), path.clone());
    let seq = shared.journal.record(entry);
    let data = bot.state.lock().file_by_path(&path).map(|f| f.data.clone());
    match data {
        Some(data) => {
            shared.journal.finish(seq, 200);
            (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/octet-stream")],
                data,
            )
                .into_response()
        }
        None => {
            shared.journal.finish(seq, 404);
            Answer::error(404, "Not Found", None).into_response()
        }
    }
}

//! The chat a Bot API request targets, from its top-level `chat_id`.

use axum::http::HeaderValue;

use super::params::{Encoding, encoding, text_param};

/// A request's target chat: Telegram's `chat_id`, an integer or an
/// `@username`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatKey {
    Id(i64),
    /// The username without its `@`, lowercased: Telegram matches
    /// usernames ignoring case.
    Username(Box<str>),
}

impl ChatKey {
    /// Whether the chat is a group, supergroup or channel: Telegram gives
    /// those negative ids, and only they (and bots) are addressed by
    /// username.
    pub fn is_group(&self) -> bool {
        match self {
            Self::Id(id) => *id < 0,
            Self::Username(_) => true,
        }
    }
}

/// The `chat_id` of a JSON or urlencoded request body (see
/// [`text_param`]); `None` for any other body, multipart included, and for
/// a value that is neither an integer nor an `@username`.
pub fn chat_key(content_type: Option<&HeaderValue>, body: &[u8]) -> Option<ChatKey> {
    if encoding(content_type?)? == Encoding::Multipart {
        return None;
    }
    let chat_id = text_param(content_type, body, "chat_id")?;
    match chat_id.strip_prefix('@') {
        Some("") => None,
        Some(username) => Some(ChatKey::Username(
            username.to_ascii_lowercase().into_boxed_str(),
        )),
        None => chat_id.parse().ok().map(ChatKey::Id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(content_type: &str, body: &str) -> Option<ChatKey> {
        chat_key(
            Some(&HeaderValue::from_str(content_type).unwrap()),
            body.as_bytes(),
        )
    }

    fn json(body: &str) -> Option<ChatKey> {
        key("application/json", body)
    }

    fn form(body: &str) -> Option<ChatKey> {
        key("application/x-www-form-urlencoded", body)
    }

    fn username(name: &str) -> Option<ChatKey> {
        Some(ChatKey::Username(name.into()))
    }

    #[test]
    fn chat_key_forms() {
        assert_eq!(
            json(r#"{"chat_id":123,"text":"a"}"#),
            Some(ChatKey::Id(123))
        );
        assert_eq!(
            json(r#"{"text":"a","chat_id":-1001234567890}"#),
            Some(ChatKey::Id(-1_001_234_567_890))
        );
        assert_eq!(
            json(r#"{"chat_id":"-100123"}"#),
            Some(ChatKey::Id(-100_123))
        );
        assert_eq!(json(r#"{"chat_id":"@chan"}"#), username("chan"));
        assert_eq!(json(r#"{"chat_id":"@Chan_Name"}"#), username("chan_name"));
        assert_eq!(form("chat_id=@x&text=hi"), username("x"));
        assert_eq!(form("text=hi&chat_id=%40x"), username("x"));
        assert_eq!(form("chat_id=-5"), Some(ChatKey::Id(-5)));
        assert_eq!(
            key("application/json; charset=utf-8", r#"{"chat_id":7}"#),
            Some(ChatKey::Id(7))
        );

        // A nested chat_id is another chat's.
        assert_eq!(
            json(r#"{"reply_parameters":{"chat_id":5},"text":"a"}"#),
            None
        );
        // Multipart bodies are not read.
        let multipart =
            "--b\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n5\r\n--b--\r\n";
        assert_eq!(key("multipart/form-data; boundary=b", multipart), None);
        // Missing, or not a chat id.
        for body in [
            r#"{"text":"a"}"#,
            r#"{"chat_id":[1]}"#,
            r#"{"chat_id":{"id":1}}"#,
            r#"{"chat_id":null}"#,
            r#"{"chat_id":true}"#,
            r#"{"chat_id":1.5}"#,
            r#"{"chat_id":"chan"}"#,
            r#"{"chat_id":"@"}"#,
            r#"{"chat_id":""}"#,
            r#"{"chat_id":99999999999999999999}"#,
        ] {
            assert_eq!(json(body), None, "{body}");
        }
        assert_eq!(form("text=a"), None);
        // Read as the Bot API server reads it: a form value that is not
        // UTF-8 is absent, and a type naming both encodings is a form.
        assert_eq!(form("chat_id=%40%FF"), None);
        let form_type = HeaderValue::from_static("application/x-www-form-urlencoded");
        assert_eq!(chat_key(Some(&form_type), b"chat_id=@\xFF"), None);
        assert_eq!(form("chat_id=%40%C3%A9t%C3%A9"), username("\u{e9}t\u{e9}"));
        assert_eq!(
            key(
                "application/x-www-form-urlencoded; profile=application/json",
                "chat_id=@x"
            ),
            username("x")
        );
        assert_eq!(chat_key(None, br#"{"chat_id":1}"#), None);
    }

    #[test]
    fn chat_key_groups() {
        assert!(ChatKey::Id(-5).is_group());
        assert!(ChatKey::Id(-1_001_234).is_group());
        assert!(!ChatKey::Id(5).is_group());
        assert!(ChatKey::Username("chan".into()).is_group());
    }
}

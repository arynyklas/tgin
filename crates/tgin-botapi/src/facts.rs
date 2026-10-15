//! Routing facts from one update, in a single borrowed pass over its JSON.

use std::borrow::Cow;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use tgin_core::facts::{ChatType, Facts};

/// The most text kept in [`Facts::text`], in bytes.
const TEXT_CAP: usize = 4096;

/// Update types the Bot API sends. Escaped object keys cannot be borrowed
/// from the payload, so an escaped key names the update type only when it
/// decodes to one of these.
const KNOWN_UPDATE_TYPES: [&str; 23] = [
    "message",
    "edited_message",
    "channel_post",
    "edited_channel_post",
    "business_connection",
    "business_message",
    "edited_business_message",
    "deleted_business_messages",
    "message_reaction",
    "message_reaction_count",
    "inline_query",
    "chosen_inline_result",
    "callback_query",
    "shipping_query",
    "pre_checkout_query",
    "purchased_paid_media",
    "poll",
    "poll_answer",
    "my_chat_member",
    "chat_member",
    "chat_join_request",
    "chat_boost",
    "removed_chat_boost",
];

/// Why an update could not be read.
#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    #[error("the update is not a valid JSON object: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the update has no update_id")]
    MissingUpdateId,
}

/// Reads `update_id` and the routing facts of one update in a single pass,
/// borrowing from `payload` wherever the JSON holds the value verbatim.
///
/// - `update_type` is the first key other than `update_id` whose value is an
///   object, so unknown update types still get one. Empty if there is none.
/// - `chat` is `.chat`, or `.message.chat` for `callback_query`.
/// - `user_id` is `.from.id`, or `.user.id`.
/// - `text` is `.text`, `.caption`, or `.query` for `inline_query`, cut to
///   4 KiB on a char boundary. Escaped strings are decoded into owned text.
/// - `callback_data` is `.data` for `callback_query`.
/// - `command` is the leading `/token` of `text`, split at the first `@`. It
///   borrows from the payload, so a token that contains a JSON escape (other
///   than the leading `\/` the Bot API writes) yields no command; command
///   names are ASCII, so real commands are never escaped.
///
/// Fields of an unexpected JSON type count as absent; only a payload that is
/// not a JSON object, or has no integer `update_id`, is an error.
pub fn extract(payload: &[u8]) -> Result<(i64, Facts<'_>), FactsError> {
    let update: Update<'_> = serde_json::from_slice(payload)?;
    let update_id = update.update_id.ok_or(FactsError::MissingUpdateId)?;
    Ok((update_id, update.into_facts()?))
}

#[derive(Default)]
struct Update<'a> {
    update_id: Option<i64>,
    kind: Option<(&'a str, Payload<'a>)>,
}

/// What the extractor keeps from the update-type object.
#[derive(Default)]
struct Payload<'a> {
    /// `.chat`, when it is an object.
    chat: Option<Chat>,
    /// `.message.chat`, read for callback queries only.
    message_chat: Option<Chat>,
    /// `.from`, when it is an object: its `id`.
    from: Option<Option<i64>>,
    /// `.user`, when it is an object: its `id`.
    user: Option<Option<i64>>,
    text: Option<&'a RawValue>,
    caption: Option<&'a RawValue>,
    /// Read for inline queries only.
    query: Option<&'a RawValue>,
    /// Read for callback queries only.
    data: Option<&'a RawValue>,
}

#[derive(Clone, Copy)]
struct Chat {
    id: Option<i64>,
    kind: Option<ChatType>,
}

impl<'a> Update<'a> {
    fn into_facts(self) -> Result<Facts<'a>, serde_json::Error> {
        let Some((update_type, p)) = self.kind else {
            return Ok(Facts::default());
        };
        let chat = p.chat.or(p.message_chat);
        let text = [p.text, p.caption, p.query]
            .into_iter()
            .flatten()
            .find_map(|raw| string_contents(raw).map(|inner| (raw, inner)));
        let (text, command) = match text {
            Some((raw, inner)) => {
                let text = cap(decode(raw, inner)?);
                let command = command(&text, inner);
                (Some(text), command)
            }
            None => (None, None),
        };
        let callback_data = match p.data.and_then(|raw| Some((raw, string_contents(raw)?))) {
            Some((raw, inner)) => Some(decode(raw, inner)?),
            None => None,
        };
        Ok(Facts {
            update_type,
            chat_id: chat.and_then(|c| c.id),
            chat_type: chat.and_then(|c| c.kind),
            user_id: p.from.or(p.user).flatten(),
            text,
            callback_data,
            command,
        })
    }
}

/// The text between the quotes when `raw` is a JSON string.
fn string_contents(raw: &RawValue) -> Option<&str> {
    raw.get().strip_prefix('"')?.strip_suffix('"')
}

/// `raw` (a JSON string with contents `inner`) decoded: borrowed when it has
/// no escapes.
fn decode<'a>(raw: &'a RawValue, inner: &'a str) -> Result<Cow<'a, str>, serde_json::Error> {
    if inner.contains('\\') {
        serde_json::from_str(raw.get()).map(Cow::Owned)
    } else {
        Ok(Cow::Borrowed(inner))
    }
}

/// `text` cut to at most [`TEXT_CAP`] bytes on a char boundary.
fn cap(text: Cow<'_, str>) -> Cow<'_, str> {
    if text.len() <= TEXT_CAP {
        return text;
    }
    let mut end = TEXT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text {
        Cow::Borrowed(s) => Cow::Borrowed(&s[..end]),
        Cow::Owned(mut s) => {
            s.truncate(end);
            Cow::Owned(s)
        }
    }
}

/// The leading `/name[@bot]` of `text`, borrowed from the payload. `raw` is
/// the still-escaped JSON string `text` was decoded from.
fn command<'a>(text: &Cow<'a, str>, raw: &'a str) -> Option<(&'a str, Option<&'a str>)> {
    let rest = text.strip_prefix('/')?;
    let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let token: &'a str = match text {
        Cow::Borrowed(s) => s.get(1..1 + len)?,
        Cow::Owned(_) => {
            // Find the same bytes in the escaped source after its `/`.
            let after = ["/", "\\/", "\\u002f", "\\u002F"]
                .into_iter()
                .find_map(|slash| raw.strip_prefix(slash))?;
            after.get(..len).filter(|t| *t == &rest[..len])?
        }
    };
    Some(match token.split_once('@') {
        Some((name, bot)) => (name, Some(bot)),
        None => (token, None),
    })
}

impl<'de> Deserialize<'de> for Update<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(UpdateVisitor)
    }
}

struct UpdateVisitor;

impl<'de> Visitor<'de> for UpdateVisitor {
    type Value = Update<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an update object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut update = Update::default();
        while let Some(key) = map.next_key::<TopKey<'de>>()? {
            match key {
                TopKey::UpdateId => update.update_id = Some(map.next_value()?),
                TopKey::Name(name) if update.kind.is_none() => {
                    let seed = PayloadSeed {
                        callback_query: name == "callback_query",
                        inline_query: name == "inline_query",
                    };
                    if let Some(payload) = map.next_value_seed(seed)? {
                        update.kind = Some((name, payload));
                    }
                }
                TopKey::Name(_) | TopKey::Unborrowable => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(update)
    }
}

/// A top-level key.
enum TopKey<'a> {
    UpdateId,
    Name(&'a str),
    /// An escaped key that is not a known update type.
    Unborrowable,
}

impl<'de> Deserialize<'de> for TopKey<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = TopKey<'de>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a key")
            }

            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(if v == "update_id" {
                    TopKey::UpdateId
                } else {
                    TopKey::Name(v)
                })
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(if v == "update_id" {
                    TopKey::UpdateId
                } else {
                    KNOWN_UPDATE_TYPES
                        .into_iter()
                        .find(|known| *known == v)
                        .map_or(TopKey::Unborrowable, TopKey::Name)
                })
            }
        }
        d.deserialize_str(V)
    }
}

/// A key inside the update-type object or one of its nested objects.
enum Field {
    Chat,
    Message,
    From,
    User,
    Text,
    Caption,
    Query,
    Data,
    Id,
    Type,
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Field;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a key")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Field, E> {
                Ok(match v {
                    "chat" => Field::Chat,
                    "message" => Field::Message,
                    "from" => Field::From,
                    "user" => Field::User,
                    "text" => Field::Text,
                    "caption" => Field::Caption,
                    "query" => Field::Query,
                    "data" => Field::Data,
                    "id" => Field::Id,
                    "type" => Field::Type,
                    _ => Field::Other,
                })
            }
        }
        d.deserialize_str(V)
    }
}

/// Visitor methods that consume a JSON value of another kind and yield
/// `None`, so a field of an unexpected type reads as absent.
macro_rules! none_for {
    ($de:lifetime; $($kind:ident),*) => {
        $(none_for!(@ $de $kind);)*
    };
    (@ $de:lifetime bool) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime i64) => {
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime u64) => {
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime f64) => {
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime str) => {
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime unit) => {
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    };
    (@ $de:lifetime seq) => {
        fn visit_seq<A: SeqAccess<$de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    };
    (@ $de:lifetime map) => {
        fn visit_map<A: MapAccess<$de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    };
}

/// Implements `DeserializeSeed` for a `Copy` visitor via `deserialize_any`.
macro_rules! seed_via_any {
    ($($ty:ty),*) => {$(
        impl<'de> DeserializeSeed<'de> for $ty {
            type Value = <Self as Visitor<'de>>::Value;

            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_any(self)
            }
        }
    )*};
}

seed_via_any!(
    PayloadSeed,
    ChatSeed,
    MessageChatSeed,
    UserSeed,
    LenientI64,
    ChatTypeSeed
);

/// The update-type object; `None` when the value is not an object.
#[derive(Clone, Copy)]
struct PayloadSeed {
    callback_query: bool,
    inline_query: bool,
}

impl<'de> Visitor<'de> for PayloadSeed {
    type Value = Option<Payload<'de>>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, i64, u64, f64, str, unit, seq);

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut p = Payload::default();
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Chat => p.chat = map.next_value_seed(ChatSeed)?,
                Field::Message if self.callback_query => {
                    p.message_chat = map.next_value_seed(MessageChatSeed)?;
                }
                Field::From => p.from = map.next_value_seed(UserSeed)?,
                Field::User => p.user = map.next_value_seed(UserSeed)?,
                Field::Text => p.text = Some(map.next_value()?),
                Field::Caption => p.caption = Some(map.next_value()?),
                Field::Query if self.inline_query => p.query = Some(map.next_value()?),
                Field::Data if self.callback_query => p.data = Some(map.next_value()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Some(p))
    }
}

/// A chat object; `None` when the value is not an object.
#[derive(Clone, Copy)]
struct ChatSeed;

impl<'de> Visitor<'de> for ChatSeed {
    type Value = Option<Chat>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, i64, u64, f64, str, unit, seq);

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut chat = Chat {
            id: None,
            kind: None,
        };
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Id => chat.id = map.next_value_seed(LenientI64)?,
                Field::Type => chat.kind = map.next_value_seed(ChatTypeSeed)?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Some(chat))
    }
}

/// A message's chat object; `None` unless both are objects.
#[derive(Clone, Copy)]
struct MessageChatSeed;

impl<'de> Visitor<'de> for MessageChatSeed {
    type Value = Option<Chat>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, i64, u64, f64, str, unit, seq);

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut chat = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Chat => chat = map.next_value_seed(ChatSeed)?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(chat)
    }
}

/// A user object's `id`; the outer `None` when the value is not an object.
#[derive(Clone, Copy)]
struct UserSeed;

impl<'de> Visitor<'de> for UserSeed {
    type Value = Option<Option<i64>>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, i64, u64, f64, str, unit, seq);

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut id = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Id => id = map.next_value_seed(LenientI64)?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Some(id))
    }
}

/// An integer that fits `i64`; `None` for anything else.
#[derive(Clone, Copy)]
struct LenientI64;

impl<'de> Visitor<'de> for LenientI64 {
    type Value = Option<i64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, f64, str, unit, seq, map);

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Some(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(i64::try_from(v).ok())
    }
}

/// A chat `type` string; `None` for anything else.
#[derive(Clone, Copy)]
struct ChatTypeSeed;

impl<'de> Visitor<'de> for ChatTypeSeed {
    type Value = Option<ChatType>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    none_for!('de; bool, i64, u64, f64, unit, seq, map);

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Some(match v {
            "private" => ChatType::Private,
            "group" => ChatType::Group,
            "supergroup" => ChatType::Supergroup,
            "channel" => ChatType::Channel,
            _ => ChatType::Other,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::fmt::{self, Write as _};

    use proptest::prelude::*;
    use proptest::sample::select;
    use serde::Deserialize;
    use serde::de::{MapAccess, Visitor};
    use serde_json::{Value, json};
    use tgin_core::facts::ChatType;

    use super::*;

    fn facts(json: &str) -> (i64, Facts<'_>) {
        extract(json.as_bytes()).expect("the update is valid")
    }

    /// Whether `s` points into `payload`.
    fn inside(payload: &str, s: &str) -> bool {
        let range = payload.as_bytes().as_ptr_range();
        let start = s.as_ptr();
        range.contains(&start) || (s.is_empty() && start == range.end)
    }

    #[test]
    fn extract_message() {
        let raw = r#"{"update_id":10,"message":{"message_id":1,"from":{"id":42,"is_bot":false,"first_name":"A"},"chat":{"id":-1001234,"type":"supergroup","title":"G"},"date":1,"text":"hello world"}}"#;
        let (id, f) = facts(raw);
        assert_eq!(id, 10);
        assert_eq!(
            f,
            Facts {
                update_type: "message",
                chat_id: Some(-1001234),
                chat_type: Some(ChatType::Supergroup),
                user_id: Some(42),
                text: Some(Cow::Borrowed("hello world")),
                callback_data: None,
                command: None,
            }
        );
        assert!(inside(raw, f.update_type));
        match &f.text {
            Some(Cow::Borrowed(t)) => assert!(inside(raw, t)),
            other => panic!("text is not borrowed: {other:?}"),
        }

        // A caption stands in for missing text.
        let raw = r#"{"update_id":11,"edited_message":{"chat":{"id":7,"type":"private"},"caption":"a photo","from":{"id":7}}}"#;
        let (_, f) = facts(raw);
        assert_eq!(f.update_type, "edited_message");
        assert_eq!(f.chat_type, Some(ChatType::Private));
        assert_eq!(f.text.as_deref(), Some("a photo"));
    }

    #[test]
    fn extract_callback_query() {
        let raw = r#"{"update_id":12,"callback_query":{"id":"77","from":{"id":5,"first_name":"U"},"message":{"message_id":3,"chat":{"id":-5,"type":"group"},"date":1,"text":"menu"},"chat_instance":"ci","data":"buy:1"}}"#;
        let (_, f) = facts(raw);
        assert_eq!(
            f,
            Facts {
                update_type: "callback_query",
                chat_id: Some(-5),
                chat_type: Some(ChatType::Group),
                user_id: Some(5),
                text: None,
                callback_data: Some(Cow::Borrowed("buy:1")),
                command: None,
            }
        );
        assert!(matches!(f.callback_data, Some(Cow::Borrowed(_))));

        // `data` and `message.chat` count only for callback queries.
        let raw = r#"{"update_id":13,"message":{"message":{"chat":{"id":1,"type":"private"}},"data":"x"}}"#;
        let (_, f) = facts(raw);
        assert_eq!((f.chat_id, f.callback_data), (None, None));
    }

    #[test]
    fn extract_inline_query() {
        let raw = r#"{"update_id":14,"inline_query":{"id":"q1","from":{"id":9,"first_name":"I"},"query":"/cats cute","offset":"","chat_type":"private"}}"#;
        let (_, f) = facts(raw);
        assert_eq!(
            f,
            Facts {
                update_type: "inline_query",
                chat_id: None,
                chat_type: None,
                user_id: Some(9),
                text: Some(Cow::Borrowed("/cats cute")),
                callback_data: None,
                command: Some(("cats", None)),
            }
        );

        // `query` is text only for inline queries.
        let raw = r#"{"update_id":15,"chosen_inline_result":{"result_id":"r","from":{"id":9},"query":"q"}}"#;
        assert_eq!(facts(raw).1.text, None);
    }

    #[test]
    fn extract_chat_member() {
        let raw = r#"{"update_id":16,"chat_member":{"chat":{"id":-100,"type":"channel","title":"C"},"from":{"id":1},"date":1,"old_chat_member":{"user":{"id":2},"status":"left"},"new_chat_member":{"user":{"id":2},"status":"member"}}}"#;
        let (_, f) = facts(raw);
        assert_eq!(f.update_type, "chat_member");
        assert_eq!(f.chat_id, Some(-100));
        assert_eq!(f.chat_type, Some(ChatType::Channel));
        assert_eq!(f.user_id, Some(1));

        // `user` stands in for a missing `from`.
        let raw =
            r#"{"update_id":17,"poll_answer":{"poll_id":"p","user":{"id":3},"option_ids":[0]}}"#;
        assert_eq!(facts(raw).1.user_id, Some(3));
    }

    #[test]
    fn extract_unknown_type() {
        let raw = r#"{"x_flag":true,"update_id":18,"x_list":[{"a":1}],"future_thing":{"chat":{"id":3,"type":"brand_new"},"user":{"id":4},"text":"t"},"message":{"chat":{"id":9}}}"#;
        let (id, f) = facts(raw);
        assert_eq!(id, 18);
        assert_eq!(f.update_type, "future_thing");
        assert_eq!(f.chat_id, Some(3));
        assert_eq!(f.chat_type, Some(ChatType::Other));
        assert_eq!(f.user_id, Some(4));
        assert_eq!(f.text.as_deref(), Some("t"));

        // No object-valued key: no update type.
        assert_eq!(facts(r#"{"update_id":19,"x":1}"#).1, Facts::default());

        // An escaped key still names a known update type.
        let (_, f) = facts(r#"{"update_id":20,"me\u0073sage":{"te\u0078t":"hi"}}"#);
        assert_eq!(f.update_type, "message");
        assert_eq!(f.text.as_deref(), Some("hi"));
    }

    #[test]
    fn extract_rejects_bad_input() {
        for bad in [
            "",
            "[]",
            "{",
            r#"{"message":{}}"#,
            r#"{"update_id":"1","message":{}}"#,
            r#"{"update_id":1.5}"#,
        ] {
            assert!(extract(bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(matches!(
            extract(br#"{"message":{}}"#),
            Err(FactsError::MissingUpdateId)
        ));
    }

    #[test]
    fn extract_escaped_text_owned() {
        // Telegram escapes `/` and non-ASCII text.
        let raw = r#"{"update_id":21,"message":{"chat":{"id":1,"type":"private"},"text":"\/start@my_bot \"quoted\" \u041f\ud83d\ude00"}}"#;
        let (_, f) = facts(raw);
        assert_eq!(
            f.text,
            Some(Cow::<str>::Owned("/start@my_bot \"quoted\" П😀".into()))
        );
        assert!(matches!(f.text, Some(Cow::Owned(_))));
        let (name, bot) = f.command.expect("a command");
        assert_eq!((name, bot), ("start", Some("my_bot")));
        assert!(inside(raw, name));
        assert!(inside(raw, bot.unwrap_or_default()));

        let raw = r#"{"update_id":22,"callback_query":{"from":{"id":1},"data":"a\nb"}}"#;
        assert!(matches!(facts(raw).1.callback_data, Some(Cow::Owned(d)) if d == "a\nb"));
    }

    #[test]
    fn extract_text_capped_4k() {
        // 3-byte chars: 4096 is not a char boundary, so the cut lands at 4095.
        let long = "€".repeat(2000);
        let raw = format!(r#"{{"update_id":23,"message":{{"text":"{long}"}}}}"#);
        let (_, f) = facts(&raw);
        let text = f.text.expect("text");
        assert!(matches!(text, Cow::Borrowed(_)));
        assert_eq!(text.len(), 4095);
        assert_eq!(text, long[..4095]);

        let escaped = "\\u20ac".repeat(2000);
        let raw = format!(r#"{{"update_id":24,"message":{{"text":"{escaped}"}}}}"#);
        let text = facts(&raw).1.text.expect("text");
        assert!(matches!(text, Cow::Owned(_)));
        assert_eq!(text, long[..4095]);

        let exact = "a".repeat(4096);
        let raw = format!(r#"{{"update_id":25,"message":{{"text":"{exact}b"}}}}"#);
        assert_eq!(facts(&raw).1.text.as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn command_with_bot_suffix() {
        type Command = Option<(&'static str, Option<&'static str>)>;
        let cases: [(&str, Command); 8] = [
            ("/start@my_bot payload", Some(("start", Some("my_bot")))),
            ("/start@my_bot\nmore", Some(("start", Some("my_bot")))),
            ("/help", Some(("help", None))),
            ("/a@b@c d", Some(("a", Some("b@c")))),
            ("/x@", Some(("x", Some("")))),
            ("/", Some(("", None))),
            ("hello /start", None),
            (" /start", None),
        ];
        for (text, expected) in cases {
            let raw = json!({"update_id": 1, "message": {"text": text}}).to_string();
            assert_eq!(facts(&raw).1.command, expected, "{text:?}");
        }
    }

    // --- Reference extractor over `serde_json::Value` -----------------------

    /// A JSON object's entries in document order.
    struct Ordered(Vec<(String, Value)>);

    impl<'de> Deserialize<'de> for Ordered {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Ordered;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                    let mut entries = Vec::new();
                    while let Some(entry) = map.next_entry()? {
                        entries.push(entry);
                    }
                    Ok(Ordered(entries))
                }
            }
            d.deserialize_map(V)
        }
    }

    #[derive(Debug, Default, PartialEq)]
    struct OwnedFacts {
        update_type: String,
        chat_id: Option<i64>,
        chat_type: Option<ChatType>,
        user_id: Option<i64>,
        text: Option<String>,
        callback_data: Option<String>,
        command: Option<(String, Option<String>)>,
    }

    impl From<&Facts<'_>> for OwnedFacts {
        fn from(f: &Facts<'_>) -> Self {
            Self {
                update_type: f.update_type.into(),
                chat_id: f.chat_id,
                chat_type: f.chat_type,
                user_id: f.user_id,
                text: f.text.as_deref().map(Into::into),
                callback_data: f.callback_data.as_deref().map(Into::into),
                command: f.command.map(|(n, b)| (n.into(), b.map(Into::into))),
            }
        }
    }

    fn reference(json: &str) -> (i64, OwnedFacts) {
        let Ordered(top) = serde_json::from_str(json).expect("generated JSON parses");
        let id = top
            .iter()
            .rev()
            .find(|(k, _)| k == "update_id")
            .and_then(|(_, v)| v.as_i64())
            .expect("generated updates carry an update_id");
        let Some((kind, p)) = top.iter().find(|(k, v)| k != "update_id" && v.is_object()) else {
            return (id, OwnedFacts::default());
        };
        let callback = kind == "callback_query";
        let inline = kind == "inline_query";
        fn object(v: Option<&Value>) -> Option<&Value> {
            v.filter(|v| v.is_object())
        }
        let string = |key: &str| p.get(key).and_then(Value::as_str);

        let chat = object(p.get("chat")).or_else(|| {
            callback
                .then(|| object(p.get("message").and_then(|m| m.get("chat"))))
                .flatten()
        });
        let user = object(p.get("from")).or_else(|| object(p.get("user")));
        let text = string("text")
            .or_else(|| string("caption"))
            .or_else(|| inline.then(|| string("query")).flatten())
            .map(|t| {
                let mut used = 0;
                t.chars()
                    .take_while(|c| {
                        used += c.len_utf8();
                        used <= 4096
                    })
                    .collect::<String>()
            });
        let command = text.as_deref().and_then(|t| {
            let token: String = t
                .strip_prefix('/')?
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            Some(match token.split_once('@') {
                Some((name, bot)) => (name.to_owned(), Some(bot.to_owned())),
                None => (token, None),
            })
        });
        let facts = OwnedFacts {
            update_type: kind.clone(),
            chat_id: chat.and_then(|c| c.get("id")).and_then(Value::as_i64),
            chat_type: chat
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                .map(|t| match t {
                    "private" => ChatType::Private,
                    "group" => ChatType::Group,
                    "supergroup" => ChatType::Supergroup,
                    "channel" => ChatType::Channel,
                    _ => ChatType::Other,
                }),
            user_id: user.and_then(|u| u.get("id")).and_then(Value::as_i64),
            text,
            callback_data: callback.then(|| string("data").map(Into::into)).flatten(),
            command,
        };
        (id, facts)
    }

    // --- Generator ------------------------------------------------------------

    const UPDATE_TYPES: [&str; 12] = [
        "message",
        "edited_message",
        "channel_post",
        "edited_channel_post",
        "callback_query",
        "inline_query",
        "chosen_inline_result",
        "my_chat_member",
        "chat_member",
        "chat_join_request",
        "poll_answer",
        "message_reaction",
    ];

    /// Fields the extractor reads, in generator mask order.
    const FIELDS: [&str; 8] = [
        "chat", "from", "user", "text", "caption", "query", "data", "message",
    ];

    /// The fields each shape usually carries.
    fn typical(kind: &str) -> &'static [&'static str] {
        match kind {
            "message" | "edited_message" => &["chat", "from", "text", "caption"],
            "channel_post" | "edited_channel_post" => &["chat", "text", "caption"],
            "callback_query" => &["from", "message", "data"],
            "inline_query" | "chosen_inline_result" => &["from", "query"],
            "my_chat_member" | "chat_member" | "chat_join_request" => &["chat", "from"],
            "poll_answer" => &["user"],
            "message_reaction" => &["chat", "user"],
            _ => &[],
        }
    }

    fn id_value() -> BoxedStrategy<Value> {
        prop_oneof![
            8 => any::<i64>().prop_map(Value::from),
            1 => any::<u64>().prop_map(Value::from),
            1 => Just(json!(1.5)),
            1 => Just(json!("7")),
        ]
        .boxed()
    }

    const CHARS: [char; 16] = [
        'a', 'Z', '0', ' ', '"', '\\', '\n', '\t', '\u{1}', '\u{7f}', 'é', '€', '😀', '/', '@',
        '\u{a0}',
    ];

    /// Text that never starts with `/`.
    fn free_text() -> BoxedStrategy<String> {
        let short =
            prop::collection::vec(select(CHARS.to_vec()), 0..24).prop_map(String::from_iter);
        let long = (
            short.clone(),
            select(vec!['a', 'é', '€', '😀']),
            1000..4200usize,
        )
            .prop_map(|(head, c, n)| head + &c.to_string().repeat(n));
        prop_oneof![8 => short, 2 => long]
            .prop_map(|s| {
                if s.starts_with('/') {
                    format!("x{s}")
                } else {
                    s
                }
            })
            .boxed()
    }

    /// Text that is sometimes a bot command. Command tokens are ASCII, which
    /// is what Telegram allows.
    fn text() -> BoxedStrategy<String> {
        let whitespace = select(vec![" ", "\n", "\t", "\u{a0}", "\u{3000}"]);
        let command = (
            "[a-z0-9_]{0,8}",
            prop::option::of("@[A-Za-z0-9_@]{0,6}"),
            prop::option::of((whitespace, free_text())),
        )
            .prop_map(|(name, bot, rest)| {
                let rest = rest.map(|(ws, t)| format!("{ws}{t}")).unwrap_or_default();
                format!("/{name}{}{rest}", bot.unwrap_or_default())
            });
        prop_oneof![free_text(), command].boxed()
    }

    fn text_value() -> BoxedStrategy<Value> {
        prop_oneof![
            8 => text().prop_map(Value::from),
            1 => Just(Value::Null),
            1 => any::<i32>().prop_map(Value::from),
        ]
        .boxed()
    }

    fn any_value() -> BoxedStrategy<Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            (-1e9..1e9f64).prop_map(Value::from),
            free_text().prop_map(Value::from),
        ];
        leaf.prop_recursive(2, 8, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..3).prop_map(Value::from),
                prop::collection::btree_map("[a-z]{1,4}", inner, 0..3)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
        .boxed()
    }

    fn extras() -> BoxedStrategy<Vec<(String, Value)>> {
        prop::collection::vec(("x_[a-z]{1,5}", any_value()), 0..3).boxed()
    }

    fn chat_value() -> BoxedStrategy<Value> {
        let kind = prop_oneof![
            4 => select(vec!["private", "group", "supergroup", "channel", "secret"])
                .prop_map(Value::from),
            1 => Just(json!(5)),
        ];
        let chat = (
            prop::option::weighted(0.9, id_value()),
            prop::option::weighted(0.9, kind),
            extras(),
        )
            .prop_map(|(id, kind, extra)| {
                let mut m: serde_json::Map<String, Value> = extra.into_iter().collect();
                if let Some(id) = id {
                    m.insert("id".into(), id);
                }
                if let Some(kind) = kind {
                    m.insert("type".into(), kind);
                }
                Value::Object(m)
            });
        prop_oneof![9 => chat, 1 => Just(json!("not a chat"))].boxed()
    }

    fn user_value() -> BoxedStrategy<Value> {
        let user = (prop::option::weighted(0.9, id_value()), extras()).prop_map(|(id, extra)| {
            let mut m: serde_json::Map<String, Value> = extra.into_iter().collect();
            m.insert("first_name".into(), json!("N"));
            if let Some(id) = id {
                m.insert("id".into(), id);
            }
            Value::Object(m)
        });
        prop_oneof![9 => user, 1 => Just(json!(42))].boxed()
    }

    fn message_value() -> BoxedStrategy<Value> {
        let message = (chat_value(), text_value()).prop_map(
            |(chat, text)| json!({"message_id": 1, "date": 2, "chat": chat, "text": text}),
        );
        prop_oneof![9 => message, 1 => Just(json!([1, 2]))].boxed()
    }

    /// Sort keys that put up to 16 entries in a random order.
    fn order() -> BoxedStrategy<Vec<u32>> {
        prop::collection::vec(any::<u32>(), 16).boxed()
    }

    fn shuffle<T>(entries: Vec<T>, order: &[u32]) -> Vec<T> {
        let mut keyed: Vec<(u32, T)> = order.iter().copied().zip(entries).collect();
        keyed.sort_by_key(|(key, _)| *key);
        keyed.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Candidate values for every field in [`FIELDS`], which of them to
    /// keep (typical fields usually, others rarely), extras, and an order.
    type PayloadParts = (
        Vec<Value>,
        Vec<bool>,
        Vec<bool>,
        Vec<(String, Value)>,
        Vec<u32>,
    );

    fn payload_parts() -> BoxedStrategy<PayloadParts> {
        let values = (
            chat_value(),
            user_value(),
            user_value(),
            text_value(),
            text_value(),
            text_value(),
            text_value(),
            message_value(),
        )
            .prop_map(|(a, b, c, d, e, f, g, h)| vec![a, b, c, d, e, f, g, h]);
        (
            values,
            prop::collection::vec(prop::bool::weighted(0.85), FIELDS.len()),
            prop::collection::vec(prop::bool::weighted(0.15), FIELDS.len()),
            extras(),
            order(),
        )
            .boxed()
    }

    /// A shape's payload entries, serialized, in a random order.
    fn payload(kind: &str, (values, hi, lo, extra, order): PayloadParts) -> Vec<(String, String)> {
        let mut entries: Vec<(String, Value)> = FIELDS
            .iter()
            .zip(values)
            .enumerate()
            .filter(|(i, (name, _))| {
                if typical(kind).contains(name) {
                    hi[*i]
                } else {
                    lo[*i]
                }
            })
            .map(|(_, (name, value))| ((*name).to_owned(), value))
            .collect();
        entries.extend(extra);
        shuffle(entries, &order)
            .into_iter()
            .map(|(k, v)| (k, v.to_string()))
            .collect()
    }

    fn object(entries: &[(String, String)]) -> String {
        let mut out = String::from("{");
        for (i, (key, value)) in entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(key).expect("a key serializes"));
            out.push(':');
            out.push_str(value);
        }
        out.push('}');
        out
    }

    /// Escapes like the Bot API does: `/` as `\/` and non-ASCII as `\uXXXX`.
    /// Outside strings, JSON has neither.
    fn telegram_escape(json: &str) -> String {
        let mut out = String::with_capacity(json.len() * 2);
        for c in json.chars() {
            match c {
                '/' => out.push_str("\\/"),
                c if c.is_ascii() => out.push(c),
                c => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        write!(out, "\\u{unit:04x}").expect("writing to a String");
                    }
                }
            }
        }
        out
    }

    fn update_json() -> BoxedStrategy<String> {
        let kind = prop_oneof![
            4 => select(UPDATE_TYPES.to_vec()).prop_map(String::from),
            1 => "[a-w][a-z_]{2,11}".prop_filter("not update_id", |k| k != "update_id"),
        ];
        let top_extras = prop::collection::vec(
            (
                "x_[a-z]{1,5}",
                prop_oneof![4 => any::<i64>().prop_map(Value::from), 1 => any_value()],
            ),
            0..3,
        );
        (
            kind,
            payload_parts(),
            any::<i64>(),
            top_extras,
            order(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(kind, parts, id, extra, order, id_first, escape)| {
                let payload = object(&payload(&kind, parts));
                let mut top = vec![("update_id".to_owned(), id.to_string()), (kind, payload)];
                top.extend(extra.into_iter().map(|(k, v)| (k, v.to_string())));
                let mut top = shuffle(top, &order);
                // Telegram puts update_id first; keep it there half the time.
                if id_first && let Some(at) = top.iter().position(|(k, _)| k == "update_id") {
                    let entry = top.remove(at);
                    top.insert(0, entry);
                }
                let json = object(&top);
                if escape { telegram_escape(&json) } else { json }
            })
            .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn extract_matches_reference(json in update_json()) {
            let expected = reference(&json);
            let (id, f) = extract(json.as_bytes())
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            if let Some((name, bot)) = f.command {
                prop_assert!(inside(&json, name));
                prop_assert!(inside(&json, bot.unwrap_or(name)));
            }
            if let Some(Cow::Borrowed(t)) = &f.text {
                prop_assert!(inside(&json, t));
            }
            prop_assert_eq!((id, OwnedFacts::from(&f)), expected);
        }
    }
}

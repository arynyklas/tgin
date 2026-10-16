//! The routing-relevant facts extracted from one update.

use std::borrow::Cow;

/// Telegram chat type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatType {
    Private,
    Group,
    Supergroup,
    Channel,
    Other,
}

impl ChatType {
    /// One bit per variant, for compiled chat-type sets.
    pub(crate) const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Facts the rules engine matches on. Borrowed from the payload where possible.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts<'a> {
    /// The update type, e.g. `message` or `callback_query`.
    pub update_type: &'a str,
    pub chat_id: Option<i64>,
    pub chat_type: Option<ChatType>,
    pub user_id: Option<i64>,
    /// Message text or caption, at most 4 KiB.
    pub text: Option<Cow<'a, str>>,
    pub callback_data: Option<Cow<'a, str>>,
    /// A leading bot command: the name without `/`, and the `@bot` suffix
    /// without `@` if present.
    pub command: Option<(&'a str, Option<&'a str>)>,
}

impl Facts<'_> {
    /// The ordering key: `chat_id`, else `user_id`. The `i64` is reinterpreted
    /// as `u64` (`as u64`, two's-complement bits), so negative chat ids map to
    /// large keys without collisions.
    pub fn ordering_key(&self) -> Option<u64> {
        self.chat_id.or(self.user_id).map(|id| id as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_key_prefers_chat_then_user() {
        let mut f = Facts {
            chat_id: Some(-100),
            user_id: Some(5),
            ..Facts::default()
        };
        assert_eq!(f.ordering_key(), Some(-100_i64 as u64));
        f.chat_id = None;
        assert_eq!(f.ordering_key(), Some(5));
        f.user_id = None;
        assert_eq!(f.ordering_key(), None);
    }
}

//! The Bot API's methods, and sets of them by name and prefix.

use std::cmp::Ordering;

/// Every Bot API method, by its canonical name: Bot API 10.3 (August 24,
/// 2026), as listed at <https://core.telegram.org/bots/api>. Sorted by the
/// ASCII-lowercased name, which is how [`lookup`] searches it.
static METHODS: [&str; 185] = [
    "addStickerToSet",
    "answerCallbackQuery",
    "answerChatJoinRequestQuery",
    "answerGuestQuery",
    "answerInlineQuery",
    "answerPreCheckoutQuery",
    "answerShippingQuery",
    "answerWebAppQuery",
    "approveChatJoinRequest",
    "approveSuggestedPost",
    "banChatMember",
    "banChatSenderChat",
    "close",
    "closeForumTopic",
    "closeGeneralForumTopic",
    "convertGiftToStars",
    "copyMessage",
    "copyMessages",
    "createChatInviteLink",
    "createChatSubscriptionInviteLink",
    "createForumTopic",
    "createInvoiceLink",
    "createNewStickerSet",
    "declineChatJoinRequest",
    "declineSuggestedPost",
    "deleteAllMessageReactions",
    "deleteBusinessMessages",
    "deleteChatPhoto",
    "deleteChatStickerSet",
    "deleteEphemeralMessage",
    "deleteForumTopic",
    "deleteMessage",
    "deleteMessageReaction",
    "deleteMessages",
    "deleteMyCommands",
    "deleteStickerFromSet",
    "deleteStickerSet",
    "deleteStory",
    "deleteWebhook",
    "editChatInviteLink",
    "editChatSubscriptionInviteLink",
    "editEphemeralMessageCaption",
    "editEphemeralMessageMedia",
    "editEphemeralMessageReplyMarkup",
    "editEphemeralMessageText",
    "editForumTopic",
    "editGeneralForumTopic",
    "editMessageCaption",
    "editMessageChecklist",
    "editMessageLiveLocation",
    "editMessageMedia",
    "editMessageReplyMarkup",
    "editMessageText",
    "editStory",
    "editUserStarSubscription",
    "exportChatInviteLink",
    "forwardMessage",
    "forwardMessages",
    "getAvailableGifts",
    "getBusinessAccountGifts",
    "getBusinessAccountStarBalance",
    "getBusinessConnection",
    "getChat",
    "getChatAdministrators",
    "getChatGifts",
    "getChatMember",
    "getChatMemberCount",
    "getChatMenuButton",
    "getCustomEmojiStickers",
    "getFile",
    "getForumTopicIconStickers",
    "getGameHighScores",
    "getManagedBotAccessSettings",
    "getManagedBotToken",
    "getMe",
    "getMyCommands",
    "getMyDefaultAdministratorRights",
    "getMyDescription",
    "getMyName",
    "getMyShortDescription",
    "getMyStarBalance",
    "getStarTransactions",
    "getStickerSet",
    "getUpdates",
    "getUserChatBoosts",
    "getUserGifts",
    "getUserPersonalChatMessages",
    "getUserProfileAudios",
    "getUserProfilePhotos",
    "getWebhookInfo",
    "giftPremiumSubscription",
    "hideGeneralForumTopic",
    "leaveChat",
    "logOut",
    "pinChatMessage",
    "postStory",
    "promoteChatMember",
    "readBusinessMessage",
    "refundStarPayment",
    "removeBusinessAccountProfilePhoto",
    "removeChatVerification",
    "removeMyProfilePhoto",
    "removeUserVerification",
    "reopenForumTopic",
    "reopenGeneralForumTopic",
    "replaceManagedBotToken",
    "replaceStickerInSet",
    "repostStory",
    "restrictChatMember",
    "revokeChatInviteLink",
    "savePreparedInlineMessage",
    "savePreparedKeyboardButton",
    "sendAnimation",
    "sendAudio",
    "sendChatAction",
    "sendChatJoinRequestWebApp",
    "sendChecklist",
    "sendContact",
    "sendDice",
    "sendDocument",
    "sendGame",
    "sendGift",
    "sendInvoice",
    "sendLivePhoto",
    "sendLocation",
    "sendMediaGroup",
    "sendMessage",
    "sendMessageDraft",
    "sendPaidMedia",
    "sendPhoto",
    "sendPoll",
    "sendRichMessage",
    "sendRichMessageDraft",
    "sendSticker",
    "sendVenue",
    "sendVideo",
    "sendVideoNote",
    "sendVoice",
    "setBusinessAccountBio",
    "setBusinessAccountGiftSettings",
    "setBusinessAccountName",
    "setBusinessAccountProfilePhoto",
    "setBusinessAccountUsername",
    "setChatAdministratorCustomTitle",
    "setChatDescription",
    "setChatMemberTag",
    "setChatMenuButton",
    "setChatPermissions",
    "setChatPhoto",
    "setChatStickerSet",
    "setChatTitle",
    "setCustomEmojiStickerSetThumbnail",
    "setGameScore",
    "setManagedBotAccessSettings",
    "setMessageReaction",
    "setMyCommands",
    "setMyDefaultAdministratorRights",
    "setMyDescription",
    "setMyName",
    "setMyProfilePhoto",
    "setMyShortDescription",
    "setPassportDataErrors",
    "setStickerEmojiList",
    "setStickerKeywords",
    "setStickerMaskPosition",
    "setStickerPositionInSet",
    "setStickerSetThumbnail",
    "setStickerSetTitle",
    "setUserEmojiStatus",
    "setWebhook",
    "stopMessageLiveLocation",
    "stopPoll",
    "transferBusinessAccountStars",
    "transferGift",
    "unbanChatMember",
    "unbanChatSenderChat",
    "unhideGeneralForumTopic",
    "unpinAllChatMessages",
    "unpinAllForumTopicMessages",
    "unpinAllGeneralForumTopicMessages",
    "unpinChatMessage",
    "upgradeGift",
    "uploadStickerFile",
    "verifyChat",
    "verifyUser",
];

/// The metric label for a method missing from the table.
pub const OTHER_LABEL: &str = "other";

/// A Bot API method of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodId(u16);

impl MethodId {
    /// The method's canonical camelCase name.
    pub fn label(self) -> &'static str {
        METHODS
            .get(usize::from(self.0))
            .copied()
            .unwrap_or(OTHER_LABEL)
    }
}

/// The Bot API method named `method`, ignoring ASCII case, as Telegram
/// does.
pub fn lookup(method: &str) -> Option<MethodId> {
    let index = METHODS
        .binary_search_by(|name| cmp_ignore_ascii_case(name, method))
        .ok()?;
    u16::try_from(index).ok().map(MethodId)
}

/// `a` and `b` ordered as if ASCII-lowercased.
fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    fn lower(s: &str) -> impl Iterator<Item = u8> + '_ {
        s.bytes().map(|c| c.to_ascii_lowercase())
    }
    lower(a).cmp(lower(b))
}

/// Method names and name prefixes (`send*`), matched ignoring ASCII case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodSet {
    /// Lowercased names.
    exact: Vec<Box<str>>,
    /// Lowercased prefixes, without their `*`.
    prefixes: Vec<Box<str>>,
}

impl MethodSet {
    /// The set of `patterns`: each a method name, or a prefix followed by
    /// one `*`.
    pub fn new(patterns: &[String]) -> MethodSet {
        let mut set = MethodSet::default();
        for pattern in patterns {
            let pattern = pattern.to_ascii_lowercase();
            match pattern.strip_suffix('*') {
                Some(prefix) => set.prefixes.push(prefix.into()),
                None => set.exact.push(pattern.into()),
            }
        }
        set
    }

    /// Whether `method` is one of the names or starts with one of the
    /// prefixes.
    pub fn matches(&self, method: &str) -> bool {
        let bytes = method.as_bytes();
        self.exact
            .iter()
            .any(|name| method.eq_ignore_ascii_case(name))
            || self.prefixes.iter().any(|prefix| {
                bytes
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_lookup_case_insensitive() {
        for (name, label) in [
            ("sendMessage", "sendMessage"),
            ("SENDMESSAGE", "sendMessage"),
            ("sendmessage", "sendMessage"),
            ("getMe", "getMe"),
            ("GetMe", "getMe"),
            ("addStickerToSet", "addStickerToSet"),
            (
                "unpinAllGeneralForumTopicMessages",
                "unpinAllGeneralForumTopicMessages",
            ),
            ("close", "close"),
            ("logOut", "logOut"),
        ] {
            assert_eq!(lookup(name).map(MethodId::label), Some(label), "{name}");
        }
        for unknown in [
            "",
            "sendMessages",
            "sendMessag",
            "send_message",
            "x",
            "zzz",
            "aaa",
        ] {
            assert_eq!(lookup(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn method_table_sorted_unique() {
        // Bot API 10.3 lists 185 methods.
        assert_eq!(METHODS.len(), 185);
        for pair in METHODS.windows(2) {
            assert_eq!(
                cmp_ignore_ascii_case(pair[0], pair[1]),
                Ordering::Less,
                "{} before {}",
                pair[0],
                pair[1]
            );
        }
        for (k, name) in METHODS.iter().enumerate() {
            let id = lookup(name).expect("every method is found");
            assert_eq!(usize::from(id.0), k);
            assert_eq!(id.label(), *name);
            let first = name.as_bytes()[0];
            assert!(first.is_ascii_lowercase(), "{name}");
            assert!(name.bytes().all(|c| c.is_ascii_alphanumeric()), "{name}");
        }
    }

    /// The methods that send or forward messages (Bot API 10.3).
    #[test]
    fn send_methods_present() {
        const SEND: [&str; 25] = [
            "sendMessage",
            "forwardMessage",
            "forwardMessages",
            "copyMessage",
            "copyMessages",
            "sendPhoto",
            "sendLivePhoto",
            "sendAudio",
            "sendDocument",
            "sendVideo",
            "sendAnimation",
            "sendVoice",
            "sendVideoNote",
            "sendPaidMedia",
            "sendMediaGroup",
            "sendLocation",
            "sendVenue",
            "sendContact",
            "sendPoll",
            "sendChecklist",
            "sendDice",
            "sendSticker",
            "sendRichMessage",
            "sendInvoice",
            "sendGame",
        ];
        for name in SEND {
            assert_eq!(lookup(name).map(MethodId::label), Some(name), "{name}");
        }
    }

    #[test]
    fn method_set_globs() {
        let set = MethodSet::new(&["send*".to_owned(), "copyMessage".to_owned()]);
        for yes in [
            "sendMessage",
            "SENDPHOTO",
            "send",
            "copyMessage",
            "COPYMESSAGE",
        ] {
            assert!(set.matches(yes), "{yes}");
        }
        for no in [
            "resend",
            "copyMessages",
            "copyMessag",
            "forwardMessage",
            "sen",
            "",
        ] {
            assert!(!set.matches(no), "{no}");
        }

        let exact = MethodSet::new(&["sendMessage".to_owned()]);
        assert!(exact.matches("sendMessage"));
        assert!(exact.matches("SendMessage"));
        assert!(!exact.matches("sendMessageDraft"));
        assert!(!exact.matches("sendPhoto"));

        // Patterns are matched ignoring case too.
        let upper = MethodSet::new(&["SEND*".to_owned(), "GETME".to_owned()]);
        assert!(upper.matches("sendDice"));
        assert!(upper.matches("getMe"));

        let all = MethodSet::new(&["*".to_owned()]);
        assert!(all.matches("getMe"));
        assert!(!MethodSet::new(&[]).matches("getMe"));
    }
}

//! The RPC allowlist of an account's clients: which TL methods a client may
//! call through the account.
//!
//! A glob is one of:
//! - `*`: every method a glob may grant;
//! - `<namespace>.*`, such as `messages.*`: every method of the namespace
//!   a glob may grant;
//! - a method name, such as `messages.sendMessage` or `ping`: that method.
//!
//! Names are TL names, `[A-Za-z][A-Za-z0-9_]*` with at most one
//! `namespace.` before them, and match case-sensitively. Nothing else is a
//! glob: no `*` inside a name, no `?`, no character classes.
//!
//! A client may call a method when an `allow` glob matches it and no `deny`
//! glob does; with no `allow`, it may call nothing. A glob grants only
//! methods of the [`GLOB_NAMESPACES`] that are not [`ACCOUNT_CHANGING`]
//! (apart from [`ACCOUNT_READ_ONLY`]): every other method, whatever its
//! namespace, is granted only by an `allow` entry that names it exactly
//! ([`exact_only`]). A namespace a later layer adds is exact-only until it
//! is reviewed.

/// The namespaces whose methods a glob (`*` or `<namespace>.*`) may grant,
/// reviewed for layer 230: what a userbot does with chats, messages,
/// media and the profile it reads. Every other namespace, such as `auth`,
/// `bots`, `payments`, `wallet`, `premium`, `fragment` or `smsjobs`, moves
/// assets, logs in, or is unreviewed, and needs exact listing.
pub const GLOB_NAMESPACES: [&str; 13] = [
    "account",  // Settings; a glob grants only `ACCOUNT_READ_ONLY`'s getters.
    "channels", // Channels and supergroups the account uses or administers.
    "contacts", // Contacts, less the address-book and block-list changes.
    "help",     // Configuration and app information.
    "langpack", // Localization strings.
    "messages", // Chats and messages, less the methods of `ACCOUNT_CHANGING`.
    "phone",    // Calls and group calls.
    "photos",   // Profile photos, less the own and contacts' photo changes.
    "stats",    // Channel and message statistics, read-only.
    "stickers", // Sticker sets the account creates and edits.
    "stories",  // Stories, less the account-wide toggle.
    "upload",   // File upload and download.
    "users",    // User profiles, read-only.
];

/// Methods of the [`GLOB_NAMESPACES`] that move assets or change the
/// account itself: its profile, settings, privacy, sessions, password,
/// address book, block list, chat folders and logins on other sites. A
/// `<namespace>.*` entry covers the whole namespace, apart from the
/// getters of [`ACCOUNT_READ_ONLY`].
///
/// A glob never allows these, `*` included: an operator lists each one
/// exactly in a client's `allow`, and the loader warns about it.
pub const ACCOUNT_CHANGING: [&str; 53] = [
    "account.*",
    // Gives another user admin rights in, or ownership of, the owner's
    // chats.
    "channels.editAdmin",
    // Sets the price of messages to the channel, in Stars.
    "channels.updatePaidMessagesPrice",
    "contacts.acceptContact",
    "contacts.addContact",
    "contacts.block",
    "contacts.blockFromReplies",
    "contacts.deleteByPhones",
    "contacts.deleteContacts",
    "contacts.editCloseFriends",
    // With `self_expires`, publishes the account's location.
    "contacts.getLocated",
    "contacts.importContacts",
    "contacts.resetSaved",
    "contacts.resetTopPeerRating",
    "contacts.setBlocked",
    "contacts.toggleTopPeers",
    "contacts.unblock",
    "contacts.updateContactNote",
    "help.acceptTermsOfService",
    // Logs the account in to a website, or asks to; a login can complete
    // on `requestUrlAuth`'s answer, with no further call.
    "messages.acceptUrlAuth",
    "messages.checkUrlAuthMatchCode",
    "messages.requestUrlAuth",
    // Account-wide deletes.
    "messages.clearAllDrafts",
    "messages.deletePhoneCallHistory",
    // Give another user admin rights in, or ownership of, the owner's
    // chats.
    "messages.editChatAdmin",
    "messages.editChatCreator",
    // The account's quick replies, a Telegram Business feature: shared by
    // every chat, and sent as the account.
    "messages.checkQuickReplyShortcut",
    "messages.deleteQuickReplyMessages",
    "messages.deleteQuickReplyShortcut",
    "messages.editQuickReplyShortcut",
    "messages.getQuickReplies",
    "messages.getQuickReplyMessages",
    "messages.reorderQuickReplies",
    "messages.sendQuickReplyMessages",
    // Web views hand a bot the account's identity, and may pay.
    "messages.prolongWebView",
    "messages.requestAppWebView",
    "messages.requestChatJoinWebView",
    "messages.requestMainWebView",
    "messages.requestSimpleWebView",
    "messages.requestWebView",
    // Spends Stars.
    "messages.sendPaidReaction",
    // Account-wide defaults.
    "messages.setDefaultHistoryTTL",
    "messages.setDefaultReaction",
    // Installs a bot in the attachment menu, with write access.
    "messages.toggleBotInAttachMenu",
    // Approves a paid suggested post, which moves its price.
    "messages.toggleSuggestedPostApproval",
    // Chat folders.
    "messages.toggleDialogFilterTags",
    "messages.updateDialogFilter",
    "messages.updateDialogFiltersOrder",
    "photos.deletePhotos",
    "photos.updateProfilePhoto",
    "photos.uploadContactProfilePhoto",
    "photos.uploadProfilePhoto",
    "stories.toggleAllStoriesHidden",
];

/// The `account.get*` methods of layer 230 that only read settings, which
/// `account.*` or `*` grants like any other method.
///
/// An explicit list rather than a rule that every `account.get*` is
/// read-only: a getter added in a later layer stays protected until it is
/// reviewed, and some getters are not harmless. Left out on purpose:
/// `account.getTmpPassword` (creates a temporary password),
/// `account.getPassword` and `account.getPasswordSettings` (2FA data),
/// `account.getPasskeys`, and the Telegram Passport getters
/// `account.getAllSecureValues`, `account.getSecureValue` and
/// `account.getAuthorizationForm`.
pub const ACCOUNT_READ_ONLY: [&str; 34] = [
    "account.getAccountTTL",
    "account.getAuthorizations",
    "account.getAutoDownloadSettings",
    "account.getAutoSaveSettings",
    "account.getBotBusinessConnection",
    "account.getBusinessChatLinks",
    "account.getChannelDefaultEmojiStatuses",
    "account.getChannelRestrictedStatusEmojis",
    "account.getChatThemes",
    "account.getCollectibleEmojiStatuses",
    "account.getConnectedBots",
    "account.getContactSignUpNotification",
    "account.getContentSettings",
    "account.getDefaultBackgroundEmojis",
    "account.getDefaultEmojiStatuses",
    "account.getDefaultGroupPhotoEmojis",
    "account.getDefaultProfilePhotoEmojis",
    "account.getGlobalPrivacySettings",
    "account.getMultiWallPapers",
    "account.getNotifyExceptions",
    "account.getNotifySettings",
    "account.getPaidMessagesRevenue",
    "account.getPrivacy",
    "account.getReactionsNotifySettings",
    "account.getRecentEmojiStatuses",
    "account.getSavedMusicIds",
    "account.getSavedRingtones",
    "account.getTheme",
    "account.getThemes",
    "account.getUniqueGiftChatThemes",
    "account.getWallPaper",
    "account.getWallPapers",
    "account.getWebAuthorizations",
    "account.getWebBrowserSettings",
];

/// One `allow` or `deny` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MethodGlob {
    /// `*`.
    Any,
    /// `<namespace>.*`, holding the namespace.
    Namespace(String),
    /// A method name.
    Exact(String),
}

impl MethodGlob {
    /// Parses a glob, if `text` is one.
    pub fn parse(text: &str) -> Option<MethodGlob> {
        if text == "*" {
            return Some(MethodGlob::Any);
        }
        if let Some(namespace) = text.strip_suffix(".*") {
            return ident(namespace).then(|| MethodGlob::Namespace(namespace.to_owned()));
        }
        let valid = match text.split_once('.') {
            Some((namespace, name)) => ident(namespace) && ident(name),
            None => ident(text),
        };
        valid.then(|| MethodGlob::Exact(text.to_owned()))
    }

    /// Whether the glob matches `method`.
    pub fn matches(&self, method: &str) -> bool {
        match self {
            MethodGlob::Any => true,
            MethodGlob::Namespace(namespace) => in_namespace(method, namespace),
            MethodGlob::Exact(name) => name == method,
        }
    }
}

/// Whether `method` is one of [`ACCOUNT_CHANGING`] and not of
/// [`ACCOUNT_READ_ONLY`]. It compares against the constants in place, so
/// a call allocates nothing.
pub fn account_changing(method: &str) -> bool {
    let listed = ACCOUNT_CHANGING
        .iter()
        .any(|entry| match entry.strip_suffix(".*") {
            Some(namespace) => in_namespace(method, namespace),
            None => *entry == method,
        });
    listed && !ACCOUNT_READ_ONLY.contains(&method)
}

/// Whether a glob may grant `namespace`'s methods.
pub fn glob_namespace(namespace: &str) -> bool {
    GLOB_NAMESPACES.contains(&namespace)
}

/// Whether only an exact `allow` entry grants `method`: it has no
/// namespace, its namespace is not one of [`GLOB_NAMESPACES`], or it is
/// [`account_changing`]. Allocates nothing.
pub fn exact_only(method: &str) -> bool {
    match method.split_once('.') {
        Some((namespace, _)) => !glob_namespace(namespace) || account_changing(method),
        None => true,
    }
}

/// Whether `method` is `<namespace>.<name>`, with a name.
fn in_namespace(method: &str, namespace: &str) -> bool {
    method
        .strip_prefix(namespace)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|name| !name.is_empty())
}

/// A client's `allow` and `deny` globs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodAcl {
    pub allow: Vec<MethodGlob>,
    pub deny: Vec<MethodGlob>,
}

impl MethodAcl {
    /// Whether the client may call `method`: deny wins, and an
    /// [`exact_only`] method needs an exact `allow` entry.
    pub fn allows(&self, method: &str) -> bool {
        if self.deny.iter().any(|glob| glob.matches(method)) {
            return false;
        }
        if exact_only(method) {
            return self
                .allow
                .iter()
                .any(|glob| matches!(glob, MethodGlob::Exact(name) if name == method));
        }
        self.allow.iter().any(|glob| glob.matches(method))
    }
}

/// `[A-Za-z][A-Za-z0-9_]*`.
fn ident(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|c| c.is_ascii_alphabetic())
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

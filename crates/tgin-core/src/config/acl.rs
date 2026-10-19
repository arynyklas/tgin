//! The RPC allowlist of an account's clients: which TL methods a client may
//! call through the account.
//!
//! A glob is one of:
//! - `*`: every method;
//! - `<namespace>.*`, such as `messages.*`: every method of the namespace;
//! - a method name, such as `messages.sendMessage` or `ping`: that method.
//!
//! Names are TL names, `[A-Za-z][A-Za-z0-9_]*` with at most one
//! `namespace.` before them, and match case-sensitively. Nothing else is a
//! glob: no `*` inside a name, no `?`, no character classes.
//!
//! A client may call a method when an `allow` glob matches it and no `deny`
//! glob does; with no `allow`, it may call nothing. The methods of
//! [`ACCOUNT_CHANGING`] are the exception: only an `allow` entry that names
//! the method exactly allows one, never a glob.

/// Methods that log the account in or out, or change the account itself:
/// its profile, settings, privacy, sessions, password or saved contacts.
/// A `<namespace>.*` entry covers the whole namespace.
///
/// A glob never allows these, `*` included: an operator lists each one
/// exactly in a client's `allow`, and the loader warns about it.
pub const ACCOUNT_CHANGING: [&str; 8] = [
    "auth.*",
    "account.*",
    "contacts.resetSaved",
    "contacts.toggleTopPeers",
    "photos.uploadProfilePhoto",
    "photos.updateProfilePhoto",
    "photos.deletePhotos",
    "help.acceptTermsOfService",
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
            MethodGlob::Namespace(namespace) => method
                .strip_prefix(namespace.as_str())
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|name| !name.is_empty()),
            MethodGlob::Exact(name) => name == method,
        }
    }
}

/// Whether `method` is one of [`ACCOUNT_CHANGING`].
pub fn account_changing(method: &str) -> bool {
    ACCOUNT_CHANGING
        .iter()
        .filter_map(|entry| MethodGlob::parse(entry))
        .any(|glob| glob.matches(method))
}

/// A client's `allow` and `deny` globs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodAcl {
    pub allow: Vec<MethodGlob>,
    pub deny: Vec<MethodGlob>,
}

impl MethodAcl {
    /// Whether the client may call `method`: deny wins, and an
    /// [`ACCOUNT_CHANGING`] method needs an exact `allow` entry.
    pub fn allows(&self, method: &str) -> bool {
        if self.deny.iter().any(|glob| glob.matches(method)) {
            return false;
        }
        if account_changing(method) {
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

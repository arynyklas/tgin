//! The session store's errors.

/// Why a [`SessionStore`](crate::SessionStore) call failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Another process holds the session directory's `lock`. `account` is
    /// the directory's last component until the caller names the account
    /// with [`SessionError::for_account`].
    #[error("account `{account}` is held by another process")]
    Held { account: String },
    /// A self user is recorded, so the home DC's auth key is the logged-in
    /// authorization: replacing it would silently log the account out.
    #[error("refusing to replace the auth key of home DC {dc}: the account is logged in")]
    KeyReplaceRefused { dc: i32 },
    /// The home DC's key goes through `set_home_key`, never through
    /// `set_media_key`.
    #[error("DC {dc} is the home DC; its key is set by set_home_key")]
    HomeDc { dc: i32 },
    #[error("session I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("session store: {0}")]
    Db(#[from] redb::Error),
    /// A stored value has the wrong shape; names the value.
    #[error("session store has a corrupt {0}")]
    Corrupt(&'static str),
    /// Test-only: the injected crash refused a commit.
    #[cfg(test)]
    #[error("injected crash before commit")]
    Crashed,
}

impl SessionError {
    /// Names the account in a [`SessionError::Held`]; other errors pass
    /// through.
    #[must_use]
    pub fn for_account(self, name: &str) -> Self {
        match self {
            Self::Held { .. } => Self::Held {
                account: name.to_owned(),
            },
            other => other,
        }
    }
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for SessionError {
            fn from(e: $t) -> Self {
                Self::Db(e.into())
            }
        }
    )*};
}
from_redb!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

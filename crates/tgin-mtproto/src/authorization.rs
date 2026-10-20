//! Whether Telegram says a key's authorization is gone: the one rule the
//! account's sessions (main and media) and the login share.
//!
//! An authorization is gone when Telegram answers
//! - any 401 except `SESSION_PASSWORD_NEEDED` (`AUTH_KEY_UNREGISTERED`,
//!   `SESSION_REVOKED`, `AUTH_KEY_INVALID`, `SESSION_EXPIRED`…);
//!   `SESSION_PASSWORD_NEEDED` belongs to a login waiting for its 2FA
//!   password, never to a logged-in key;
//! - 406 `AUTH_KEY_DUPLICATED`: the key was used from two places at once
//!   (a copied session directory, say) and Telegram dropped it;
//! - a transport −404: Telegram has no such key;
//! - an init call refused with any of these: every request on that sender
//!   is refused the same way.
//!
//! None of these was executed, so the request is safe to send again on a
//! new authorization.

use crate::{RpcOutcome, SenderError};

/// Telegram's `rpc_error` says the key's authorization is gone.
pub(crate) fn gone(code: i32, message: &str) -> bool {
    (code == 401 && message != "SESSION_PASSWORD_NEEDED") || message == "AUTH_KEY_DUPLICATED"
}

/// A sender's failure says the key's authorization is gone: transport
/// −404, or an init call refused with such an error.
pub(crate) fn sender_gone(e: &SenderError) -> bool {
    match e {
        SenderError::KeyRevoked | SenderError::KeyUnknown => true,
        SenderError::InitRefused { code, message } => gone(*code, message),
        _ => false,
    }
}

/// A call's outcome says the key's authorization is gone.
pub(crate) fn outcome_gone(outcome: &Result<RpcOutcome, SenderError>) -> bool {
    match outcome {
        Ok(RpcOutcome::Error { code, message }) => gone(*code, message),
        Ok(RpcOutcome::Result(_)) => false,
        Err(e) => sender_gone(e),
    }
}

/// Telegram says the key holds no login: its authorization is gone, or a
/// login on it still waits for the 2FA password (401
/// `SESSION_PASSWORD_NEEDED`). For a key whose login may never have
/// finished.
pub(crate) fn no_login(code: i32, message: &str) -> bool {
    gone(code, message) || (code == 401 && message == "SESSION_PASSWORD_NEEDED")
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn error(code: i32, message: &str) -> Result<RpcOutcome, SenderError> {
        Ok(RpcOutcome::Error {
            code,
            message: message.to_owned(),
        })
    }

    fn refused(code: i32, message: &str) -> SenderError {
        SenderError::InitRefused {
            code,
            message: message.to_owned(),
        }
    }

    #[test]
    fn gone_is_any_401_but_the_2fa_step_or_a_duplicated_key() {
        for (code, message) in [
            (401, "AUTH_KEY_UNREGISTERED"),
            (401, "SESSION_REVOKED"),
            (401, "AUTH_KEY_INVALID"),
            (401, "SESSION_EXPIRED"),
            (401, "USER_DEACTIVATED"),
            (406, "AUTH_KEY_DUPLICATED"),
        ] {
            assert!(gone(code, message), "{code} {message}");
            assert!(no_login(code, message), "{code} {message}");
            assert!(outcome_gone(&error(code, message)), "{code} {message}");
            assert!(sender_gone(&refused(code, message)), "{code} {message}");
            assert!(
                outcome_gone(&Err(refused(code, message))),
                "{code} {message}"
            );
        }
        for (code, message) in [
            (401, "SESSION_PASSWORD_NEEDED"),
            (406, "PHONE_NUMBER_INVALID"),
            (400, "AUTH_KEY_UNREGISTERED_NOT"),
            (420, "FLOOD_WAIT_30"),
            (500, "INTERNAL"),
            (303, "USER_MIGRATE_4"),
        ] {
            assert!(!gone(code, message), "{code} {message}");
            assert!(!outcome_gone(&error(code, message)), "{code} {message}");
            assert!(!sender_gone(&refused(code, message)), "{code} {message}");
        }
        // A login waiting for its password holds no login, but its key is
        // not gone.
        assert!(no_login(401, "SESSION_PASSWORD_NEEDED"));
        assert!(!no_login(406, "PHONE_NUMBER_INVALID"));
    }

    #[test]
    fn sender_gone_is_minus_404_or_such_an_init_refusal() {
        assert!(sender_gone(&SenderError::KeyRevoked));
        assert!(sender_gone(&SenderError::KeyUnknown));
        for e in [
            SenderError::NotSent,
            SenderError::NoAnswer,
            SenderError::BadRequest,
            SenderError::BadMessage { code: 16 },
            refused(420, "FLOOD_WAIT_9"),
        ] {
            assert!(!sender_gone(&e), "{e}");
        }
        assert!(!outcome_gone(&Ok(RpcOutcome::Result(Bytes::from_static(
            &[0; 4]
        )))));
    }
}

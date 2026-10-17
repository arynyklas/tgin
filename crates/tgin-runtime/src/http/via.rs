//! Loop detection with `Via` (RFC 9110 §7.6.3): a process names itself with
//! a random pseudonym, adds `Via: 1.1 <pseudonym>` to the requests it sends,
//! and refuses a request it sent itself when one comes back to it
//! ([`refuse_loops`]).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::VIA;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use parking_lot::Mutex;
use tokio::time::Instant;

/// The header that names the bot a push came from.
const SOURCE_HEADER: &str = "x-tgin-source";
/// The longest source name a refusal logs.
const MAX_LOGGED_SOURCE: usize = 64;
/// At most one warning per this long.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// This process's `Via` pseudonym, `tgin-<16 hex digits>`, random per
/// process, so another process (a chained instance) has another one.
#[derive(Clone, Debug)]
pub struct Via {
    pseudonym: Arc<str>,
    header: HeaderValue,
}

impl Via {
    /// A new random pseudonym.
    pub fn generate() -> Via {
        Via::named(&format!("tgin-{:016x}", rand::random::<u64>()))
    }

    /// The pseudonym `pseudonym`, which must be a valid header token.
    fn named(pseudonym: &str) -> Via {
        let header = HeaderValue::from_str(&format!("1.1 {pseudonym}"))
            .unwrap_or_else(|_| HeaderValue::from_static("1.1 tgin"));
        Via {
            pseudonym: Arc::from(pseudonym),
            header,
        }
    }

    pub fn pseudonym(&self) -> &str {
        &self.pseudonym
    }

    /// `1.1 <pseudonym>`, the value to add as a `Via` header.
    pub fn header_value(&self) -> &HeaderValue {
        &self.header
    }

    /// Whether any `Via` header of `headers` lists this process as an
    /// intermediary: an element `<protocol> <received-by> [comment]` whose
    /// `received-by` is the pseudonym.
    pub fn seen_in(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(VIA)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|element| element.split_whitespace().nth(1) == Some(&*self.pseudonym))
    }
}

/// `router`, refusing every request whose `Via` names `via` with
/// `refusal()`, `via`'s own `Via` added: a request this process sent came
/// back to it, and the sender tells its own refusal by that header. A
/// warning, at most one a minute, names `plane` and the bot from
/// `X-Tgin-Source`; never a path or a URL.
pub fn refuse_loops(
    router: Router,
    via: Via,
    plane: &'static str,
    refusal: fn() -> Response,
) -> Router {
    let guard = Arc::new(LoopGuard {
        via,
        plane,
        refusal,
        warned: Mutex::new(None),
    });
    router.layer(middleware::from_fn_with_state(guard, refuse))
}

struct LoopGuard {
    via: Via,
    plane: &'static str,
    refusal: fn() -> Response,
    /// When the last warning was logged.
    warned: Mutex<Option<Instant>>,
}

async fn refuse(State(guard): State<Arc<LoopGuard>>, req: Request, next: Next) -> Response {
    if !guard.via.seen_in(req.headers()) {
        return next.run(req).await;
    }
    let now = Instant::now();
    let warn = {
        let mut warned = guard.warned.lock();
        let due = warned.is_none_or(|at| now.saturating_duration_since(at) >= WARN_EVERY);
        if due {
            *warned = Some(now);
        }
        due
    };
    if warn {
        let source = req
            .headers()
            .get(SOURCE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| &s[..s.len().min(MAX_LOGGED_SOURCE)]);
        tracing::warn!(
            plane = guard.plane,
            source,
            "request refused: it is a push from this tgin coming back (Via loop); \
             a push endpoint points at tgin itself"
        );
    }
    // Signed with this process's `Via`, so the pushing dispatcher can tell
    // its own refusal from another server's 508.
    let mut response = (guard.refusal)();
    response
        .headers_mut()
        .insert(VIA, guard.via.header_value().clone());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudonyms_are_random_and_well_formed() {
        let a = Via::generate();
        let b = Via::generate();
        assert_ne!(a.pseudonym(), b.pseudonym());
        let hex = a.pseudonym().strip_prefix("tgin-").expect("prefix");
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            a.header_value().to_str().expect("ASCII"),
            format!("1.1 {}", a.pseudonym())
        );
    }

    #[test]
    fn seen_in_finds_own_element_only() {
        let me = Via::named("tgin-00000000000000aa");
        let with = |values: &[&'static str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(VIA, HeaderValue::from_static(value));
            }
            me.seen_in(&headers)
        };
        assert!(with(&["1.1 tgin-00000000000000aa"]));
        assert!(with(&["1.0 fred, 1.1 tgin-00000000000000aa (tgin)"]));
        assert!(with(&["1.1 proxy", "HTTP/1.1  tgin-00000000000000aa"]));
        assert!(!with(&[]));
        assert!(!with(&["1.1 tgin-00000000000000ab"]));
        assert!(!with(&["1.1 tgin-00000000000000aa0"]));
        assert!(!with(&["tgin-00000000000000aa"]), "no received-by");
        assert!(!with(&["1.1 proxy (tgin-00000000000000aa)"]), "a comment");
    }
}

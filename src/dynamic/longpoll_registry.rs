//! Process-global registry mapping `getUpdates` paths to their
//! [`LongPollRoute`] for the dynamic axum fallback handler.
//!
//! Backed by [`arc_swap::ArcSwap`]: reads on the hot path (every poll from
//! a downstream bot) are a single atomic load + `Arc` clone, with no lock
//! and no poisoning. Writes (`add_route` / `remove_route` from the API)
//! build a new map and publish it via RCU.
//!
//! This module is a workaround for `axum::Router` being fixed at startup.
//! Once the LB tree owns its own routes and the dynamic-dispatch fallback
//! is no longer needed, this whole module is expected to disappear — keep
//! the surface area small.

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

use crate::route::longpull::LongPollRoute;

type Registry = HashMap<String, Arc<LongPollRoute>>;

static LONGPOLL_REGISTRY: Lazy<ArcSwap<Registry>> =
    Lazy::new(|| ArcSwap::from_pointee(Registry::new()));

/// Look up a long-poll route by its `getUpdates` path.
pub fn get(path: &str) -> Option<Arc<LongPollRoute>> {
    LONGPOLL_REGISTRY.load().get(path).cloned()
}

/// Register `route` under `path`, replacing any previous entry. Cannot fail.
pub fn insert(path: String, route: Arc<LongPollRoute>) {
    LONGPOLL_REGISTRY.rcu(|cur| {
        let mut next = Registry::clone(cur);
        next.insert(path.clone(), route.clone());
        Arc::new(next)
    });
}

/// Remove the entry for `path` if present. Cannot fail.
pub fn remove(path: &str) {
    LONGPOLL_REGISTRY.rcu(|cur| {
        let mut next = Registry::clone(cur);
        next.remove(path);
        Arc::new(next)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global; these tests use distinct path keys
    // so they don't interfere with each other or with parallel LB tests.

    #[test]
    fn insert_then_get_returns_route() {
        let path = "/__registry_test_insert/getUpdates".to_string();
        let route = Arc::new(LongPollRoute::new(path.clone()));

        insert(path.clone(), route.clone());

        let got = get(&path).expect("route present after insert");
        assert!(Arc::ptr_eq(&got, &route));

        remove(&path);
    }

    #[test]
    fn remove_drops_entry() {
        let path = "/__registry_test_remove/getUpdates".to_string();
        let route = Arc::new(LongPollRoute::new(path.clone()));

        insert(path.clone(), route);
        assert!(get(&path).is_some());

        remove(&path);
        assert!(get(&path).is_none());
    }

    #[test]
    fn get_missing_returns_none() {
        assert!(get("/__registry_test_missing/getUpdates").is_none());
    }
}

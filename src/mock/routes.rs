#![cfg(test)]

use crate::base::{Printable, Routeable, Serverable};
use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct MockCallsRoute {
    pub id: String,
    /// Raw wire bytes captured at every `process` call. Tests that need
    /// structured access parse via [`MockCallsRoute::get_calls_as_values`].
    pub calls: Arc<Mutex<Vec<Bytes>>>,
}

impl MockCallsRoute {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            calls: Arc::new(Mutex::new(vec![])),
        }
    }

    pub async fn count(&self) -> usize {
        self.calls.lock().await.len()
    }

    pub async fn get_calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .await
            .iter()
            .map(|b| serde_json::from_slice(b).expect("MockCallsRoute received non-JSON payload"))
            .collect()
    }
}

#[async_trait]
impl Routeable for MockCallsRoute {
    async fn process(&self, update: Bytes) {
        // Capture the bytes verbatim so tests assert against the same wire
        // payload that downstreams would receive.
        self.calls.lock().await.push(update);
    }
}

#[async_trait]
impl Printable for MockCallsRoute {
    async fn print(&self) -> String {
        format!("MockRoute({})", self.id)
    }
    async fn json_struct(&self) -> Value {
        json!({ "type": "mock", "id": self.id })
    }
}

#[async_trait]
impl Serverable for MockCallsRoute {}

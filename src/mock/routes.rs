#![cfg(test)]

use crate::base::{Printable, Routeable, Serverable};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct MockCallsRoute {
    pub id: String,
    pub calls: Arc<Mutex<Vec<Value>>>,
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
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl Routeable for MockCallsRoute {
    async fn process(&self, update: Arc<Value>) {
        // Tests inspect captured Values; clone the inner Value once on
        // capture so the recorded history is independent of the Arc's
        // ownership chain.
        self.calls.lock().await.push((*update).clone());
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

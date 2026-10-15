//! Per-component readiness behind `GET /readyz`.

use std::collections::BTreeMap;

use parking_lot::RwLock;

/// Whether one component can do its work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// Not ready, with a reason for operators.
    NotReady(String),
}

/// Readiness of every registered component, keyed by component name.
#[derive(Debug, Default)]
pub struct Health {
    components: RwLock<BTreeMap<String, Readiness>>,
}

impl Health {
    /// Records the readiness of `component`, registering it on first use.
    pub fn set(&self, component: &str, r: Readiness) {
        let mut components = self.components.write();
        match components.get_mut(component) {
            Some(current) => *current = r,
            None => {
                components.insert(component.to_owned(), r);
            }
        }
    }

    /// Stops tracking `component`.
    pub fn remove(&self, component: &str) {
        self.components.write().remove(component);
    }

    /// Whether every registered component is ready (vacuously true when none
    /// is), with each component's readiness sorted by name.
    pub fn ready(&self) -> (bool, Vec<(String, Readiness)>) {
        let components = self.components.read();
        let ready = components.values().all(|r| *r == Readiness::Ready);
        let list = components
            .iter()
            .map(|(name, r)| (name.clone(), r.clone()))
            .collect();
        (ready, list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ready_only_when_all_ready() {
        let health = Health::default();
        health.set("ingress", Readiness::Ready);
        health.set("egress", Readiness::NotReady("starting".to_owned()));

        let (ready, components) = health.ready();
        assert!(!ready);
        assert_eq!(
            components,
            vec![
                (
                    "egress".to_owned(),
                    Readiness::NotReady("starting".to_owned())
                ),
                ("ingress".to_owned(), Readiness::Ready),
            ]
        );

        health.set("egress", Readiness::Ready);
        assert!(health.ready().0);

        health.set("api", Readiness::NotReady("down".to_owned()));
        assert!(!health.ready().0);
        health.remove("api");
        assert_eq!(
            health.ready(),
            (
                true,
                vec![
                    ("egress".to_owned(), Readiness::Ready),
                    ("ingress".to_owned(), Readiness::Ready),
                ]
            )
        );
    }
}

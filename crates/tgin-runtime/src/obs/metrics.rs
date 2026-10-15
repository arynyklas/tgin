//! The OpenMetrics registry behind `GET /metrics`.

use prometheus_client::encoding::text;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

/// `Content-Type` of the text that [`MetricsRegistry::encode`] produces.
pub const METRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Every metric of the process, named with the `tgin_` prefix. Built and
/// filled at startup; afterwards only the registered metrics change.
#[derive(Debug)]
pub struct MetricsRegistry {
    registry: Registry,
}

impl MetricsRegistry {
    /// Creates the registry with `tgin_build_info`, which carries the
    /// `version` and `commit` labels.
    pub fn new(version: &str, commit: &str) -> Self {
        let mut registry = Registry::with_prefix("tgin");
        registry.register(
            "build",
            "Version and commit of the running build",
            Info::new([
                ("version", version.to_owned()),
                ("commit", commit.to_owned()),
            ]),
        );
        Self { registry }
    }

    /// The underlying registry, for registering metrics at startup.
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Renders every metric in the OpenMetrics text format, ending with
    /// `# EOF`.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing to a `String` cannot fail, and prometheus-client's metric
        // types fail only when the writer does, so this reports a custom
        // metric whose encoder fails. The partial output then lacks `# EOF`,
        // which scrapers reject.
        if let Err(err) = text::encode(&mut out, &self.registry) {
            tracing::error!(%err, "encoding the metrics failed");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_encoded() {
        let encoded = MetricsRegistry::new("1.2.3", "abc1234").encode();
        assert!(
            encoded.contains("# TYPE tgin_build info\n"),
            "missing type line: {encoded}"
        );
        assert!(
            encoded.contains("tgin_build_info{version=\"1.2.3\",commit=\"abc1234\"} 1\n"),
            "missing sample: {encoded}"
        );
        assert!(encoded.ends_with("# EOF\n"), "missing EOF: {encoded}");
    }

    #[test]
    fn content_type_constant() {
        assert_eq!(
            METRICS_CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8"
        );
    }
}

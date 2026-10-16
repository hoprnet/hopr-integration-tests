//! Optional span export for the test process, which runs the edgli client.
//!
//! Off unless `HOPRD_USE_OPENTELEMETRY` is on (`true`, `1`, `yes`, `on`), the same switch that
//! turns on span export for the cluster nodes (see `node_telemetry_env` in `cluster.rs`). When on,
//! the process gets a logger that writes the same text output as `#[test_log::test]` (filtered by
//! `RUST_LOG`, through the test framework's capture) and also sends spans over OTLP/HTTP to
//! `HOPRD_OTLP_ENDPOINT` (default `http://localhost:4318`). Every span carries `run_id` and
//! `component=edgli`.
//!
//! The logger is installed when the process starts, before any test runs, so the
//! `#[test_log::test]` setup that each test does afterwards finds a logger already in place and
//! leaves it. Spans still waiting are sent when the process exits.

use std::sync::OnceLock;

use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// Default collector, the same one localcluster gives the nodes.
const DEFAULT_ENDPOINT: &str = "http://localhost:4318";

static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Whether a `HOPRD_USE_OPENTELEMETRY` value turns span export on.
pub(crate) fn export_requested(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The OTLP/HTTP traces URL for a collector endpoint: `<endpoint>/v1/traces`, unless already given.
pub(crate) fn traces_url(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return None;
    }
    Some(if endpoint.ends_with("/v1/traces") {
        endpoint.to_string()
    } else {
        format!("{endpoint}/v1/traces")
    })
}

#[ctor::ctor(unsafe)]
fn install() {
    if !export_requested(std::env::var("HOPRD_USE_OPENTELEMETRY").ok().as_deref()) {
        return;
    }
    if let Err(error) = try_install() {
        eprintln!("client span export is off: {error:#}");
    }
}

fn try_install() -> anyhow::Result<()> {
    let endpoint = std::env::var("HOPRD_OTLP_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into());
    let url = traces_url(&endpoint).ok_or_else(|| {
        anyhow::anyhow!("HOPRD_OTLP_ENDPOINT '{endpoint}' is not http(s); the client exports over OTLP/HTTP only")
    })?;

    // One id for the whole process: the cluster labels its nodes with the same `HOPR_RUN_ID`.
    let run_id = match std::env::var("HOPR_RUN_ID") {
        Ok(id) if !id.trim().is_empty() => id,
        _ => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let id = format!("it-{secs}-{}", std::process::id());
            // SAFETY: runs before `main`, while the process has a single thread.
            unsafe { std::env::set_var("HOPR_RUN_ID", &id) };
            id
        }
    };

    // A blocking HTTP client: the exporter runs on its own thread, and the last spans are sent at
    // exit, after the async runtime is gone. Without it the exporter would pick the async client
    // that other crates in the build turn on, which needs a Tokio runtime.
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(reqwest::blocking::Client::new())
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&url)
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name("edgli")
                .with_attributes([
                    KeyValue::new("run_id", run_id.clone()),
                    KeyValue::new("component", "edgli"),
                ])
                .build(),
        )
        .build();
    let tracer = provider.tracer("hoprd-integration-test");

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()?;
    let _ = PROVIDER.set(provider);
    eprintln!("client spans go to {url} (run_id={run_id})");
    Ok(())
}

/// Sends the spans still waiting when the process exits.
#[dtor::dtor(unsafe)]
fn flush() {
    if let Some(provider) = PROVIDER.get()
        && let Err(error) = provider.shutdown()
    {
        eprintln!("client span export: could not send the last spans: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_is_off_unless_requested() {
        assert!(!export_requested(None));
        assert!(!export_requested(Some("false")));
        assert!(!export_requested(Some("0")));
        assert!(export_requested(Some("true")));
        assert!(export_requested(Some(" YES ")));
    }

    #[test]
    fn traces_url_adds_the_otlp_path_once() {
        assert_eq!(
            traces_url("http://127.0.0.1:4318").as_deref(),
            Some("http://127.0.0.1:4318/v1/traces")
        );
        assert_eq!(
            traces_url("http://127.0.0.1:4318/v1/traces/").as_deref(),
            Some("http://127.0.0.1:4318/v1/traces")
        );
        assert_eq!(traces_url("grpc://127.0.0.1:4317"), None);
    }
}

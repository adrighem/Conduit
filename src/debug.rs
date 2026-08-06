use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Registry;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FILTER_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();
static PIPELINE_COUNTERS: PipelineCounters = PipelineCounters::new();

#[derive(Default)]
pub(crate) struct PipelineCounters {
    jobs: AtomicU64,
    api_requests: AtomicU64,
    sqlite_connections: AtomicU64,
    sqlite_transactions: AtomicU64,
    sqlite_changed_rows: AtomicU64,
    sqlite_skipped_rows: AtomicU64,
    sidebar_operations: AtomicU64,
    document_loads: AtomicU64,
    timeline_deltas: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PipelineCountersSnapshot {
    jobs: u64,
    api_requests: u64,
    sqlite_connections: u64,
    sqlite_transactions: u64,
    sqlite_changed_rows: u64,
    sqlite_skipped_rows: u64,
    sidebar_operations: u64,
    document_loads: u64,
    timeline_deltas: u64,
}

impl PipelineCounters {
    const fn new() -> Self {
        Self {
            jobs: AtomicU64::new(0),
            api_requests: AtomicU64::new(0),
            sqlite_connections: AtomicU64::new(0),
            sqlite_transactions: AtomicU64::new(0),
            sqlite_changed_rows: AtomicU64::new(0),
            sqlite_skipped_rows: AtomicU64::new(0),
            sidebar_operations: AtomicU64::new(0),
            document_loads: AtomicU64::new(0),
            timeline_deltas: AtomicU64::new(0),
        }
    }

    pub(crate) fn record_jobs(&self, amount: u64) {
        saturating_add(&self.jobs, amount);
    }

    pub(crate) fn record_api_request(&self) {
        saturating_add(&self.api_requests, 1);
    }

    pub(crate) fn record_sqlite_connections(&self, amount: u64) {
        saturating_add(&self.sqlite_connections, amount);
    }

    pub(crate) fn record_sqlite_work(&self, transactions: u64, changed: u64, skipped: u64) {
        saturating_add(&self.sqlite_transactions, transactions);
        saturating_add(&self.sqlite_changed_rows, changed);
        saturating_add(&self.sqlite_skipped_rows, skipped);
    }

    pub(crate) fn record_sidebar_operations(&self, amount: usize) {
        saturating_add(
            &self.sidebar_operations,
            u64::try_from(amount).unwrap_or(u64::MAX),
        );
    }

    pub(crate) fn record_document_load(&self) {
        saturating_add(&self.document_loads, 1);
    }

    pub(crate) fn record_timeline_delta(&self) {
        saturating_add(&self.timeline_deltas, 1);
    }

    pub(crate) fn snapshot(&self) -> PipelineCountersSnapshot {
        PipelineCountersSnapshot {
            jobs: self.jobs.load(Ordering::Relaxed),
            api_requests: self.api_requests.load(Ordering::Relaxed),
            sqlite_connections: self.sqlite_connections.load(Ordering::Relaxed),
            sqlite_transactions: self.sqlite_transactions.load(Ordering::Relaxed),
            sqlite_changed_rows: self.sqlite_changed_rows.load(Ordering::Relaxed),
            sqlite_skipped_rows: self.sqlite_skipped_rows.load(Ordering::Relaxed),
            sidebar_operations: self.sidebar_operations.load(Ordering::Relaxed),
            document_loads: self.document_loads.load(Ordering::Relaxed),
            timeline_deltas: self.timeline_deltas.load(Ordering::Relaxed),
        }
    }
}

impl PipelineCountersSnapshot {
    pub(crate) fn delta_since(self, baseline: Self) -> Self {
        Self {
            jobs: self.jobs.saturating_sub(baseline.jobs),
            api_requests: self.api_requests.saturating_sub(baseline.api_requests),
            sqlite_connections: self
                .sqlite_connections
                .saturating_sub(baseline.sqlite_connections),
            sqlite_transactions: self
                .sqlite_transactions
                .saturating_sub(baseline.sqlite_transactions),
            sqlite_changed_rows: self
                .sqlite_changed_rows
                .saturating_sub(baseline.sqlite_changed_rows),
            sqlite_skipped_rows: self
                .sqlite_skipped_rows
                .saturating_sub(baseline.sqlite_skipped_rows),
            sidebar_operations: self
                .sidebar_operations
                .saturating_sub(baseline.sidebar_operations),
            document_loads: self.document_loads.saturating_sub(baseline.document_loads),
            timeline_deltas: self
                .timeline_deltas
                .saturating_sub(baseline.timeline_deltas),
        }
    }
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(amount))
    });
}

pub(crate) fn pipeline_counters() -> &'static PipelineCounters {
    &PIPELINE_COUNTERS
}

pub(crate) fn pipeline_counter_snapshot() -> PipelineCountersSnapshot {
    PIPELINE_COUNTERS.snapshot()
}

pub(crate) fn trace_pipeline_counter_sample(
    baseline: PipelineCountersSnapshot,
    interval_seconds: u64,
) -> PipelineCountersSnapshot {
    let current = PIPELINE_COUNTERS.snapshot();
    let delta = current.delta_since(baseline);
    tracing::debug!(
        target: "conduit::pipeline",
        event = "pipeline_counter_sample",
        interval_seconds,
        jobs = delta.jobs,
        api_requests = delta.api_requests,
        sqlite_connections = delta.sqlite_connections,
        sqlite_transactions = delta.sqlite_transactions,
        sqlite_changed_rows = delta.sqlite_changed_rows,
        sqlite_skipped_rows = delta.sqlite_skipped_rows,
        sidebar_operations = delta.sidebar_operations,
        document_loads = delta.document_loads,
        timeline_deltas = delta.timeline_deltas,
        total_jobs = current.jobs,
        total_api_requests = current.api_requests,
        total_sqlite_connections = current.sqlite_connections,
        total_sqlite_transactions = current.sqlite_transactions,
        total_sqlite_changed_rows = current.sqlite_changed_rows,
        total_sqlite_skipped_rows = current.sqlite_skipped_rows,
        total_sidebar_operations = current.sidebar_operations,
        total_document_loads = current.document_loads,
        total_timeline_deltas = current.timeline_deltas,
    );
    current
}

pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_new(tracing_filter_spec(false, rust_log().as_deref()))?;
    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_target(false)
                .with_writer(std::io::stderr),
        )
        .try_init()?;
    let _ = FILTER_HANDLE.set(filter_handle);
    Ok(())
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if let Some(handle) = FILTER_HANDLE.get() {
        let filter = EnvFilter::new(tracing_filter_spec(enabled, rust_log().as_deref()));
        let _ = handle.reload(filter);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn log(scope: &str, message: &str) {
    if enabled() {
        tracing::debug!(target: "conduit", scope, message);
    }
}

fn rust_log() -> Option<String> {
    std::env::var("RUST_LOG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn tracing_filter_spec(debug: bool, environment: Option<&str>) -> String {
    match (
        debug,
        environment.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (true, Some(environment)) => format!("conduit=debug,{environment}"),
        (true, None) => "conduit=debug".to_string(),
        (false, Some(environment)) => environment.to_string(),
        (false, None) => "off".to_string(),
    }
}

pub fn url_for_log(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return truncate(value);
    };

    if url.scheme() == "data" {
        return "data:<redacted>".to_string();
    }

    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    truncate(url.as_str())
}

fn truncate(value: &str) -> String {
    const MAX_LENGTH: usize = 160;
    if value.chars().count() <= MAX_LENGTH {
        value.to_string()
    } else {
        format!("{}...", value.chars().take(MAX_LENGTH).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_filter_is_quiet_by_default_and_debuggable_on_request() {
        assert_eq!(tracing_filter_spec(false, None), "off");
        assert_eq!(tracing_filter_spec(true, None), "conduit=debug");
    }

    #[test]
    fn tracing_filter_preserves_explicit_environment_overrides() {
        assert_eq!(
            tracing_filter_spec(false, Some("conduit::runtime=trace")),
            "conduit::runtime=trace"
        );
        assert_eq!(
            tracing_filter_spec(true, Some("conduit::runtime=trace")),
            "conduit=debug,conduit::runtime=trace"
        );
    }

    #[test]
    fn diagnostic_urls_remove_credentials_queries_fragments_and_data() {
        assert_eq!(
            url_for_log(
                "https://viewer:password@files.slack.com/path/image.png?token=signed-secret#preview"
            ),
            "https://files.slack.com/path/image.png"
        );
        assert_eq!(
            url_for_log("data:image/png;base64,sensitive"),
            "data:<redacted>"
        );
    }

    #[test]
    fn pipeline_counters_report_redacted_activity_deltas() {
        let counters = PipelineCounters::default();
        let baseline = counters.snapshot();

        counters.record_jobs(2);
        counters.record_api_request();
        counters.record_sqlite_connections(3);
        counters.record_sqlite_work(1, 4, 5);
        counters.record_sidebar_operations(2);
        counters.record_document_load();
        counters.record_timeline_delta();

        assert_eq!(
            counters.snapshot().delta_since(baseline),
            PipelineCountersSnapshot {
                jobs: 2,
                api_requests: 1,
                sqlite_connections: 3,
                sqlite_transactions: 1,
                sqlite_changed_rows: 4,
                sqlite_skipped_rows: 5,
                sidebar_operations: 2,
                document_loads: 1,
                timeline_deltas: 1,
            }
        );
    }

    #[test]
    fn pipeline_counter_addition_saturates() {
        let counter = AtomicU64::new(u64::MAX - 1);

        saturating_add(&counter, 3);

        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}

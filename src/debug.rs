use std::collections::BTreeMap;
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
static API_METHOD_COUNTS: std::sync::Mutex<BTreeMap<String, u64>> =
    std::sync::Mutex::new(BTreeMap::new());

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

    #[cfg(test)]
    pub(crate) fn record_document_load(&self) {
        saturating_add(&self.document_loads, 1);
    }

    #[cfg(test)]
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
    trace_api_requests_by_method(interval_seconds);
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

/// Loader an image decode goes through, which decides where the work runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageLoader {
    /// `gdk::Texture::from_filename`: GTK decodes PNG and JPEG in-process and
    /// hands anything else to gdk-pixbuf.
    Texture,
    /// gdk-pixbuf, whose glycin loaders run in a sandboxed subprocess.
    Pixbuf,
}

/// `format=` label for a decode log line.
fn image_format_label(format: Option<crate::image_header::ImageFormat>) -> &'static str {
    use crate::image_header::ImageFormat;
    match format {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpeg",
        Some(ImageFormat::Gif) => "gif",
        Some(ImageFormat::Webp) => "webp",
        None => "other",
    }
}

/// Whether `loader` decodes `format` in GTK itself (`gtk`) or through
/// gdk-pixbuf and glycin (`pixbuf`).
fn image_decode_backend(
    loader: ImageLoader,
    format: Option<crate::image_header::ImageFormat>,
) -> &'static str {
    use crate::image_header::ImageFormat;
    match (loader, format) {
        (ImageLoader::Texture, Some(ImageFormat::Png | ImageFormat::Jpeg)) => "gtk",
        _ => "pixbuf",
    }
}

thread_local! {
    /// Decodes since the last idle: count, GTK backend ms, pixbuf backend
    /// ms, and whether the idle report is scheduled.
    static DECODE_BURST: std::cell::Cell<(u32, f64, f64, bool)> =
        const { std::cell::Cell::new((0, 0.0, 0.0, false)) };
}

/// Folds one decode into [`DECODE_BURST`] and, on the GTK thread, schedules
/// one `image_decode_burst count=<n> gtk_ms=<x> pixbuf_ms=<y>` line per idle.
fn record_decode_burst(backend: &str, elapsed_ms: f64) {
    if !gtk::glib::MainContext::default().is_owner() {
        return;
    }
    let (count, gtk_ms, pixbuf_ms, scheduled) = DECODE_BURST.get();
    let (gtk_ms, pixbuf_ms) = if backend == "gtk" {
        (gtk_ms + elapsed_ms, pixbuf_ms)
    } else {
        (gtk_ms, pixbuf_ms + elapsed_ms)
    };
    DECODE_BURST.set((count + 1, gtk_ms, pixbuf_ms, true));
    if !scheduled {
        gtk::glib::idle_add_local_once(|| {
            let (count, gtk_ms, pixbuf_ms, _) = DECODE_BURST.replace((0, 0.0, 0.0, false));
            log(
                "performance",
                &format!(
                    "image_decode_burst count={count} gtk_ms={gtk_ms:.2} pixbuf_ms={pixbuf_ms:.2}"
                ),
            );
        });
    }
}

#[cfg(test)]
thread_local! {
    /// Image decodes traced on this thread, whether or not logging is on, so
    /// perf guards can assert how many decodes a code path makes.
    pub(crate) static IMAGE_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs an image decode of `path` through `loader` and logs it when it took
/// at least 4 ms, since the callers run on the GTK thread. Costs nothing but
/// a flag check when debug logging is off.
pub(crate) fn trace_image_decode<T>(
    kind: &str,
    path: &std::path::Path,
    loader: ImageLoader,
    decode: impl FnOnce() -> T,
) -> T {
    #[cfg(test)]
    IMAGE_DECODES.set(IMAGE_DECODES.get() + 1);
    if !enabled() {
        return decode();
    }
    let started = std::time::Instant::now();
    let result = decode();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let format = crate::image_header::declared_header(path).map(|(format, _, _)| format);
    let backend = image_decode_backend(loader, format);
    record_decode_burst(backend, elapsed_ms);
    if elapsed_ms >= 4.0 {
        let format = image_format_label(format);
        log(
            "performance",
            &format!(
                "image_decode kind={kind} format={format} backend={backend} elapsed_ms={elapsed_ms:.2}"
            ),
        );
    }
    result
}

/// Counts one Slack API request for `label` (a method name or a fixed
/// bucket) when debug logging is on. Bounded: at most 64 distinct labels.
pub(crate) fn record_api_request_method(label: &str) {
    if !enabled() {
        return;
    }
    let mut counts = API_METHOD_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(count) = counts.get_mut(label) {
        *count = count.saturating_add(1);
    } else if counts.len() < 64 {
        counts.insert(label.to_string(), 1);
    } else {
        let other = counts.entry("other".to_string()).or_insert(0);
        *other = other.saturating_add(1);
    }
}

/// Logs and resets the per-method API request counts for the last interval.
fn trace_api_requests_by_method(interval_seconds: u64) {
    let mut sorted = {
        let mut counts = API_METHOD_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *counts).into_iter().collect::<Vec<_>>()
    };
    if sorted.is_empty() {
        return;
    }
    sorted.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let breakdown = sorted
        .iter()
        .map(|(method, count)| format!("{method}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    log(
        "performance",
        &format!("api_requests_by_method interval_seconds={interval_seconds} {breakdown}"),
    );
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
    fn image_decodes_name_the_backend_that_runs_them() {
        use crate::image_header::ImageFormat;
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            assert_eq!(
                image_decode_backend(ImageLoader::Texture, Some(format)),
                "gtk"
            );
            assert_eq!(
                image_decode_backend(ImageLoader::Pixbuf, Some(format)),
                "pixbuf"
            );
        }
        // GTK hands formats it cannot decode itself to gdk-pixbuf.
        for format in [Some(ImageFormat::Gif), Some(ImageFormat::Webp), None] {
            assert_eq!(image_decode_backend(ImageLoader::Texture, format), "pixbuf");
        }
        assert_eq!(image_format_label(Some(ImageFormat::Jpeg)), "jpeg");
        assert_eq!(image_format_label(None), "other");
    }

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

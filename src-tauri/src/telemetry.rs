//! Opt-in local traces. Exporters run on SDK batch workers, never on the
//! mutation path; the existing log/env_logger pipeline remains independent.

use opentelemetry::trace::{SpanContext, TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue, Value};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider, SpanData, SpanExporter,
};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

/// Own until shutdown so the final partial batch is flushed as well.
pub struct TelemetryGuard(Option<SdkTracerProvider>);

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.0.take() {
            if let Err(error) = provider.shutdown() {
                log::warn!("trace exporter shutdown failed: {error}");
            }
        }
    }
}

pub fn init() -> Result<TelemetryGuard, String> {
    let otlp = std::env::var("GARDEN_OTEL").is_ok_and(|value| value == "1");
    let file = std::env::var_os("GARDEN_OTEL_FILE").filter(|path| !path.is_empty());
    init_options(otlp, file)
}

fn init_options(otlp: bool, file: Option<std::ffi::OsString>) -> Result<TelemetryGuard, String> {
    if !otlp && file.is_none() {
        // tracing's `log` feature is on through a dependency, and it turns every span into a `log` record while no
        // subscriber is set: on the hot path, and past env_logger's target filter. The no-op subscriber keeps spans
        // inert (never interested, nothing formatted).
        let _ =
            tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
        return Ok(TelemetryGuard(None));
    }
    let file = file
        .map(|path| JsonFileExporter::open(Path::new(&path)))
        .transpose()?;
    let mut resource = opentelemetry_sdk::Resource::builder_empty().with_attributes([
        KeyValue::new("service.name", "gardend"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]);
    if let Some(build) = std::env::var_os("GARDEN_UI_DIR")
        .and_then(|ui| {
            Path::new(&ui)
                .parent()
                .map(|parent| parent.join("BUILD.txt"))
        })
        .and_then(|path| std::fs::read_to_string(path).ok())
    {
        resource =
            resource.with_attributes([KeyValue::new("sophia.build", build.trim().to_string())]);
    }
    let mut builder = SdkTracerProvider::builder()
        .with_resource(resource.build())
        .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
            opentelemetry_sdk::trace::Sampler::AlwaysOn,
        )));
    if otlp {
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:4318".to_string());
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
            .with_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| format!("initialize OTLP exporter: {error}"))?;
        builder = builder.with_span_processor(batch(exporter));
    }
    if let Some(exporter) = file {
        builder = builder.with_span_processor(batch(exporter));
    }
    let provider = builder.build();
    tracing::subscriber::set_global_default(subscriber(&provider, true))
        .map_err(|error| format!("install trace subscriber: {error}"))?;
    Ok(TelemetryGuard(Some(provider)))
}

fn subscriber(
    provider: &SdkTracerProvider,
    enabled: bool,
) -> impl tracing::Subscriber + Send + Sync + 'static {
    tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("gardend"))
            .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
                enabled && metadata.target() == "gardend_otel"
            })),
    )
}

fn batch(exporter: impl SpanExporter + 'static) -> BatchSpanProcessor {
    BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(2048)
                .with_max_export_batch_size(256)
                .with_scheduled_delay(Duration::from_millis(500))
                .build(),
        )
        .build()
}

pub(crate) fn enabled() -> bool {
    tracing::enabled!(target: "gardend_otel", tracing::Level::INFO)
}

pub(crate) fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn elapsed_ms(start: Option<Instant>) -> f64 {
    start.map_or(0.0, |start| start.elapsed().as_secs_f64() * 1000.0)
}

/// A blocking worker needs both the span and the dispatch (the latter also
/// keeps scoped SDK subscribers working in tests).
pub(crate) fn blocking<F, R>(work: F) -> impl FnOnce() -> R + Send
where
    F: FnOnce() -> R + Send,
{
    let span = tracing::Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    move || tracing::dispatcher::with_default(&dispatch, || span.in_scope(work))
}

/// Journal only W3C ids and flags. Baggage and arbitrary caller values never
/// enter durable operations. Recovery changes causality to a link in memory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct TraceContext {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    traceparent: String,
    #[serde(skip)]
    pub(crate) recovered: bool,
}

impl TraceContext {
    pub(crate) fn is_empty(&self) -> bool {
        self.traceparent.is_empty()
    }

    pub(crate) fn capture() -> Self {
        if !enabled() {
            return Self::default();
        }
        let context = tracing::Span::current().context();
        let span = context.span();
        let sc = span.span_context();
        Self {
            traceparent: if sc.is_valid() {
                format!(
                    "00-{}-{}-{:02x}",
                    sc.trace_id(),
                    sc.span_id(),
                    sc.trace_flags().to_u8()
                )
            } else {
                String::new()
            },
            recovered: false,
        }
    }

    pub(crate) fn span_context(&self) -> Option<SpanContext> {
        use opentelemetry::propagation::TextMapPropagator;
        let carrier = std::collections::HashMap::from([(
            "traceparent".to_string(),
            self.traceparent.clone(),
        )]);
        let context = TraceContextPropagator::new().extract(&carrier);
        let sc = context.span().span_context().clone();
        sc.is_valid().then_some(sc)
    }
}

pub(crate) fn operation_span(
    operation: &crate::crdt_operation_types::CrdtOperation,
) -> tracing::Span {
    let span = tracing::info_span!(target: "gardend_otel", parent: None, "crdt.op.apply",
        sophia.graph.id = %operation.graph_id,
        sophia.document.id = operation.document_id.as_deref().unwrap_or(""),
        sophia.operation.id = %operation.operation_id,
        sophia.crdt.kind = safe_crdt_kind(&operation.kind),
        sophia.queue.wait_ms = tracing::field::Empty,
        sophia.operation.recovered = operation.trace_context.recovered,
        otel.status_code = tracing::field::Empty);
    if !span.is_disabled() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Ok(enqueued) = operation.enqueue_timestamp.parse::<u64>() {
            span.record("sophia.queue.wait_ms", now.saturating_sub(enqueued));
        }
        if let Some(context) = operation.trace_context.span_context() {
            if operation.trace_context.recovered {
                span.add_link(context);
            } else {
                let _ = span.set_parent(Context::new().with_remote_span_context(context));
            }
        }
    }
    span
}

pub(crate) fn safe_crdt_kind(kind: &str) -> &'static str {
    // Reuse the existing closed allowlist; never export arbitrary input.
    match kind {
        "graph.restoreArchive" => "graph.restoreArchive",
        _ => {
            crate::cell_durability_trace::known_crdt_kind(kind).map_or("other", |kind| kind.label())
        }
    }
}

pub(crate) fn linked_flush_span(
    graph_id: &str,
    document_id: Option<&str>,
    generation: u64,
    debounce: Duration,
    contexts: Vec<TraceContext>,
    dropped: u64,
) -> tracing::Span {
    let span = tracing::info_span!(target: "gardend_otel", parent: None, "projection.flush",
        sophia.graph.id = graph_id,
        sophia.document.id = document_id.unwrap_or(""),
        sophia.projection.generation = generation,
        sophia.projection.debounce_ms = debounce.as_millis() as u64,
        sophia.projection.attempts = 0_u64,
        sophia.projection.dropped_links = dropped);
    for context in contexts {
        if let Some(context) = context.span_context() {
            span.add_link(context);
        }
    }
    span
}

pub(crate) async fn http_server(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use opentelemetry::propagation::TextMapPropagator;
    use tracing::Instrument;
    if !enabled() {
        return next.run(request).await;
    }
    // Route templates contain no query strings, document text, credentials,
    // or arbitrary fallback paths. Never record the raw URL or headers.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str())
        .unwrap_or("unmatched");
    let method = match request.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "_OTHER",
    };
    let span = tracing::info_span!(target: "gardend_otel", parent: None, "HTTP request",
        otel.name = %format!("{method} {route}"), otel.kind = "server",
        http.request.method = method, http.route = route,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty);
    let carrier = request
        .headers()
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            std::collections::HashMap::from([("traceparent".to_string(), value.to_string())])
        })
        .unwrap_or_default();
    let _ = span.set_parent(TraceContextPropagator::new().extract(&carrier));
    async move {
        let response = next.run(request).await;
        tracing::Span::current().record("http.response.status_code", response.status().as_u16());
        if response.status().is_server_error() {
            tracing::Span::current().record("otel.status_code", "ERROR");
        }
        let (parts, body) = response.into_parts();
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), tracing::Span::current()),
            |(mut body, span)| async move {
                use futures_util::StreamExt;
                let next = body.next().instrument(span.clone()).await;
                next.map(|chunk| (chunk, (body, span)))
            },
        );
        axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
    }
    .instrument(span)
    .await
}

#[derive(Debug)]
struct JsonFileExporter {
    file: Mutex<File>,
    resource: serde_json::Value,
}

impl JsonFileExporter {
    fn open(path: &Path) -> Result<Self, String> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|file| Self {
                file: Mutex::new(file),
                resource: serde_json::json!({}),
            })
            .map_err(|error| format!("open trace file: {error}"))
    }
}

fn attributes(values: &[KeyValue]) -> serde_json::Value {
    values
        .iter()
        .map(|kv| (kv.key.as_str().to_string(), attribute_value(&kv.value)))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn attribute_value(value: &Value) -> serde_json::Value {
    use opentelemetry::Array;
    match value {
        Value::Bool(v) => (*v).into(),
        Value::I64(v) => (*v).into(),
        Value::F64(v) => serde_json::json!(v),
        Value::String(v) => v.as_str().into(),
        Value::Array(Array::Bool(v)) => serde_json::json!(v),
        Value::Array(Array::I64(v)) => serde_json::json!(v),
        Value::Array(Array::F64(v)) => serde_json::json!(v),
        Value::Array(Array::String(v)) => {
            serde_json::json!(v.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        }
        _ => serde_json::Value::Null,
    }
}

fn nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn span_json(span: &SpanData) -> serde_json::Value {
    serde_json::json!({
        "trace_id": span.span_context.trace_id().to_string(),
        "span_id": span.span_context.span_id().to_string(),
        "parent_span_id": span.parent_span_id.to_string(),
        "name": span.name,
        "kind": format!("{:?}", span.span_kind),
        "start_unix_nanos": nanos(span.start_time),
        "end_unix_nanos": nanos(span.end_time),
        "attributes": attributes(&span.attributes),
        "events": span.events.events.iter().map(|event| serde_json::json!({
            "name": event.name, "time_unix_nanos": nanos(event.timestamp),
            "attributes": attributes(&event.attributes),
            "dropped_attributes_count": event.dropped_attributes_count,
        })).collect::<Vec<_>>(),
        "links": span.links.links.iter().map(|link| serde_json::json!({
            "trace_id": link.span_context.trace_id().to_string(),
            "span_id": link.span_context.span_id().to_string(),
            "trace_flags": link.span_context.trace_flags().to_u8(),
            "trace_state": link.span_context.trace_state().header(),
            "attributes": attributes(&link.attributes),
            "dropped_attributes_count": link.dropped_attributes_count,
        })).collect::<Vec<_>>(),
        "status": match &span.status {
            opentelemetry::trace::Status::Unset => serde_json::json!({"code": "unset"}),
            opentelemetry::trace::Status::Ok => serde_json::json!({"code": "ok"}),
            opentelemetry::trace::Status::Error { description } => serde_json::json!({"code": "error", "description": description}),
        },
    })
}

impl SpanExporter for JsonFileExporter {
    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.resource = resource
            .iter()
            .map(|(key, value)| (key.as_str().to_string(), attribute_value(value)))
            .collect::<serde_json::Map<_, _>>()
            .into();
    }

    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let mut file = self
            .file
            .lock()
            .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
        for span in batch {
            let mut json = span_json(&span);
            json["resource"] = self.resource.clone();
            let mut line = serde_json::to_vec(&json)
                .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
            line.push(b'\n');
            file.write_all(&line)
                .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
        }
        file.flush()
            .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    pub(crate) fn capture(
        enabled: bool,
    ) -> (SdkTracerProvider, InMemorySpanExporter, tracing::Dispatch) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let dispatch = tracing::Dispatch::new(subscriber(&provider, enabled));
        (provider, exporter, dispatch)
    }

    pub(crate) fn json(spans: &[SpanData]) -> String {
        serde_json::to_string(&spans.iter().map(span_json).collect::<Vec<_>>()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    #[test]
    fn disabled_pipeline_has_no_provider_or_recorded_spans() {
        assert!(init_options(false, None).unwrap().0.is_none());
        let (provider, exporter, dispatch) = test_support::capture(false);
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!(target: "gardend_otel", "disabled");
            assert!(span.is_disabled());
            assert!(TraceContext::capture().span_context().is_none());
            let lease =
                crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator::default()
                    .acquire_hot_write_blocking("disabled")
                    .unwrap();
            lease.declare_rdf_read_only();
        });
        provider.force_flush().unwrap();
        assert!(exporter.get_finished_spans().unwrap().is_empty());
    }

    #[test]
    fn http_extracts_w3c_context_and_records_only_route_templates() {
        use tower::ServiceExt;
        let (provider, exporter, dispatch) = test_support::capture(true);
        tracing::dispatcher::with_default(&dispatch, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let router = axum::Router::new()
                        .route(
                            "/documents/{document_id}",
                            axum::routing::get(|| async { axum::http::StatusCode::NO_CONTENT }),
                        )
                        .layer(axum::middleware::from_fn(http_server));
                    let response = router
                        .oneshot(
                            axum::http::Request::builder()
                                .uri("/documents/PRIVATE_TEXT_MARKER?token=PRIVATE_TEXT_MARKER")
                                .header("authorization", "Bearer PRIVATE_TEXT_MARKER")
                                .header(
                                    "traceparent",
                                    "00-12345678901234567890123456789012-1234567890123456-01",
                                )
                                .body(axum::body::Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
                    axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap();
                });
        });
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "GET /documents/{document_id}");
        assert_eq!(spans[0].span_kind, opentelemetry::trace::SpanKind::Server);
        assert_eq!(
            spans[0].span_context.trace_id().to_string(),
            "12345678901234567890123456789012"
        );
        assert_eq!(spans[0].parent_span_id.to_string(), "1234567890123456");
        assert!(!test_support::json(&spans).contains("PRIVATE_TEXT_MARKER"));
    }

    #[test]
    fn file_exporter_appends_complete_json_spans() {
        let path = std::env::temp_dir().join(format!("garden-otel-{}.jsonl", uuid::Uuid::new_v4()));
        // Existing lines survive exporter startup.
        std::fs::write(&path, "{\"existing\":true}\n").unwrap();
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_simple_exporter(JsonFileExporter::open(&path).unwrap())
            .build();
        let dispatch = tracing::Dispatch::new(subscriber(&provider, true));
        tracing::dispatcher::with_default(&dispatch, || {
            let root = tracing::info_span!(target: "gardend_otel", "file.root");
            let context = root.in_scope(TraceContext::capture);
            root.in_scope(|| {
                let child = tracing::info_span!(target: "gardend_otel", "file.child", otel.kind = "server",
                    sophia.count = 7_u64, otel.status_code = "ERROR");
                child.add_link(context.span_context().unwrap());
                child.in_scope(|| tracing::info!(target: "gardend_otel", { sophia.count = 1_u64 }, "finished"));
            });
        });
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = bytes
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), spans.len() + 1);
        for span in &lines[1..] {
            for field in [
                "trace_id",
                "span_id",
                "parent_span_id",
                "name",
                "kind",
                "start_unix_nanos",
                "end_unix_nanos",
                "attributes",
                "events",
                "links",
                "status",
            ] {
                assert!(span.get(field).is_some(), "missing {field}");
            }
            assert_eq!(span["trace_id"].as_str().unwrap().len(), 32);
            assert_eq!(span["span_id"].as_str().unwrap().len(), 16);
            assert!(span["end_unix_nanos"].as_u64() >= span["start_unix_nanos"].as_u64());
        }
        let child = lines
            .iter()
            .find(|span| span["name"] == "file.child")
            .unwrap();
        assert!(!child["events"].as_array().unwrap().is_empty());
        assert_eq!(child["links"].as_array().unwrap().len(), 1);
        assert_eq!(child["status"]["code"], "error");
        assert_ne!(child["parent_span_id"], "0000000000000000");
        provider.shutdown().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}

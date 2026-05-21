//! Integration tests for the tracing-usage HTTP endpoints.
//!
//! Mirrors `tracing_dashboard_http.rs` for the four new usage rollup
//! routes:
//!
//! - `GET /v1/tracing/usage[?label=key:value]`
//! - `GET /v1/tracing/runs/{run_id}/usage`
//! - `GET /v1/sessions/{session_id}/usage`
//! - `GET /v1/sessions/{session_id}/runs/{run_id}/usage`
//!
//! Records are pushed directly into the shared [`SpanBuffer`] so the
//! tests don't depend on a real LLM provider — they exercise the wire
//! contract end-to-end (axum extractors, JSON shape, status codes,
//! session-membership gating, pricing on/off).

#![cfg(feature = "dashboard")]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    ModelPricing, ServerInfoPlugin, SpanBuffer, SpanKind, SpanRecord, TracingPlugin, UsagePricing,
};
use polaris_system::server::Server;
use serde_json::{Value, json};

async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

async fn wait_for_server(port: u16) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(10));
    loop {
        interval.tick().await;
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("server on port {port} did not become ready within 5 s");
        }
    }
}

/// Builds a synthetic `chat`-shaped close record carrying the `OTel` `GenAI`
/// attributes the production decorator sets on real LLM calls.
fn chat_record(
    session_id: Option<&str>,
    run_id: &str,
    provider: &str,
    model: &str,
    agent_type: Option<&str>,
    input: u64,
    output: u64,
) -> SpanRecord {
    let mut rec = SpanRecord::new(
        "2026-05-15T12:00:00.000Z",
        "info",
        "tests",
        "chat",
        SpanKind::SpanClose,
    )
    .with_started_at("2026-05-15T11:59:59.000Z")
    .with_duration_ms(10)
    .with_run_id(run_id)
    .with_field("gen_ai.provider.name", json!(provider))
    .with_field("gen_ai.request.model", json!(model))
    .with_field("gen_ai.usage.input_tokens", json!(input))
    .with_field("gen_ai.usage.output_tokens", json!(output));
    if let Some(session) = session_id {
        rec = rec.with_label("session_id", session);
    }
    if let Some(agent) = agent_type {
        rec = rec.with_label("agent_type", agent);
    }
    rec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracing_usage_endpoints_serve_aggregated_views() {
    let (listener, port) = bind_ephemeral().await;
    let base = format!("http://127.0.0.1:{port}");

    let mut server = Server::new();
    server.add_plugins(ServerInfoPlugin);
    server.add_plugins(polaris_models::ModelsPlugin);
    server.add_plugins(polaris_tools::ToolsPlugin);
    server
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(TracingPlugin::new());
    server.finish().await;

    // Seed the shared SpanBuffer with a mix of session-scoped and
    // unattributed LLM calls.
    let buffer = server.api::<SpanBuffer>().expect("buffer registered");

    // sess-A → run-A1: two anthropic calls, opus model, react agent.
    buffer.push(chat_record(
        Some("sess-A"),
        "run-A1",
        "anthropic",
        "claude-opus-4-7",
        Some("react"),
        100,
        50,
    ));
    buffer.push(chat_record(
        Some("sess-A"),
        "run-A1",
        "anthropic",
        "claude-opus-4-7",
        Some("react"),
        200,
        75,
    ));
    // sess-A → run-A2: one openai call.
    buffer.push(chat_record(
        Some("sess-A"),
        "run-A2",
        "openai",
        "gpt-5",
        Some("rewoo"),
        10,
        20,
    ));
    // sess-B → run-B1: one bedrock call.
    buffer.push(chat_record(
        Some("sess-B"),
        "run-B1",
        "bedrock",
        "claude-haiku-4-5",
        Some("react"),
        5,
        5,
    ));

    let pricing_clone = server
        .api::<UsagePricing>()
        .expect("pricing registered")
        .clone();

    wait_for_server(port).await;
    let client = reqwest::Client::new();

    // -- /v1/tracing/usage — buffer-wide totals (no pricing yet) ---------
    let resp = client
        .get(format!("{base}/v1/tracing/usage"))
        .send()
        .await
        .expect("usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("usage body");
    assert_eq!(body["totals"]["input_tokens"], 315);
    assert_eq!(body["totals"]["output_tokens"], 150);
    assert_eq!(body["totals"]["total_tokens"], 465);
    assert!(
        body["totals"].get("cost_usd").is_none() || body["totals"]["cost_usd"] == Value::Null,
        "no pricing registered → cost_usd is absent or null"
    );
    let model_keys: Vec<&str> = body["by_model"]
        .as_array()
        .expect("by_model")
        .iter()
        .filter_map(|row| row["key"].as_str())
        .collect();
    assert_eq!(
        model_keys[0], "claude-opus-4-7",
        "highest-volume model first"
    );
    assert_eq!(body["source_span_count"], 4);

    // -- /v1/tracing/usage?label=session_id:sess-A -----------------------
    let resp = client
        .get(format!("{base}/v1/tracing/usage?label=session_id:sess-A"))
        .send()
        .await
        .expect("label-filtered usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("label-filtered body");
    assert_eq!(body["totals"]["input_tokens"], 310);
    assert_eq!(body["totals"]["output_tokens"], 145);
    assert_eq!(body["source_span_count"], 3);

    // -- /v1/tracing/runs/{run_id}/usage --------------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-A1/usage"))
        .send()
        .await
        .expect("run usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("run usage body");
    assert_eq!(body["totals"]["input_tokens"], 300);
    assert_eq!(body["totals"]["output_tokens"], 125);
    assert_eq!(body["source_span_count"], 2);

    // -- unknown run → 404 -----------------------------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/does-not-exist/usage"))
        .send()
        .await
        .expect("unknown run usage GET");
    assert_eq!(resp.status(), 404);

    // -- /v1/sessions/{id}/usage -----------------------------------------
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/usage"))
        .send()
        .await
        .expect("session usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("session usage body");
    assert_eq!(body["totals"]["total_tokens"], 455);

    // Unknown session — zeroed response, NOT 404.
    let resp = client
        .get(format!("{base}/v1/sessions/sess-unknown/usage"))
        .send()
        .await
        .expect("empty session usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("empty session body");
    assert_eq!(body["totals"]["total_tokens"], 0);
    assert_eq!(body["source_span_count"], 0);

    // -- /v1/sessions/{id}/runs/{run}/usage gates on membership ----------
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/runs/run-A1/usage"))
        .send()
        .await
        .expect("session run usage GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("session run usage body");
    assert_eq!(body["totals"]["total_tokens"], 425);

    // Cross-session lookup must 404 even when both ids exist in the buffer.
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/runs/run-B1/usage"))
        .send()
        .await
        .expect("cross-session run usage GET");
    assert_eq!(resp.status(), 404);

    // -- Register pricing → cost_usd populates next request --------------
    pricing_clone.set(
        "anthropic",
        "claude-opus-4-7",
        ModelPricing::new(15.0, 75.0),
    );

    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-A1/usage"))
        .send()
        .await
        .expect("run usage GET (priced)");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("priced body");
    // 300 input * $15/M + 125 output * $75/M = $0.0045 + $0.009375 = $0.013875.
    let cost = body["totals"]["cost_usd"].as_f64().expect("cost present");
    assert!(
        (cost - 0.013_875).abs() < 1e-9,
        "expected $0.013875, got {cost}"
    );

    server.cleanup().await;
}

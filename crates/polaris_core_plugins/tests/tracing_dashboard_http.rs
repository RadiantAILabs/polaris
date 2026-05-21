//! Integration tests for the tracing dashboard HTTP endpoints.
//!
//! Verifies the run-aware endpoints introduced by A10:
//!
//! - `GET /v1/tracing/runs` — distinct run summaries.
//! - `GET /v1/tracing/runs/{run_id}` — hierarchical span tree
//!   (payloads embedded by default; `?include=structure` strips them).
//! - `GET /v1/tracing/runs/{run_id}/spans/{span_id}` — single-span
//!   payload lookup used by the structure-only follow-up.
//!
//! The tests share one server: `TracingPlugin::ready()` installs a global
//! tracing subscriber via `try_init()`, which only succeeds once per
//! process. A single `#[tokio::test]` covers every scenario by issuing
//! requests against distinct synthetic `run_id`s pushed into the shared
//! `SpanBuffer`.

#![cfg(feature = "dashboard")]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{ServerInfoPlugin, SpanBuffer, SpanKind, SpanRecord, TracingPlugin};
use polaris_system::server::Server;
use serde_json::Value;

/// Binds to an ephemeral port and returns the listener with its port.
async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

/// Polls the server until it accepts a TCP connection, or panics after timeout.
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

fn record(
    run_id: &str,
    span_id: Option<&str>,
    parent_span_id: Option<&str>,
    name: &str,
    kind: SpanKind,
) -> SpanRecord {
    let mut rec = SpanRecord::new("2026-05-15T12:00:00.000Z", "info", "tests", name, kind)
        .with_started_at("2026-05-15T11:59:59.000Z")
        .with_duration_ms(42)
        .with_run_id(run_id)
        .with_field("polaris.session.agent_type", Value::String("demo".into()));
    if let Some(id) = span_id {
        rec = rec.with_span_id(id);
    }
    if let Some(parent) = parent_span_id {
        rec = rec.with_parent_span_id(parent);
    }
    rec
}

/// Variant of [`record`] that attaches a `session_id` label, so the run
/// surfaces on the session-scoped tracing endpoints.
fn record_for_session(
    session_id: &str,
    run_id: &str,
    span_id: Option<&str>,
    parent_span_id: Option<&str>,
    name: &str,
    kind: SpanKind,
) -> SpanRecord {
    record(run_id, span_id, parent_span_id, name, kind).with_label("session_id", session_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracing_dashboard_endpoints_serve_run_aware_views() {
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

    // Seed synthetic records into the shared SpanBuffer. Each scenario
    // uses a distinct run id so they don't cross-contaminate.
    let buffer = server.api::<SpanBuffer>().expect("buffer registered");

    // run-1: root with one nested child + one event under the child.
    buffer.push(record(
        "run-1",
        Some("root-1"),
        None,
        "polaris.graph.execute",
        SpanKind::SpanClose,
    ));
    buffer.push(record(
        "run-1",
        Some("child-1"),
        Some("root-1"),
        "polaris.graph.execute_system",
        SpanKind::SpanClose,
    ));
    buffer.push(
        record(
            "run-1",
            None,
            Some("child-1"),
            "system.log",
            SpanKind::Event,
        )
        .with_message("hello from a child"),
    );

    // run-2: solo root, no children — used to verify multi-run summary.
    buffer.push(record(
        "run-2",
        Some("root-2"),
        None,
        "polaris.graph.execute",
        SpanKind::SpanClose,
    ));

    // run-orphan: child whose parent was evicted (aged-out root).
    buffer.push(record(
        "run-orphan",
        Some("c-orphan"),
        Some("aged-out"),
        "polaris.graph.execute_system",
        SpanKind::SpanClose,
    ));

    // Session-scoped records — exercise `polaris.label.session_id`
    // membership checks on the `/v1/sessions/...` family. sess-A has two
    // runs, sess-B one — the wrapper endpoints must filter accordingly
    // and 404 on cross-session lookups.
    buffer.push(record_for_session(
        "sess-A",
        "run-A1",
        Some("root-A1"),
        None,
        "polaris.graph.execute",
        SpanKind::SpanClose,
    ));
    buffer.push(record_for_session(
        "sess-A",
        "run-A2",
        Some("root-A2"),
        None,
        "polaris.graph.execute",
        SpanKind::SpanClose,
    ));
    buffer.push(record_for_session(
        "sess-B",
        "run-B1",
        Some("root-B1"),
        None,
        "polaris.graph.execute",
        SpanKind::SpanClose,
    ));

    wait_for_server(port).await;

    let client = reqwest::Client::new();

    // -- /v1/tracing/runs ------------------------------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs"))
        .send()
        .await
        .expect("runs GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("runs body");
    let items = body["items"].as_array().expect("items array");
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["run_id"].as_str().unwrap_or_default())
        .collect();
    assert!(ids.contains(&"run-1"), "run-1 missing from {ids:?}");
    assert!(ids.contains(&"run-2"), "run-2 missing from {ids:?}");
    assert!(
        ids.contains(&"run-orphan"),
        "run-orphan missing from {ids:?}"
    );

    // -- /v1/tracing/runs/{id} — payloads embedded by default ------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-1"))
        .send()
        .await
        .expect("run-1 tree GET");
    assert_eq!(resp.status(), 200);
    let tree: Value = resp.json().await.expect("tree body");
    assert_eq!(tree["run_id"], "run-1");
    let roots = tree["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1, "single root expected: {roots:?}");
    let root = &roots[0];
    assert_eq!(root["span_id"], "root-1");
    let children = root["children"].as_array().expect("children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["span_id"], "child-1");
    let events = children[0]["events"].as_array().expect("events");
    assert_eq!(events.len(), 1, "event payload must be embedded by default");
    assert_eq!(events[0]["message"], "hello from a child");

    // -- /v1/tracing/runs/{id}?include=structure — payloads stripped ----
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-1?include=structure"))
        .send()
        .await
        .expect("run-1 structure GET");
    assert_eq!(resp.status(), 200);
    let tree: Value = resp.json().await.expect("structure body");
    let child_events = tree["roots"][0]["children"][0]
        .get("events")
        .and_then(Value::as_array);
    assert!(
        child_events.is_none_or(Vec::is_empty),
        "structure-only must drop event payloads, got {child_events:?}"
    );

    // -- aged-out root surfaces in the orphans bucket -------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-orphan"))
        .send()
        .await
        .expect("run-orphan tree GET");
    assert_eq!(resp.status(), 200);
    let tree: Value = resp.json().await.expect("orphan body");
    let orphans = tree["orphans"].as_array().expect("orphans");
    assert!(
        !orphans.is_empty(),
        "child with missing parent must appear in orphans"
    );

    // -- unknown run → 404 -----------------------------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/does-not-exist"))
        .send()
        .await
        .expect("unknown run GET");
    assert_eq!(resp.status(), 404);

    // -- /v1/tracing/runs/{id}/spans/{span_id} ---------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-1/spans/child-1"))
        .send()
        .await
        .expect("span GET");
    assert_eq!(resp.status(), 200);
    let span: Value = resp.json().await.expect("span body");
    assert_eq!(span["span_id"], "child-1");
    assert_eq!(span["name"], "polaris.graph.execute_system");

    // -- unknown span → 404 ----------------------------------------------
    let resp = client
        .get(format!("{base}/v1/tracing/runs/run-1/spans/missing"))
        .send()
        .await
        .expect("missing span GET");
    assert_eq!(resp.status(), 404);

    // ────────────────────────────────────────────────────────────────────
    // Session-scoped surface — exercises the `polaris.label.session_id`
    // convention and the membership-validated wrappers.
    // ────────────────────────────────────────────────────────────────────

    // -- /v1/sessions/{id}/runs filters to that session's runs ----------
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/runs"))
        .send()
        .await
        .expect("sess-A runs GET");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("sess-A runs body");
    let items = body["items"].as_array().expect("items array");
    let run_ids: Vec<&str> = items
        .iter()
        .filter_map(|item| item["run_id"].as_str())
        .collect();
    assert_eq!(
        run_ids,
        vec!["run-A2", "run-A1"],
        "session-scoped run list must show only this session's runs, most-recent-first",
    );
    let session_label = items[0]["labels"]["session_id"]
        .as_str()
        .expect("labels.session_id must surface on the wire");
    assert_eq!(session_label, "sess-A");

    // -- /v1/sessions/{id}/runs/{run}/tree validates membership ---------
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/runs/run-A1/tree"))
        .send()
        .await
        .expect("sess-A run-A1 tree GET");
    assert_eq!(resp.status(), 200);
    let tree: Value = resp.json().await.expect("tree body");
    assert_eq!(tree["run_id"], "run-A1");
    assert_eq!(tree["labels"]["session_id"], "sess-A");

    // Cross-session lookup must 404, even when both the session and run
    // exist in the buffer.
    let resp = client
        .get(format!("{base}/v1/sessions/sess-A/runs/run-B1/tree"))
        .send()
        .await
        .expect("cross-session tree GET");
    assert_eq!(resp.status(), 404);

    // -- /v1/sessions/{id}/runs/{run}/spans/{span} validates membership -
    let resp = client
        .get(format!(
            "{base}/v1/sessions/sess-A/runs/run-A1/spans/root-A1"
        ))
        .send()
        .await
        .expect("sess-A span GET");
    assert_eq!(resp.status(), 200);
    let span: Value = resp.json().await.expect("span body");
    assert_eq!(span["span_id"], "root-A1");
    assert_eq!(span["labels"]["session_id"], "sess-A");

    let resp = client
        .get(format!(
            "{base}/v1/sessions/sess-A/runs/run-B1/spans/root-B1"
        ))
        .send()
        .await
        .expect("cross-session span GET");
    assert_eq!(resp.status(), 404);

    server.cleanup().await;
}

//! Measurement test for the `?include=payloads` default on
//! `GET /v1/tracing/runs/{run_id}`.
//!
//! Builds synthetic but ReAct-shaped runs in `SpanBuffer`, asks
//! `SpanBuffer::run_tree` for the payload-embedded tree, and reports
//! the wire size distribution. The default stays at `payloads` while
//! p95 stays under 1 MB.
//!
//! Run with: `cargo test -p polaris_core_plugins --features dashboard
//! --test tracing_run_tree_p95 -- --ignored --nocapture`.

#![cfg(feature = "dashboard")]

use polaris_core_plugins::{SpanBuffer, SpanKind, SpanRecord, TreeView};
use serde_json::{Value, json};

/// Roughly imitates one turn of a `ReAct` loop:
/// - 1 root `polaris.graph.execute` span,
/// - `system_spans` nested `polaris.graph.execute_system` spans,
/// - `events_per_system` events per system span, each carrying an
///   LLM-shaped payload (`gen_ai.input.messages`, `gen_ai.output.messages`,
///   `gen_ai.tool.call.arguments`).
///
/// Sizes are intentionally pessimistic — each event embeds ~3 KB of
/// payload (an LLM call's worth) which is at the upper end of what
/// `tracing_opentelemetry` typically emits.
fn populate_run(buffer: &SpanBuffer, run_id: &str, system_spans: usize, events_per_system: usize) {
    let ts = "2026-05-15T12:00:00.000Z";
    let agent_fields = ("polaris.session.agent_type", Value::String("react".into()));

    let root_span_id = format!("{run_id}-root");
    buffer.push(
        SpanRecord::new(
            ts,
            "info",
            "polaris.graph",
            "polaris.graph.execute",
            SpanKind::SpanClose,
        )
        .with_started_at("2026-05-15T11:59:59.000Z")
        .with_duration_ms(12_500)
        .with_run_id(run_id)
        .with_span_id(&root_span_id)
        .with_field(agent_fields.0, agent_fields.1.clone()),
    );

    for sys_idx in 0..system_spans {
        let sys_span_id = format!("{run_id}-sys-{sys_idx}");
        buffer.push(
            SpanRecord::new(
                ts,
                "info",
                "polaris.graph",
                "polaris.graph.execute_system",
                SpanKind::SpanClose,
            )
            .with_started_at("2026-05-15T11:59:59.500Z")
            .with_duration_ms(900)
            .with_run_id(run_id)
            .with_span_id(&sys_span_id)
            .with_parent_span_id(&root_span_id)
            .with_field(agent_fields.0, agent_fields.1.clone()),
        );

        for evt_idx in 0..events_per_system {
            let llm_input = synthetic_llm_messages(5, 480);
            let llm_output = synthetic_llm_messages(2, 1100);
            let tool_args = synthetic_tool_args(200);
            buffer.push(
                SpanRecord::new(ts, "info", "polaris.models", "gen_ai.chat", SpanKind::Event)
                    .with_run_id(run_id)
                    .with_parent_span_id(&sys_span_id)
                    .with_message(format!("chat completion #{sys_idx}-{evt_idx}"))
                    .with_field("gen_ai.input.messages", llm_input)
                    .with_field("gen_ai.output.messages", llm_output)
                    .with_field("gen_ai.tool.call.arguments", tool_args)
                    .with_field(
                        "gen_ai.request.model",
                        Value::String("claude-opus-4-7".into()),
                    ),
            );
        }
    }
}

fn synthetic_llm_messages(count: usize, chars: usize) -> Value {
    let body: String = std::iter::repeat_n('x', chars).collect();
    Value::Array(
        (0..count)
            .map(|i| {
                json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": body,
                })
            })
            .collect(),
    )
}

fn synthetic_tool_args(chars: usize) -> Value {
    let blob: String = std::iter::repeat_n('a', chars).collect();
    json!({ "path": blob, "mode": "read" })
}

#[test]
#[ignore = "measurement-only — run with --ignored --nocapture"]
#[expect(
    clippy::print_stdout,
    reason = "measurement output is the point of this --ignored test"
)]
fn run_tree_payload_p95_under_1mb() {
    // 200 distinct runs with varying density. The mix mirrors what a
    // typical interactive ReAct session looks like over an hour: many
    // short turns, a handful of long deliberative ones.
    let buffer = SpanBuffer::with_capacity(200_000);
    let mut sizes_bytes: Vec<usize> = Vec::with_capacity(200);

    for idx in 0..200 {
        let run_id = format!("run-{idx:03}");
        // Mostly short runs (≤5 system spans), occasional long ones
        // (up to 30) for the upper tail.
        let (system_spans, events_per_system) = match idx % 20 {
            0 => (30, 5), // 5% — deliberative chain
            1..=3 => (15, 4),
            4..=8 => (8, 3),
            _ => (3, 2), // most turns are short
        };
        populate_run(&buffer, &run_id, system_spans, events_per_system);

        let tree = buffer
            .run_tree(&run_id, TreeView::Payloads)
            .expect("tree present");
        let wire = serde_json::to_vec(&tree).expect("serialize");
        sizes_bytes.push(wire.len());
    }

    sizes_bytes.sort_unstable();
    let min = *sizes_bytes.first().unwrap();
    let p50 = sizes_bytes[sizes_bytes.len() / 2];
    let p95 = sizes_bytes[(sizes_bytes.len() * 95) / 100];
    let max = *sizes_bytes.last().unwrap();

    println!(
        "run_tree wire size (payloads embedded) over {} runs",
        sizes_bytes.len()
    );
    println!("  min   = {min:>10} bytes ({:.1} KiB)", min as f64 / 1024.0);
    println!("  p50   = {p50:>10} bytes ({:.1} KiB)", p50 as f64 / 1024.0);
    println!("  p95   = {p95:>10} bytes ({:.1} KiB)", p95 as f64 / 1024.0);
    println!("  max   = {max:>10} bytes ({:.1} KiB)", max as f64 / 1024.0);

    assert!(
        p95 < 1_048_576,
        "p95 must stay under 1 MiB to justify the payload-embedded default; got {p95} bytes"
    );
}

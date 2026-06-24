//! Graph execution tracing middleware.
//!
//! Registers tracing spans around all graph execution targets
//! (systems, loops, parallel branches, decisions, switches).
//!
//! Decision and switch *outcomes* (which branch / case was taken) are
//! recorded onto the active span via [`register_outcome_hooks`], which
//! subscribes to `OnDecisionComplete` / `OnSwitchComplete`. The hooks fire
//! inside the middleware's instrumented future, so `Span::current()` is the
//! span created above and `Span::record` lands the attribute on it.

use polaris_graph::MiddlewareAPI;
use polaris_graph::hooks::schedule::{OnDecisionComplete, OnSwitchComplete};
use polaris_graph::hooks::{GraphEvent, HooksAPI};
use tracing::Instrument;
use tracing::field::Empty;

/// Registers tracing middleware on all graph execution targets.
pub(crate) fn register(mw: &MiddlewareAPI) {
    mw.register_graph_execution("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute",
            polaris.graph.node_count = info.node_count,
            polaris.graph.run.id = info.run_id.as_str(),
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_system("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_system",
            polaris.graph.system.name = info.node_name,
            polaris.graph.system.node_id = %info.node_id,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_loop("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_loop",
            polaris.graph.loop.name = info.node_name,
            polaris.graph.loop.max_iterations = info.max_iterations,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_loop_iteration("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.loop_iteration",
            polaris.graph.loop.iteration = info.iteration,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_parallel("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_parallel",
            polaris.graph.parallel.name = info.node_name,
            polaris.graph.parallel.branch_count = info.branch_count,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_parallel_branch("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.parallel_branch",
            polaris.graph.parallel.branch_index = info.branch_index,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_decision("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_decision",
            polaris.graph.decision.name = info.node_name,
            polaris.graph.decision.branch_index = Empty,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_switch("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_switch",
            polaris.graph.switch.name = info.node_name,
            polaris.graph.switch.case_index = Empty,
            polaris.graph.switch.used_default = Empty,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });
}

/// Registers hooks that record decision/switch outcomes onto the active span.
///
/// Fires inside the middleware-instrumented future, so `Span::current()` is
/// the `polaris.graph.execute_decision` / `polaris.graph.execute_switch` span
/// created in [`register`]. The matching fields are declared with
/// [`tracing::field::Empty`] there so `record` can land them.
pub(crate) fn register_outcome_hooks(hooks: &HooksAPI) {
    hooks
        .register_observer::<OnDecisionComplete, _>(
            "tracing.decision_outcome",
            |event: &GraphEvent| {
                if let GraphEvent::DecisionComplete {
                    selected_branch, ..
                } = event
                {
                    tracing::Span::current()
                        .record("polaris.graph.decision.branch_index", *selected_branch);
                }
            },
        )
        .expect("tracing.decision_outcome hook must register");

    hooks
        .register_observer::<OnSwitchComplete, _>("tracing.switch_outcome", |event: &GraphEvent| {
            if let GraphEvent::SwitchComplete {
                selected_case,
                used_default,
                ..
            } = event
            {
                let span = tracing::Span::current();
                span.record("polaris.graph.switch.case_index", *selected_case);
                span.record("polaris.graph.switch.used_default", *used_default);
            }
        })
        .expect("tracing.switch_outcome hook must register");
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use polaris_graph::MiddlewareAPI;
    use polaris_graph::executor::GraphExecutor;
    use polaris_graph::graph::Graph;
    use polaris_system::param::SystemContext;
    use polaris_system::system::{BoxFuture, System, SystemError};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    /// No-op system so a single-node graph has something to execute.
    struct NoopSystem;

    impl System for NoopSystem {
        type Output = ();

        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async move { Ok(()) })
        }

        fn name(&self) -> &'static str {
            "noop_system"
        }
    }

    /// Records the value of `polaris.graph.run.id` on the
    /// `polaris.graph.execute` span when the span is created.
    #[derive(Clone, Default)]
    struct RunIdCapture(Arc<Mutex<Option<String>>>);

    impl RunIdCapture {
        fn recorded(&self) -> Option<String> {
            self.0.lock().clone()
        }
    }

    /// Field visitor that pulls `polaris.graph.run.id` out of a span's attributes.
    struct RunIdVisitor(HashMap<String, String>);

    impl Visit for RunIdVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "polaris.graph.run.id" {
                self.0
                    .insert("polaris.graph.run.id".to_string(), value.to_string());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
            // `run.id` is recorded as `&str` (via `record_str`), but capture the
            // debug form as a fallback so the test fails loudly with the actual
            // value rather than silently missing it if the recording type changes.
            if field.name() == "polaris.graph.run.id" {
                self.0
                    .entry("polaris.graph.run.id".to_string())
                    .or_insert_with(|| format!("{value:?}"));
            }
        }
    }

    impl<S> tracing_subscriber::Layer<S> for RunIdCapture
    where
        S: tracing::Subscriber,
    {
        fn on_new_span(
            &self,
            attrs: &span::Attributes<'_>,
            _id: &span::Id,
            _ctx: LayerContext<'_, S>,
        ) {
            if attrs.metadata().name() != "polaris.graph.execute" {
                return;
            }
            let mut visitor = RunIdVisitor(HashMap::new());
            attrs.record(&mut visitor);
            if let Some(run_id) = visitor.0.remove("polaris.graph.run.id") {
                *self.0.lock() = Some(run_id);
            }
        }
    }

    #[tokio::test]
    async fn graph_execute_span_records_run_id() {
        // Drive a real graph execution through the tracing middleware installed
        // by `register`, and assert the `polaris.graph.execute` span carries
        // `polaris.graph.run.id` equal to the run id the executor minted (which
        // is also surfaced on the `ExecutionResult`).
        let capture = RunIdCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        // Use the free `set_default` (thread-local, scoped) rather than
        // `SubscriberInitExt::set_default`, which would also globally install the
        // `log` bridge and make a later `TracingLayers::install()` panic with
        // `SetLoggerError` when the full crate test suite runs in one binary.
        let _guard = tracing::subscriber::set_default(subscriber);

        let mw = MiddlewareAPI::new();
        register(&mw);

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(NoopSystem));

        let mut ctx = SystemContext::new();
        let result = GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(&mw))
            .await
            .expect("graph execution should succeed");

        let recorded = capture
            .recorded()
            .expect("polaris.graph.execute span should record polaris.graph.run.id");
        assert_eq!(
            recorded,
            result.run_id().as_str(),
            "span run id should match the run id minted for the execution"
        );
        assert!(
            !recorded.is_empty(),
            "recorded run id should be non-empty: {recorded:?}"
        );
    }
}

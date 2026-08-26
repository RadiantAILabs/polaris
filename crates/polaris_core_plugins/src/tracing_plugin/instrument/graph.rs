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
    use crate::test_support::{FieldMapVisitor, set_default_and_rebuild};
    use crate::{InspectionAPI, InspectionPlugin};
    use parking_lot::Mutex;
    use polaris_graph::MiddlewareAPI;
    use polaris_graph::executor::GraphExecutor;
    use polaris_graph::graph::Graph;
    use polaris_system::param::{Res, SystemContext};
    use polaris_system::resource::LocalResource;
    use polaris_system::server::Server;
    use polaris_system::system;
    use polaris_system::system::{BoxFuture, System, SystemError};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tracing::Level;
    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
    use tracing_subscriber::registry::{LookupSpan, Registry};

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

    /// Captures `polaris.graph.system.name` from `polaris.graph.execute_system` spans.
    #[derive(Clone, Default)]
    struct SystemNameCapture(Arc<Mutex<Option<String>>>);

    impl SystemNameCapture {
        fn recorded(&self) -> Option<String> {
            self.0.lock().clone()
        }
    }

    struct SystemNameVisitor(Option<String>);

    impl Visit for SystemNameVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "polaris.graph.system.name" {
                self.0 = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
            if field.name() == "polaris.graph.system.name" {
                self.0.get_or_insert_with(|| format!("{value:?}"));
            }
        }
    }

    impl<S> tracing_subscriber::Layer<S> for SystemNameCapture
    where
        S: tracing::Subscriber,
    {
        fn on_new_span(
            &self,
            attrs: &span::Attributes<'_>,
            _id: &span::Id,
            _ctx: LayerContext<'_, S>,
        ) {
            if attrs.metadata().name() != "polaris.graph.execute_system" {
                return;
            }
            let mut visitor = SystemNameVisitor(None);
            attrs.record(&mut visitor);
            if let Some(name) = visitor.0 {
                *self.0.lock() = Some(name);
            }
        }
    }

    #[tokio::test]
    async fn execute_system_span_records_system_name() {
        // Drive a single-system graph through the tracing middleware and assert
        // that `polaris.graph.execute_system` carries the correct system name.
        let capture = SystemNameCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let mw = MiddlewareAPI::new();
        register(&mw);

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(NoopSystem));

        let mut ctx = SystemContext::new();
        GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(&mw))
            .await
            .expect("graph execution should succeed");

        let recorded = capture
            .recorded()
            .expect("polaris.graph.execute_system span should record polaris.graph.system.name");
        assert_eq!(
            recorded, "noop_system",
            "span system name should match the system's name() return value"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Inspection events correlate to the per-step span
    // ─────────────────────────────────────────────────────────────────────────

    #[derive(Debug)]
    struct Marker {
        value: i32,
    }

    impl LocalResource for Marker {}

    #[system(inspect(marker))]
    async fn observed(marker: Res<Marker>) {
        let _ = marker.value;
    }

    /// One captured `polaris::inspection` event: its fields, level, and the
    /// name of the span it fired inside.
    #[derive(Clone, Debug)]
    struct CapturedEvent {
        fields: HashMap<String, String>,
        level: Level,
        parent_span: Option<String>,
    }

    /// Captures every `polaris::inspection` event the subscriber sees.
    #[derive(Clone, Default)]
    struct InspectionEventCapture(Arc<Mutex<Vec<CapturedEvent>>>);

    impl InspectionEventCapture {
        fn first(&self) -> Option<CapturedEvent> {
            self.0.lock().first().cloned()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for InspectionEventCapture
    where
        S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, ctx: LayerContext<'_, S>) {
            if event.metadata().target() != "polaris::inspection" {
                return;
            }
            let mut visitor = FieldMapVisitor::default();
            event.record(&mut visitor);
            self.0.lock().push(CapturedEvent {
                fields: visitor.0,
                level: *event.metadata().level(),
                parent_span: ctx.event_span(event).map(|s| s.name().to_owned()),
            });
        }
    }

    #[tokio::test]
    async fn inspection_records_emit_inside_the_execute_system_span() {
        // Thread-local (scoped `set_default`) rather than a global install:
        // the current-thread runtime keeps the run on this thread, and a
        // global subscriber would clash with other tests in the same binary.
        // The install rebuilds callsite interest so a sibling test that reached
        // this callsite unsubscribed cannot have cached it as disabled.
        let capture = InspectionEventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        // Production wiring: `InspectionPlugin::build()` preloads the shipped
        // `TracingInspectionSink` and registers the fan-out middleware — on a
        // `MiddlewareAPI` that already exists, exercising the reuse branch.
        let mut server = Server::new();
        server.insert_api(MiddlewareAPI::new());
        server.add_plugins(InspectionPlugin::default());
        server.finish().await.expect("server must build");

        let mw = server
            .api::<MiddlewareAPI>()
            .expect("MiddlewareAPI was inserted above");
        // This module's real per-step span middleware, on the same instance.
        register(mw);
        server
            .api::<InspectionAPI>()
            .expect("InspectionPlugin provides InspectionAPI")
            .enable();

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(observed()));

        let mut ctx = server.create_context();
        ctx.insert(Marker { value: 7 });
        GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(mw))
            .await
            .expect("graph execution should succeed");

        let event = capture
            .first()
            .expect("the shipped TracingInspectionSink must emit a polaris::inspection event");
        assert_eq!(event.level, Level::INFO);
        assert_eq!(
            event.parent_span.as_deref(),
            Some("polaris.graph.execute_system"),
            "the event must fire inside the per-step span"
        );
        let field = |name: &str| event.fields.get(name).map(String::as_str);
        assert_eq!(field("polaris.inspection.system"), Some("observed"));
        assert_eq!(field("polaris.inspection.param"), Some("marker"));
        assert_eq!(field("polaris.inspection.type_name"), Some("Marker"));
        assert_eq!(field("polaris.inspection.kind"), Some("Res"));
        assert_eq!(
            field("polaris.inspection.phase"),
            Some("Before"),
            "an input capture must identify that it ran before the system"
        );
        assert_eq!(
            field("polaris.inspection.rendering"),
            Some("text"),
            "a rendered value must be labelled as one"
        );
        assert_eq!(
            field("polaris.inspection.value"),
            Some(r#""Marker { value: 7 }""#),
            "the value field must carry the parameter's exact Debug rendering, \
             itself emitted through Debug so the log cannot be forged"
        );
        assert_eq!(field("polaris.inspection.truncated"), Some("false"));
    }
}

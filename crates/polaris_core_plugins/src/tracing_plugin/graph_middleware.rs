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
pub(super) fn register(mw: &MiddlewareAPI) {
    mw.register_graph_execution("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute",
            polaris.graph.node_count = info.node_count,
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
            polaris.graph.decision.branch = Empty,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_switch("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_switch",
            polaris.graph.switch.name = info.node_name,
            polaris.graph.switch.case = Empty,
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
pub(super) fn register_outcome_hooks(hooks: &HooksAPI) {
    hooks
        .register_observer::<OnDecisionComplete, _>(
            "tracing.decision_outcome",
            |event: &GraphEvent| {
                if let GraphEvent::DecisionComplete {
                    selected_branch, ..
                } = event
                {
                    tracing::Span::current()
                        .record("polaris.graph.decision.branch", *selected_branch);
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
                span.record("polaris.graph.switch.case", *selected_case);
                span.record("polaris.graph.switch.used_default", *used_default);
            }
        })
        .expect("tracing.switch_outcome hook must register");
}

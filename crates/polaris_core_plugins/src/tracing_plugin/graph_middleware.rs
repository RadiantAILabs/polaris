//! Graph execution tracing middleware.
//!
//! Registers tracing spans around all graph execution targets
//! (systems, loops, parallel branches, decisions, switches).

use polaris_graph::MiddlewareAPI;
use tracing::Instrument;

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
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });

    mw.register_switch("tracing", |info, ctx, next| {
        let span = tracing::info_span!(
            "polaris.graph.execute_switch",
            polaris.graph.switch.name = info.node_name,
        );
        Box::pin(async move { next.run(ctx).await }.instrument(span))
    });
}

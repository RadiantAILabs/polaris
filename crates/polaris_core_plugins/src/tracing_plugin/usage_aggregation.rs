//! Rolls descendant `chat` usage and cost up onto the enclosing turn span.
//!
//! `chat` spans record `gen_ai.usage.*` (including `gen_ai.usage.cost`) on
//! themselves as generations complete. [`UsageAggregationLayer`] observes those
//! records and adds them into a [`UsageAccumulator`] held in the extensions of
//! every enclosing `polaris.session.turn` span, so each turn span accumulates
//! its whole subtree's usage. The sessions layer then reads the total via
//! [`record_turn_usage`] and stamps the usage and cost attributes on the span
//! before it closes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::{LookupSpan, Registry};

/// Span name identifying an agent invocation turn.
const TURN_SPAN: &str = "polaris.session.turn";

/// Running totals of a turn's descendant LLM usage and cost.
///
/// Uses atomics so it can be mutated through the shared reference handed out by
/// a span's (read-locked) extensions.
#[derive(Debug, Default)]
pub(crate) struct UsageAccumulator {
    seen: AtomicBool,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    cache_read_tokens: AtomicU64,
    cache_creation_tokens: AtomicU64,
    cost_usd_bits: AtomicU64,
}

impl UsageAccumulator {
    fn add(&self, delta: &UsageDelta) {
        self.seen.store(true, Ordering::Relaxed);
        self.input_tokens
            .fetch_add(delta.input_tokens, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(delta.output_tokens, Ordering::Relaxed);
        self.cache_read_tokens
            .fetch_add(delta.cache_read_tokens, Ordering::Relaxed);
        self.cache_creation_tokens
            .fetch_add(delta.cache_creation_tokens, Ordering::Relaxed);
        if delta.cost_usd > 0.0 {
            self.add_cost(delta.cost_usd);
        }
    }

    fn add_cost(&self, delta: f64) {
        let mut current = self.cost_usd_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self.cost_usd_bits.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn snapshot(&self) -> UsageTotals {
        UsageTotals {
            seen: self.seen.load(Ordering::Relaxed),
            input_tokens: self.input_tokens.load(Ordering::Relaxed),
            output_tokens: self.output_tokens.load(Ordering::Relaxed),
            cache_read_tokens: self.cache_read_tokens.load(Ordering::Relaxed),
            cache_creation_tokens: self.cache_creation_tokens.load(Ordering::Relaxed),
            cost_usd: f64::from_bits(self.cost_usd_bits.load(Ordering::Relaxed)),
        }
    }
}

/// One generation's usage, extracted from a `chat` span's field record.
#[derive(Debug, Default)]
struct UsageDelta {
    seen: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
}

/// A read-time snapshot of a [`UsageAccumulator`].
#[derive(Debug, Default)]
struct UsageTotals {
    seen: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
}

/// Visitor that pulls the usage and cost fields out of a span record.
#[derive(Default)]
struct UsageVisitor(UsageDelta);

impl UsageVisitor {
    fn assign_tokens(&mut self, name: &str, value: u64) {
        let slot = match name {
            "gen_ai.usage.input_tokens" => &mut self.0.input_tokens,
            "gen_ai.usage.output_tokens" => &mut self.0.output_tokens,
            "gen_ai.usage.cache_read.input_tokens" => &mut self.0.cache_read_tokens,
            "gen_ai.usage.cache_creation.input_tokens" => &mut self.0.cache_creation_tokens,
            _ => return,
        };
        *slot = value;
        self.0.seen = true;
    }
}

impl Visit for UsageVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.assign_tokens(field.name(), value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(value) = u64::try_from(value) {
            self.assign_tokens(field.name(), value);
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "gen_ai.usage.cost" && value.is_finite() && value > 0.0 {
            self.0.cost_usd = value;
            self.0.seen = true;
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// Layer that accumulates descendant `chat` usage onto enclosing
/// `polaris.session.turn` spans. Installed by
/// [`TracingPlugin`](super::TracingPlugin).
pub(crate) struct UsageAggregationLayer;

impl<S> Layer<S> for UsageAggregationLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() != TURN_SPAN {
            return;
        }
        if let Some(span) = ctx.span(id) {
            // `on_new_span` fires once per span, so a plain insert is sufficient.
            span.extensions_mut()
                .insert(Arc::new(UsageAccumulator::default()));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        // A turn span's own finalized aggregate (stamped by `record_turn_usage`)
        // must not be re-propagated upward — that would double-count nested
        // agent runs.
        if span.name() == TURN_SPAN {
            return;
        }

        let mut visitor = UsageVisitor::default();
        values.record(&mut visitor);
        let delta = visitor.0;
        if !delta.seen {
            return;
        }

        // Add to every enclosing turn span so each carries its whole subtree's
        // total. Reading a nested agent's own span reports only its subtree; no
        // consumer sums across runs, so this never double-counts.
        if let Some(scope) = ctx.span_scope(id) {
            for ancestor in scope {
                if let Some(accumulator) = ancestor.extensions().get::<Arc<UsageAccumulator>>() {
                    accumulator.add(&delta);
                }
            }
        }
    }
}

/// Stamps the accumulated usage and cost onto the current turn span. No-op
/// when the layer is not installed or nothing was accumulated.
///
/// Call this while the `polaris.session.turn` span is still live (before it
/// closes), passing the span whose subtree usage should be finalized.
#[doc(hidden)]
pub fn record_turn_usage(span: &tracing::Span) {
    let totals = span
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<Registry>()?;
            let span_ref = registry.span(id)?;
            let accumulator = span_ref
                .extensions()
                .get::<Arc<UsageAccumulator>>()?
                .clone();
            Some(accumulator.snapshot())
        })
        .flatten();

    let Some(totals) = totals else {
        return;
    };
    if !totals.seen {
        return;
    }

    span.record("gen_ai.usage.input_tokens", totals.input_tokens);
    span.record("gen_ai.usage.output_tokens", totals.output_tokens);
    if totals.cache_read_tokens > 0 {
        span.record(
            "gen_ai.usage.cache_read.input_tokens",
            totals.cache_read_tokens,
        );
    }
    if totals.cache_creation_tokens > 0 {
        span.record(
            "gen_ai.usage.cache_creation.input_tokens",
            totals.cache_creation_tokens,
        );
    }
    if totals.cost_usd > 0.0 {
        span.record("gen_ai.usage.cost", totals.cost_usd);
    }
    span.record("polaris.usage.aggregate", true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc as StdArc;
    use tracing::field::Empty;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::Registry;

    #[test]
    fn accumulator_sums_tokens_and_cost() {
        let acc = UsageAccumulator::default();
        acc.add(&UsageDelta {
            seen: true,
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.25,
            ..Default::default()
        });
        acc.add(&UsageDelta {
            seen: true,
            input_tokens: 3,
            output_tokens: 7,
            cache_read_tokens: 2,
            cost_usd: 0.25,
            ..Default::default()
        });

        let totals = acc.snapshot();
        assert_eq!(
            totals.input_tokens, 13,
            "input tokens sum across generations"
        );
        assert_eq!(
            totals.output_tokens, 12,
            "output tokens sum across generations"
        );
        assert_eq!(
            totals.cache_read_tokens, 2,
            "cache-read tokens carry through"
        );
        assert!(
            (totals.cost_usd - 0.5).abs() < 1e-9,
            "cost sums across generations"
        );
    }

    #[test]
    fn empty_accumulator_is_unseen() {
        assert!(
            !UsageAccumulator::default().snapshot().seen,
            "an accumulator with no additions reports nothing seen"
        );
    }

    /// Captures field values recorded on `polaris.session.turn` spans, so the
    /// finalized aggregate can be asserted after `record_turn_usage`.
    #[derive(Clone, Default)]
    struct TurnSpanCapture(StdArc<Mutex<HashMap<String, u64>>>);

    struct U64Visitor<'a>(&'a mut HashMap<String, u64>);

    impl Visit for U64Visitor<'_> {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value);
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            if let Ok(value) = u64::try_from(value) {
                self.0.insert(field.name().to_string(), value);
            }
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    impl<S> Layer<S> for TurnSpanCapture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
            if ctx.span(id).is_some_and(|s| s.name() == TURN_SPAN) {
                let mut map = self.0.lock();
                values.record(&mut U64Visitor(&mut map));
            }
        }
    }

    #[test]
    fn descendant_chat_usage_rolls_up_to_turn_span() {
        let capture = TurnSpanCapture::default();
        let subscriber = Registry::default()
            .with(UsageAggregationLayer)
            .with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let turn = tracing::info_span!(
            "polaris.session.turn",
            gen_ai.usage.input_tokens = Empty,
            gen_ai.usage.output_tokens = Empty,
            polaris.usage.aggregate = Empty,
        );
        let entered = turn.enter();

        // Two nested chat spans, each recording usage on themselves.
        for (input, output) in [(10_u64, 20_u64), (5, 7)] {
            let chat = tracing::info_span!(
                "chat",
                gen_ai.usage.input_tokens = Empty,
                gen_ai.usage.output_tokens = Empty,
            );
            let _c = chat.enter();
            chat.record("gen_ai.usage.input_tokens", input);
            chat.record("gen_ai.usage.output_tokens", output);
        }

        record_turn_usage(&turn);
        drop(entered);

        let map = capture.0.lock();
        assert_eq!(
            map.get("gen_ai.usage.input_tokens").copied(),
            Some(15),
            "turn span aggregates input tokens from both chats"
        );
        assert_eq!(
            map.get("gen_ai.usage.output_tokens").copied(),
            Some(27),
            "turn span aggregates output tokens from both chats"
        );
    }
}

//! Helpers shared across this crate's unit tests.

use std::collections::HashMap;
use tracing::field::{Field, Visit};

/// Collects every field of a tracing event into a name → rendered-value map.
#[derive(Default)]
pub(crate) struct FieldMapVisitor(pub(crate) HashMap<String, String>);

impl Visit for FieldMapVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Installs `subscriber` for this thread, then re-evaluates callsite interest.
///
/// Use this instead of a bare [`tracing::subscriber::set_default`] in any test
/// whose assertion depends on an event actually being dispatched.
///
/// `tracing` caches a callsite's `Interest` the first time that callsite is
/// hit, and the cache is process-wide. A test that reaches one of our
/// `event!` callsites with no subscriber installed — say, by dropping a
/// redaction rule while exercising something else — caches
/// `Interest::never()` for it, after which a *thread-local* subscriber
/// installed later never sees the event at all. That breaks tests two ways:
/// one asserting the event arrives fails, and one asserting the event is
/// absent passes without testing anything.
///
/// Rebuilding after the install re-evaluates every registered callsite against
/// the dispatchers currently registered, which now includes this one. Ordering
/// matters: rebuild *after* `set_default`, or the dispatcher being rebuilt for
/// is not yet registered.
pub(crate) fn set_default_and_rebuild<S>(subscriber: S) -> tracing::subscriber::DefaultGuard
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    install_dispatcher_floor();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    guard
}

/// Keeps one do-nothing dispatcher registered for the whole test binary.
///
/// Scoped subscribers come and go as tests start and finish, and `tracing`
/// recomputes both the callsite interest cache and the global max level from
/// whichever dispatchers are registered at that moment. When the last one
/// unregisters those collapse to "nothing is enabled" — and a test installing
/// its own subscriber concurrently can lose that write, leaving spans and
/// events silently dropped on a thread that *does* have a subscriber. It shows
/// up as a span assertion failing a few times in a thousand runs.
///
/// A floor dispatcher that never unregisters keeps the count above zero, so
/// that collapse never happens. It reports [`Interest::sometimes`] so every
/// callsite stays undecided and is resolved per call against the thread's
/// current subscriber, and `enabled` returns `false` so it never records
/// anything itself.
fn install_dispatcher_floor() {
    static FLOOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    // Constructing a `Dispatch` is what enters it into the registry that
    // interest and max-level are computed from; keeping it alive forever is
    // what keeps it there. Deliberately *not* `set_global_default` — that slot
    // belongs to `TracingPlugin::ready()`, which expects to win it.
    FLOOR.get_or_init(|| tracing::Dispatch::new(FloorSubscriber));
}

/// The floor dispatcher installed by [`install_dispatcher_floor`].
struct FloorSubscriber;

impl tracing::Subscriber for FloorSubscriber {
    fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

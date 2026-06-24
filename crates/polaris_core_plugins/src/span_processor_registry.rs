//! Build-time registry for extra OpenTelemetry [`SpanProcessor`]s.
//!
//! [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin) provides this registry in
//! its `build()` phase and installs a single fan-out processor on the
//! `SdkTracerProvider`. Downstream plugins add their own processors through
//! [`Extends`](polaris_system::plugin::Extends) during their `build()`.
//!
//! The fan-out is necessary because the provider is built before any extender
//! runs: the resolver orders a provider ahead of its extenders, and an
//! `SdkTracerProvider` is fixed once built. Installing one processor that
//! forwards to a shared list lets extenders keep pushing to that list and still
//! reach the provider that was already built.

use opentelemetry::Context;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use parking_lot::Mutex;
use polaris_system::plugin::{Contract, Version};
use std::sync::Arc;
use std::time::Duration;

/// State shared between the registry and the fan-out on the provider.
///
/// Holds the contributed processors plus the [`Resource`] the provider hands the
/// fan-out via `set_resource` at build time.
#[derive(Default)]
struct Shared {
    processors: Vec<Box<dyn SpanProcessor + 'static>>,
    resource: Option<Resource>,
}

type SharedState = Arc<Mutex<Shared>>;

/// Build-time resource that collects extra [`SpanProcessor`]s.
///
/// Provided by [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin). Plugins that
/// depend on it can push processors during their own `build()` phase.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use opentelemetry::Context;
/// use opentelemetry_sdk::error::OTelSdkResult;
/// use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
/// use polaris_system::plugin;
/// use polaris_system::plugin::{Extends, Plugin};
/// use polaris_core_plugins::SpanProcessorRegistry;
///
/// #[derive(Debug)]
/// struct MyProcessor;
/// impl SpanProcessor for MyProcessor {
///     fn on_start(&self, _: &mut Span, _: &Context) {}
///     fn on_end(&self, _: SpanData) {}
///     fn force_flush(&self) -> OTelSdkResult { Ok(()) }
///     fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult { Ok(()) }
/// }
///
/// struct MyPlugin;
///
/// #[plugin(id = "my::plugin", version = "0.1.0")]
/// impl Plugin for MyPlugin {
///     fn build(&self, mut registry: Extends<SpanProcessorRegistry>) {
///         registry.push(MyProcessor);
///     }
/// }
/// ```
pub struct SpanProcessorRegistry {
    state: SharedState,
}

impl SpanProcessorRegistry {
    /// Empty registry. Inserted by
    /// [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin) during `build()`.
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// Contribute a [`SpanProcessor`].
    ///
    /// Call this from a consumer plugin's `build()` phase, while holding an
    /// [`Extends<SpanProcessorRegistry>`](polaris_system::plugin::Extends). The
    /// registry is effectively frozen once the server finishes building: the
    /// fan-out installed on the provider reads the live processor list, so a
    /// processor pushed after `build()` (e.g. during `ready()` or at runtime)
    /// still receives `on_end`, **but** the provider's `force_flush`/`shutdown`
    /// at [`cleanup()`](crate::OpenTelemetryPlugin) is not guaranteed to reach
    /// it — late pushes may silently drop their buffered spans on exit.
    ///
    /// Contributed processors must be **non-blocking**: every callback runs
    /// synchronously inside the fan-out while it holds an internal lock, so a
    /// processor that blocks in `on_end`/`force_flush`/`shutdown_with_timeout`
    /// stalls span export for everyone.
    pub fn push(&mut self, processor: impl SpanProcessor + 'static) {
        let mut boxed: Box<dyn SpanProcessor + 'static> = Box::new(processor);
        let mut shared = self.state.lock();
        if let Some(resource) = &shared.resource {
            boxed.set_resource(resource);
        }
        shared.processors.push(boxed);
    }

    /// A single [`SpanProcessor`] that forwards every callback to the
    /// contributed processors.
    pub(crate) fn fanout(&self) -> SpanProcessorFanout {
        SpanProcessorFanout(self.state.clone())
    }
}

impl std::fmt::Debug for SpanProcessorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanProcessorRegistry")
            .field("processors", &self.state.lock().processors.len())
            .finish()
    }
}

impl Contract for SpanProcessorRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

/// Fan-out [`SpanProcessor`] installed on the provider by
/// [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin).
///
/// Forwards every callback to each processor in the [`SpanProcessorRegistry`]
/// list, which it shares with the registry.
pub(crate) struct SpanProcessorFanout(SharedState);

impl std::fmt::Debug for SpanProcessorFanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanProcessorFanout")
            .field("processors", &self.0.lock().processors.len())
            .finish()
    }
}

impl SpanProcessor for SpanProcessorFanout {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        for processor in self.0.lock().processors.iter() {
            processor.on_start(span, cx);
        }
    }

    fn on_end(&self, span: SpanData) {
        // `on_end` consumes the span, so each contributor gets its own clone.
        for processor in self.0.lock().processors.iter() {
            processor.on_end(span.clone());
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        // The SDK calls this once at provider build, before any extender has
        // contributed. Forward to whoever is present and remember it so
        // late-pushed processors receive it too (see `SpanProcessorRegistry::push`).
        let mut shared = self.0.lock();
        for processor in shared.processors.iter_mut() {
            processor.set_resource(resource);
        }
        shared.resource = Some(resource.clone());
    }

    fn force_flush(&self) -> OTelSdkResult {
        for processor in self.0.lock().processors.iter() {
            processor.force_flush()?;
        }
        Ok(())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        for processor in self.0.lock().processors.iter() {
            processor.shutdown_with_timeout(timeout)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone)]
    struct Counter(Arc<AtomicUsize>);

    impl SpanProcessor for Counter {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, _span: SpanData) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    #[test]
    fn fanout_reaches_processors_contributed_after_capture() {
        use opentelemetry::trace::{Span as _, Tracer, TracerProvider as _};
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();
        let count = Arc::new(AtomicUsize::new(0));
        registry.push(Counter(count.clone()));

        let provider = SdkTracerProvider::builder()
            .with_span_processor(fanout)
            .build();
        let tracer = provider.tracer("test");
        tracer.start("op").end();

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "processor pushed after fan-out capture still receives on_end"
        );
    }

    /// Records the `service.name` it is handed via `set_resource`.
    #[derive(Debug, Clone)]
    struct ResourceCapture(Arc<Mutex<Option<String>>>);

    impl SpanProcessor for ResourceCapture {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, _span: SpanData) {}
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
        fn set_resource(&mut self, resource: &Resource) {
            for (key, value) in resource.iter() {
                if key.as_str() == "service.name" {
                    *self.0.lock() = Some(value.to_string());
                }
            }
        }
    }

    #[test]
    fn resource_reaches_processors_contributed_after_capture() {
        use opentelemetry::KeyValue;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();

        // Build the provider first — this fires `set_resource` on the fan-out
        // while the registry is still empty, exactly as `OpenTelemetryPlugin`
        // does before the dashboard recorder is contributed.
        let _provider = SdkTracerProvider::builder()
            .with_span_processor(fanout)
            .with_resource(
                Resource::builder_empty()
                    .with_attribute(KeyValue::new("service.name", "svc"))
                    .build(),
            )
            .build();

        let captured = Arc::new(Mutex::new(None));
        registry.push(ResourceCapture(captured.clone()));

        assert_eq!(
            captured.lock().as_deref(),
            Some("svc"),
            "a processor pushed after the provider captured its resource still receives service.name"
        );
    }

    #[test]
    fn resource_reaches_processors_present_at_capture() {
        use opentelemetry::KeyValue;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();

        // Contribute *before* the provider is built, so `set_resource` iterates
        // the already-present processor (the `iter_mut` branch in the fan-out)
        // rather than relying on the stored resource at push time.
        let captured = Arc::new(Mutex::new(None));
        registry.push(ResourceCapture(captured.clone()));

        let _provider = SdkTracerProvider::builder()
            .with_span_processor(fanout)
            .with_resource(
                Resource::builder_empty()
                    .with_attribute(KeyValue::new("service.name", "svc"))
                    .build(),
            )
            .build();

        assert_eq!(
            captured.lock().as_deref(),
            Some("svc"),
            "a processor present when the provider captures its resource receives service.name"
        );
    }

    /// Records `force_flush`/`shutdown_with_timeout` calls and can be configured
    /// to fail, so fan-out forwarding and error short-circuiting are observable.
    #[derive(Debug, Clone)]
    struct LifecycleSpy {
        flushed: Arc<AtomicUsize>,
        shutdown: Arc<AtomicUsize>,
        fail: bool,
    }

    impl LifecycleSpy {
        fn new(fail: bool) -> Self {
            Self {
                flushed: Arc::new(AtomicUsize::new(0)),
                shutdown: Arc::new(AtomicUsize::new(0)),
                fail,
            }
        }
    }

    impl SpanProcessor for LifecycleSpy {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, _span: SpanData) {}
        fn force_flush(&self) -> OTelSdkResult {
            self.flushed.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                    "forced flush failure".to_owned(),
                ));
            }
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            self.shutdown.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                    "forced shutdown failure".to_owned(),
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn fanout_force_flush_forwards_to_every_processor() {
        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();
        let first = LifecycleSpy::new(false);
        let second = LifecycleSpy::new(false);
        registry.push(first.clone());
        registry.push(second.clone());

        assert!(
            fanout.force_flush().is_ok(),
            "force_flush should succeed when no processor fails"
        );
        assert_eq!(first.flushed.load(Ordering::SeqCst), 1, "first flushed");
        assert_eq!(second.flushed.load(Ordering::SeqCst), 1, "second flushed");
    }

    #[test]
    fn fanout_force_flush_short_circuits_on_error() {
        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();
        let first = LifecycleSpy::new(false);
        let failing = LifecycleSpy::new(true);
        let after = LifecycleSpy::new(false);
        registry.push(first.clone());
        registry.push(failing.clone());
        registry.push(after.clone());

        assert!(
            fanout.force_flush().is_err(),
            "a failing processor must propagate its error"
        );
        assert_eq!(first.flushed.load(Ordering::SeqCst), 1, "first reached");
        assert_eq!(failing.flushed.load(Ordering::SeqCst), 1, "failing reached");
        assert_eq!(
            after.flushed.load(Ordering::SeqCst),
            0,
            "processor after the failure must not be flushed (short-circuit)"
        );
    }

    #[test]
    fn fanout_shutdown_forwards_and_short_circuits() {
        let mut registry = SpanProcessorRegistry::new();
        let fanout = registry.fanout();
        let first = LifecycleSpy::new(false);
        let failing = LifecycleSpy::new(true);
        let after = LifecycleSpy::new(false);
        registry.push(first.clone());
        registry.push(failing.clone());
        registry.push(after.clone());

        assert!(
            fanout
                .shutdown_with_timeout(Duration::from_secs(1))
                .is_err(),
            "a failing processor must propagate its shutdown error"
        );
        assert_eq!(first.shutdown.load(Ordering::SeqCst), 1, "first reached");
        assert_eq!(
            failing.shutdown.load(Ordering::SeqCst),
            1,
            "failing reached"
        );
        assert_eq!(
            after.shutdown.load(Ordering::SeqCst),
            0,
            "processor after the failure must not be shut down (short-circuit)"
        );
    }
}

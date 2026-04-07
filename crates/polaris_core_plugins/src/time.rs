//! Time utilities plugin and resources.
//!
//! Provides [`TimePlugin`] which registers time-related resources:
//! - [`Clock`] - Global time provider, mockable for testing
//! - [`Stopwatch`] - Per-context execution timer
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use polaris_system::server::Server;
//! use polaris_system::param::{Res, ResMut};
//! use polaris_system::system;
//! use polaris_core_plugins::{ServerInfoPlugin, TimePlugin, Clock, Stopwatch};
//!
//! // A system that tracks execution time
//! #[system]
//! async fn timed_operation(
//!     clock: Res<Clock>,
//!     mut stopwatch: ResMut<Stopwatch>,
//! ) {
//!     let start = clock.now();
//!
//!     // ... perform some work ...
//!
//!     stopwatch.lap();
//!     println!("Total elapsed: {:?}", stopwatch.elapsed());
//!     println!("Lap times: {:?}", stopwatch.laps());
//! }
//!
//! // Set up the server
//! let mut server = Server::new();
//! server.add_plugins(ServerInfoPlugin);
//! server.add_plugins(TimePlugin::default());
//! # tokio_test::block_on(async {
//! server.finish().await;
//! # });
//! ```

use crate::ServerInfoPlugin;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::resource::{GlobalResource, LocalResource};
use polaris_system::server::Server;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// ClockProvider Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for providing current time.
///
/// Implement this for custom time providers (e.g., mock clock for testing).
///
/// # Example
///
/// ```
/// use std::time::Instant;
/// use polaris_core_plugins::ClockProvider;
///
/// /// A clock that always returns a fixed instant.
/// struct FixedClock(Instant);
///
/// impl ClockProvider for FixedClock {
///     fn now(&self) -> Instant {
///         self.0
///     }
/// }
///
/// // Use with TimePlugin::with_clock()
/// let fixed = std::sync::Arc::new(FixedClock(Instant::now()));
/// ```
pub trait ClockProvider: Send + Sync + 'static {
    /// Returns the current instant.
    fn now(&self) -> Instant;
}

/// System clock provider using `std::time::Instant`.
#[derive(Debug, Clone, Copy, Default)]
struct SystemClock;

impl ClockProvider for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Clock Resource
// ─────────────────────────────────────────────────────────────────────────────

/// Time provider resource.
///
/// Global resource providing access to current time. Uses system clock
/// by default, but can be configured with a mock provider for testing.
///
/// # Methods
///
/// - [`Clock::now`] - Returns the current [`Instant`]
/// - [`Clock::elapsed_since`] - Returns duration since a given instant
///
/// # Example
///
/// ```
/// use std::time::Instant;
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_core_plugins::Clock;
///
/// #[system]
/// async fn measure_operation(clock: Res<Clock>) {
///     let start = clock.now();
///
///     // ... perform some work ...
///
///     let duration = clock.elapsed_since(start);
///     println!("Operation completed in {:?}", duration);
/// }
/// ```
pub struct Clock {
    provider: Arc<dyn ClockProvider>,
}

impl GlobalResource for Clock {}

impl Clock {
    /// Creates a Clock using the system clock.
    fn system() -> Self {
        Self {
            provider: Arc::new(SystemClock),
        }
    }

    /// Creates a Clock with a custom provider.
    fn with_provider(provider: Arc<dyn ClockProvider>) -> Self {
        Self { provider }
    }

    /// Returns the current instant.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.provider.now()
    }

    /// Returns the duration elapsed since the given instant.
    #[must_use]
    pub fn elapsed_since(&self, earlier: Instant) -> Duration {
        self.now().duration_since(earlier)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stopwatch Resource
// ─────────────────────────────────────────────────────────────────────────────

/// Per-context execution timer.
///
/// Local resource tracking elapsed time for the current execution context.
/// Each agent context gets its own fresh stopwatch instance.
///
/// # Methods
///
/// - [`Stopwatch::elapsed`] - Total time since creation
/// - [`Stopwatch::lap`] - Record a lap time
/// - [`Stopwatch::laps`] - Get all recorded lap times
/// - [`Stopwatch::reset`] - Reset the stopwatch
///
/// # Example
///
/// ```
/// use polaris_system::param::ResMut;
/// use polaris_system::system;
/// use polaris_core_plugins::Stopwatch;
///
/// #[system]
/// async fn multi_step_operation(mut stopwatch: ResMut<Stopwatch>) {
///     // Step 1
///     perform_step_one();
///     stopwatch.lap();
///
///     // Step 2
///     perform_step_two();
///     stopwatch.lap();
///
///     // Report timing
///     let laps = stopwatch.laps();
///     println!("Step 1: {:?}", laps[0]);
///     println!("Step 2: {:?}", laps[1] - laps[0]);
///     println!("Total: {:?}", stopwatch.elapsed());
/// }
///
/// fn perform_step_one() { /* ... */ }
/// fn perform_step_two() { /* ... */ }
/// ```
pub struct Stopwatch {
    start: Instant,
    laps: Vec<Duration>,
}

impl LocalResource for Stopwatch {}

impl Stopwatch {
    /// Creates a new stopwatch started at the current instant.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            laps: Vec::new(),
        }
    }

    /// Returns the duration since the stopwatch was created.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Records a lap time (duration since start).
    pub fn lap(&mut self) {
        self.laps.push(self.elapsed());
    }

    /// Returns recorded lap times.
    #[must_use]
    pub fn laps(&self) -> &[Duration] {
        &self.laps
    }

    /// Resets the stopwatch to the current instant.
    pub fn reset(&mut self) {
        self.start = Instant::now();
        self.laps.clear();
    }
}

impl Default for Stopwatch {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TimePlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Time utilities plugin.
///
/// Provides time-related resources for systems and plugins.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`Clock`] | Global | Time provider, mockable for testing |
/// | [`Stopwatch`] | Local | Per-context execution timer |
///
/// # Dependencies
///
/// - [`ServerInfoPlugin`]
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::param::{Res, ResMut};
/// use polaris_system::system;
/// use polaris_core_plugins::{ServerInfoPlugin, TimePlugin, Clock, Stopwatch};
///
/// #[system]
/// async fn timed_system(
///     clock: Res<Clock>,
///     mut stopwatch: ResMut<Stopwatch>,
/// ) {
///     let start = clock.now();
///     // ... do work ...
///     stopwatch.lap();
///     println!("Elapsed: {:?}", stopwatch.elapsed());
/// }
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// server.add_plugins(TimePlugin::default());
/// # tokio_test::block_on(async {
/// server.finish().await;
/// # });
/// ```
///
/// # Testing with Mock Clock
///
/// Use [`TimePlugin::with_clock`] to inject a mock clock for deterministic tests:
///
/// ```
/// # #[cfg(feature = "test-utils")]
/// # {
/// use std::sync::Arc;
/// use std::time::{Duration, Instant};
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{ServerInfoPlugin, TimePlugin, Clock, MockClock};
///
/// // Create a mock clock
/// let mock = Arc::new(MockClock::new(Instant::now()));
/// let plugin = TimePlugin::with_clock(mock.clone());
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// server.add_plugins(plugin);
/// # tokio_test::block_on(async {
/// server.finish().await;
/// # });
///
/// // Advance time in tests without waiting
/// mock.advance(Duration::from_secs(60));
///
/// // Clock now reports 60 seconds later
/// let ctx = server.create_context();
/// let clock = ctx.get_resource::<Clock>().unwrap();
/// // clock.now() is now 60 seconds ahead
/// # }
/// ```
#[derive(Clone, Default)]
pub struct TimePlugin {
    /// Custom clock provider (for testing).
    clock: Option<Arc<dyn ClockProvider>>,
}

impl TimePlugin {
    /// Creates a new `TimePlugin` with default system clock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a `TimePlugin` with a custom clock provider.
    ///
    /// Use this for testing with mock/frozen time.
    ///
    /// # Example
    ///
    /// ```
    /// # #[cfg(feature = "test-utils")]
    /// # {
    /// use std::sync::Arc;
    /// use std::time::Instant;
    /// use polaris_core_plugins::{TimePlugin, MockClock};
    ///
    /// let mock = Arc::new(MockClock::new(Instant::now()));
    /// let plugin = TimePlugin::with_clock(mock);
    /// # }
    /// ```
    #[must_use]
    pub fn with_clock(clock: Arc<dyn ClockProvider>) -> Self {
        Self { clock: Some(clock) }
    }
}

impl Plugin for TimePlugin {
    const ID: &'static str = "polaris::time";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        // Global: Clock provider (mockable for testing)
        let clock = match &self.clock {
            Some(provider) => Clock::with_provider(provider.clone()),
            None => Clock::system(),
        };
        server.insert_global(clock);

        // Local: Per-context stopwatch
        server.register_local(Stopwatch::new);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ServerInfoPlugin>()]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MockClock for Testing
// ─────────────────────────────────────────────────────────────────────────────

/// Mock clock for testing with controllable time.
///
/// # Example
///
/// ```
/// # #[cfg(feature = "test-utils")]
/// # {
/// use std::sync::Arc;
/// use std::time::{Duration, Instant};
/// use polaris_core_plugins::{TimePlugin, MockClock};
///
/// let mock = Arc::new(MockClock::new(Instant::now()));
/// let plugin = TimePlugin::with_clock(mock.clone());
///
/// // Later, advance time
/// mock.advance(Duration::from_secs(60));
/// # }
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct MockClock {
    current: std::sync::RwLock<Instant>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockClock {
    /// Creates a mock clock set to the given instant.
    #[must_use]
    pub fn new(start: Instant) -> Self {
        Self {
            current: std::sync::RwLock::new(start),
        }
    }

    /// Advances the clock by the given duration.
    pub fn advance(&self, duration: Duration) {
        let mut current = self.current.write().expect("MockClock lock poisoned");
        *current += duration;
    }

    /// Sets the clock to a specific instant.
    pub fn set(&self, instant: Instant) {
        let mut current = self.current.write().expect("MockClock lock poisoned");
        *current = instant;
    }

    /// Returns the current instant.
    #[must_use]
    pub fn current(&self) -> Instant {
        *self.current.read().expect("MockClock lock poisoned")
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl ClockProvider for MockClock {
    fn now(&self) -> Instant {
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_default_uses_system_time() {
        let clock = Clock::default();
        let before = Instant::now();
        let clock_now = clock.now();
        let after = Instant::now();

        assert!(clock_now >= before);
        assert!(clock_now <= after);
    }

    #[test]
    fn clock_elapsed_since() {
        let clock = Clock::default();
        let earlier = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = clock.elapsed_since(earlier);

        assert!(elapsed >= Duration::from_millis(10));
    }

    #[test]
    fn stopwatch_elapsed() {
        let stopwatch = Stopwatch::new();
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = stopwatch.elapsed();

        assert!(elapsed >= Duration::from_millis(10));
    }

    #[test]
    fn stopwatch_laps() {
        let mut stopwatch = Stopwatch::new();
        std::thread::sleep(Duration::from_millis(5));
        stopwatch.lap();
        std::thread::sleep(Duration::from_millis(5));
        stopwatch.lap();

        let laps = stopwatch.laps();
        assert_eq!(laps.len(), 2);
        assert!(laps[1] > laps[0]);
    }

    #[test]
    fn stopwatch_reset() {
        let mut stopwatch = Stopwatch::new();
        std::thread::sleep(Duration::from_millis(10));
        stopwatch.lap();

        stopwatch.reset();

        assert!(stopwatch.elapsed() < Duration::from_millis(5));
        assert!(stopwatch.laps().is_empty());
    }

    #[test]
    fn mock_clock_advance() {
        let mock = MockClock::new(Instant::now());
        let initial = mock.current();

        mock.advance(Duration::from_secs(60));

        let after = mock.current();
        assert_eq!(after.duration_since(initial), Duration::from_secs(60));
    }

    #[test]
    fn mock_clock_set() {
        let mock = MockClock::new(Instant::now());
        let target = Instant::now() + Duration::from_secs(100);

        mock.set(target);

        assert_eq!(mock.current(), target);
    }

    #[tokio::test]
    async fn time_plugin_with_mock_clock() {
        let mock = Arc::new(MockClock::new(Instant::now()));
        let plugin = TimePlugin::with_clock(mock.clone());

        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);
        server.add_plugins(plugin);
        server.finish().await;

        let ctx = server.create_context();
        assert!(ctx.contains_resource::<Clock>());
        assert!(ctx.contains_resource::<Stopwatch>());

        // Advance mock time
        mock.advance(Duration::from_secs(60));
    }
}

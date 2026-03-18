//! Fmt console output layer construction.

use super::TracingFormat;
use super::TracingLayersApi;
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::Layer as _;

/// Fmt console output configuration.
///
/// Controls the appearance and filtering of human-readable log output.
/// Pass to [`TracingPlugin::with_fmt`](super::TracingPlugin::with_fmt) to
/// enable console logging with custom settings.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::{TracingPlugin, FmtConfig, TracingFormat};
///
/// TracingPlugin::default().with_fmt(
///     FmtConfig::default()
///         .format(TracingFormat::Json)
///         .env_filter("polaris=debug,hyper=warn")
///         .span_events(true),
/// );
/// ```
#[derive(Debug, Clone)]
pub struct FmtConfig {
    /// Output format.
    pub(super) format: TracingFormat,
    /// Environment filter string (e.g., `"polaris=debug,hyper=warn"`).
    pub(super) env_filter: Option<String>,
    /// Whether to include span enter/exit events.
    pub(super) span_events: bool,
}

impl Default for FmtConfig {
    fn default() -> Self {
        Self {
            format: TracingFormat::Pretty,
            env_filter: None,
            span_events: false,
        }
    }
}

impl FmtConfig {
    /// Sets the output format.
    #[must_use]
    pub fn format(mut self, format: TracingFormat) -> Self {
        self.format = format;
        self
    }

    /// Sets a custom environment filter string.
    ///
    /// Format: `target=level,target=level,...`
    #[must_use]
    pub fn env_filter(mut self, filter: impl Into<String>) -> Self {
        self.env_filter = Some(filter.into());
        self
    }

    /// Enables span enter/exit events in output.
    #[must_use]
    pub fn span_events(mut self, enabled: bool) -> Self {
        self.span_events = enabled;
        self
    }
}

/// Pushes a fmt layer into the shared [`TracingLayersApi`].
pub(super) fn push_layer(api: &mut TracingLayersApi, config: &FmtConfig, level: Level) {
    let env_filter = match &config.env_filter {
        Some(filter) => {
            EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new(level.as_str()))
        }
        None => EnvFilter::new(level.as_str()),
    };

    let span_events = if config.span_events {
        FmtSpan::ENTER | FmtSpan::EXIT
    } else {
        FmtSpan::NONE
    };

    match config.format {
        TracingFormat::Pretty => {
            api.push(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_span_events(span_events)
                    .with_filter(env_filter),
            );
        }
        TracingFormat::Compact => {
            api.push(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_span_events(span_events)
                    .with_filter(env_filter),
            );
        }
        TracingFormat::Json => {
            api.push(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_span_events(span_events)
                    .with_filter(env_filter),
            );
        }
    }
}

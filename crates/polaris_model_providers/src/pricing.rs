//! Shared per-million-token model pricing, with optional live prices fetched
//! from the web.
//!
//! Every provider in this crate needs the same thing: a way to answer
//! [`LlmProvider::pricing`](polaris_models::llm::LlmProvider::pricing)
//! without hardcoding `if model.starts_with(...)` match arms that go stale
//! the moment a vendor changes a rate card. This module is that answer, built
//! once here and reused by every provider's `pricing()` — a future provider
//! gets pricing for free by holding a [`PriceStore`] too. Network code lives
//! only here, not in `polaris_models` (the trait crate), which stays
//! transport-agnostic.
//!
//! # Design
//!
//! - [`PricingTable`] is a plain, in-memory lookup: model-id prefix ->
//!   [`ModelPricing`], resolved by longest-prefix match.
//! - [`PriceStore`] answers lookups from, in order: its own per-model
//!   overrides ([`PriceStore::with_override`]), the fetched or cached table,
//!   and the built-in [`fallback_table`].
//! - [`PriceStore::builtin`] is the default for every provider in this crate.
//!   It uses only the built-in table: it never touches the network and never
//!   reads or writes a file.
//! - Live prices are opt-in. [`PriceStore::live`] (or [`PriceStore::new`] /
//!   [`PriceStore::with_fetcher`] for your own source) fetches a price table
//!   and keeps it fresh. See [Side effects of a live store](#side-effects-of-a-live-store).
//!
//! # Side effects of a live store
//!
//! A store built with [`PriceStore::live`], [`PriceStore::new`] or
//! [`PriceStore::with_fetcher`] does the following, and nothing else:
//!
//! - **Disk read at construction:** when it has a cache path, it reads that
//!   file (at most [`MAX_PRICE_DATA_BYTES`]) to seed its prices.
//! - **Network fetch from a background thread:** [`PriceStore::get`] is
//!   synchronous and never waits on the network. When the prices are missing
//!   or older than the TTL, it starts at most one background thread at a
//!   time to fetch the source ([`DEFAULT_SOURCE_URL`] for
//!   [`PriceStore::live`]), and answers the current call with the prices
//!   already in memory.
//! - **Disk write after a successful fetch:** when it has a cache path, it
//!   writes the parsed prices there ([`default_cache_path`] for
//!   [`PriceStore::live`]). It writes a temp file beside the cache file and
//!   renames it into place, so a reader never sees a half-written file.
//! - **Wait after a failed fetch:** a fetch that fails (network error,
//!   non-2xx status, a body over [`MAX_PRICE_DATA_BYTES`], unparsable JSON,
//!   no usable rows, or a panic in a custom [`PriceFetcher`]) keeps the
//!   previous prices, and the store waits [`FAILED_REFRESH_RETRY_DELAY`]
//!   before it tries again.
//!
//! Fetched rows are checked before use: a row whose rate is not finite, is
//! negative, is above 10,000 USD per million tokens, or whose input and
//! output rates are both zero is dropped, so a bad upstream file cannot make
//! reported costs zero, negative, or absurd.
//!
//! Tests must never hit the network: build a [`PriceStore`] with
//! [`PriceStore::with_fetcher`] and an injected [`PriceFetcher`] fixture, or
//! use [`PriceStore::builtin`].

use polaris_models::llm::ModelPricing;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

/// `LiteLLM`'s machine-readable price table: first-party `claude-*` /
/// `gpt-*` rows plus per-region Bedrock rows (e.g.
/// `us.anthropic.claude-opus-5-5`). [`PriceStore::live`] fetches it.
pub const DEFAULT_SOURCE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Default cache freshness window: re-fetch once a day.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a live [`PriceStore`] waits after a failed fetch before it tries
/// again. Without this wait, a host with no network access would start a new
/// fetch on almost every lookup.
pub const FAILED_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);

/// The largest price data, in bytes, a [`PriceStore`] reads: a fetched body
/// or a cache file. Larger data is rejected before it is parsed. The
/// `LiteLLM` table is a few megabytes, so this leaves ample room.
pub const MAX_PRICE_DATA_BYTES: usize = 32 * 1024 * 1024;

/// The highest rate, in USD per million tokens, a fetched or cached row may
/// carry. A row above it is treated as corrupt and dropped.
const MAX_PLAUSIBLE_RATE_PER_MILLION_USD: f64 = 10_000.0;

/// The cache file name written under the user cache dir (see
/// [`default_cache_path`]).
const CACHE_FILE_NAME: &str = "model-prices.json";

/// How long the HTTP fetcher waits for the price source before giving up.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

// ─────────────────────────────────────────────────────────────────────────────
// PricingTable
// ─────────────────────────────────────────────────────────────────────────────

/// Per-million-token USD pricing, looked up by model-id prefix.
///
/// [`get`](Self::get) resolves the *longest* matching prefix, so a specific
/// row (e.g. `claude-opus-5-5`) wins over a broader family fallback (e.g.
/// `claude-opus-5`) without either having to know about the other.
///
/// # Examples
///
/// ```
/// use polaris_model_providers::pricing::PricingTable;
/// use polaris_models::llm::ModelPricing;
///
/// let table = PricingTable::new()
///     .with_entry("claude-opus-5", ModelPricing::new(5.0, 25.0))
///     .with_entry("claude-opus-5-5", ModelPricing::new(4.0, 20.0));
///
/// // The longest matching prefix wins.
/// assert_eq!(
///     table.get("claude-opus-5-5-20260315"),
///     Some(ModelPricing::new(4.0, 20.0))
/// );
/// assert_eq!(table.get("gpt-5.5"), None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: HashMap<String, ModelPricing>,
}

impl PricingTable {
    /// An empty table (every lookup returns `None`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts one row (builder form).
    #[must_use]
    pub fn with_entry(mut self, prefix: impl Into<String>, pricing: ModelPricing) -> Self {
        self.insert(prefix, pricing);
        self
    }

    /// Inserts (or replaces) one row.
    pub fn insert(&mut self, prefix: impl Into<String>, pricing: ModelPricing) -> &mut Self {
        self.entries.insert(prefix.into(), pricing);
        self
    }

    /// Overlays `other`'s rows on top of `self` (a shared prefix in `other`
    /// wins), returning the merged table.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.entries.extend(other.entries);
        self
    }

    /// Number of rows in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Longest-prefix (or exact) match against `model`; `None` when no row's
    /// prefix matches — never a same-length-family guess.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<ModelPricing> {
        self.entries
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, pricing)| *pricing)
    }
}

/// The small built-in fallback table, used when a live price is unavailable and
/// no cache file (even expired) is available. Anthropic first-party list
/// prices are USD per million tokens, as of 2026-09-25; the `OpenAI` rows
/// mirror what `OpenAiProvider::pricing` hardcoded before this module
/// existed. Cache rates left unset default to the standard 0.1x read / 1.25x
/// write of the input rate (see [`ModelPricing::new`]).
///
/// Returns a copy; [`PriceStore`] reads one shared instance built once per
/// process.
#[must_use]
pub fn fallback_table() -> PricingTable {
    builtin_table().clone()
}

/// The built-in table, built on first use and shared for the life of the
/// process, so a lookup that misses the fetched table does not rebuild it.
fn builtin_table() -> &'static PricingTable {
    static TABLE: OnceLock<PricingTable> = OnceLock::new();
    TABLE.get_or_init(build_builtin_table)
}

fn build_builtin_table() -> PricingTable {
    let mut table = PricingTable::new();
    table
        // Anthropic — current generation.
        .insert(
            "claude-fable-5-1",
            ModelPricing::new(10.0, 50.0).with_cache_rates(0.25, 12.5),
        )
        .insert("claude-fable-5", ModelPricing::new(10.0, 50.0))
        .insert(
            "claude-opus-5-5",
            ModelPricing::new(4.0, 20.0).with_cache_rates(0.20, 5.0),
        )
        .insert("claude-opus-5", ModelPricing::new(5.0, 25.0))
        .insert("claude-opus-4-8", ModelPricing::new(5.0, 25.0))
        .insert("claude-opus-4-7", ModelPricing::new(5.0, 25.0))
        .insert("claude-opus-4-6", ModelPricing::new(5.0, 25.0))
        // Kept from the pre-table match arms.
        .insert("claude-opus-4-5", ModelPricing::new(5.0, 25.0))
        .insert("claude-opus-4", ModelPricing::new(15.0, 75.0))
        .insert(
            "claude-sonnet-5-5",
            ModelPricing::new(2.0, 10.0).with_cache_rates(0.20, 2.5),
        )
        .insert("claude-sonnet-5", ModelPricing::new(2.0, 10.0))
        .insert("claude-sonnet-4-6", ModelPricing::new(3.0, 15.0))
        .insert("claude-sonnet-4", ModelPricing::new(3.0, 15.0))
        .insert("claude-haiku-4-5", ModelPricing::new(1.0, 5.0))
        .insert("claude-haiku-4", ModelPricing::new(1.0, 5.0))
        .insert("claude-haiku-3-5", ModelPricing::new(0.8, 4.0))
        // OpenAI — kept from `OpenAiProvider::pricing`'s old match arms.
        .insert(
            "gpt-6-luna",
            ModelPricing::new(0.10, 0.50).with_cache_rates(0.01, 0.125),
        )
        .insert(
            "global.openai.gpt-6-luna",
            ModelPricing::new(0.10, 0.50).with_cache_rates(0.01, 0.125),
        )
        .insert(
            "us.openai.gpt-6-luna",
            ModelPricing::new(0.11, 0.55).with_cache_rates(0.011, 0.1375),
        )
        .insert("gpt-5.5", ModelPricing::new(5.0, 30.0))
        .insert("gpt-5.4-mini", ModelPricing::new(0.75, 4.5))
        .insert("gpt-5.4", ModelPricing::new(2.5, 15.0));
    table
}

// ─────────────────────────────────────────────────────────────────────────────
// Fetching
// ─────────────────────────────────────────────────────────────────────────────

/// Why a [`PriceFetcher`] could not return a price-table body.
///
/// Every variant is handled the same way by [`PriceStore`]: the previous
/// prices stay in place and the store waits [`FAILED_REFRESH_RETRY_DELAY`]
/// before it fetches again. The variants exist so logs say what went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PriceFetchError {
    /// The request could not be sent, or the body could not be read.
    #[error("price request failed: {0}")]
    Request(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The source answered with a status other than 2xx.
    #[error("price source answered with HTTP status {status}")]
    Status {
        /// The HTTP status code the source returned.
        status: u16,
    },
    /// The body is larger than [`MAX_PRICE_DATA_BYTES`].
    #[error("price data is larger than the {limit}-byte limit")]
    TooLarge {
        /// The limit, in bytes, that the body went over.
        limit: usize,
    },
}

impl PriceFetchError {
    /// Wraps any error as a [`PriceFetchError::Request`]. Use it from a
    /// custom [`PriceFetcher`] for failures that have no variant of their
    /// own.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::pricing::PriceFetchError;
    ///
    /// let err = PriceFetchError::request("connection refused");
    /// assert_eq!(err.to_string(), "price request failed: connection refused");
    /// ```
    #[must_use]
    pub fn request(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Request(source.into())
    }
}

/// Fetches the raw price-table JSON body.
///
/// Implementations run on a dedicated background thread (never on the
/// caller's task), so they are free to block. [`PriceStore::new`] and
/// [`PriceStore::live`] use an internal `reqwest::blocking`-backed fetcher;
/// implement this trait to read prices from somewhere else, or to keep tests
/// off the network (see the module docs). The body must be in `LiteLLM`'s
/// format (see [`DEFAULT_SOURCE_URL`]).
///
/// # Examples
///
/// ```
/// use polaris_model_providers::pricing::{PriceFetchError, PriceFetcher, PriceStore, DEFAULT_TTL};
/// use std::sync::Arc;
///
/// /// Serves a price table from a string, for example one bundled with the app.
/// struct Bundled(&'static str);
///
/// impl PriceFetcher for Bundled {
///     fn fetch(&self) -> Result<String, PriceFetchError> {
///         Ok(self.0.to_string())
///     }
/// }
///
/// let body = r#"{"claude-opus-5-5": {
///     "litellm_provider": "anthropic",
///     "input_cost_per_token": 0.000004,
///     "output_cost_per_token": 0.00002
/// }}"#;
/// // No cache path: this store never touches the disk.
/// let store = PriceStore::with_fetcher(Arc::new(Bundled(body)), None, DEFAULT_TTL);
/// # let _ = store;
/// ```
pub trait PriceFetcher: Send + Sync + 'static {
    /// Returns the raw JSON body, or why the fetch failed.
    ///
    /// # Errors
    ///
    /// Returns a [`PriceFetchError`] when the body cannot be obtained
    /// (network error, non-2xx status, a body over the size limit, etc).
    fn fetch(&self) -> Result<String, PriceFetchError>;
}

/// Fetches `url` with a blocking HTTP client. Meant to be constructed once
/// and invoked from the background refresh thread — never from the calling
/// task, since [`reqwest::blocking`] cannot run inside an async runtime.
struct HttpFetcher {
    url: String,
}

impl PriceFetcher for HttpFetcher {
    fn fetch(&self) -> Result<String, PriceFetchError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(PriceFetchError::request)?;
        let response = client
            .get(&self.url)
            .send()
            .map_err(PriceFetchError::request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(PriceFetchError::Status {
                status: status.as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PRICE_DATA_BYTES as u64)
        {
            return Err(PriceFetchError::TooLarge {
                limit: MAX_PRICE_DATA_BYTES,
            });
        }
        read_capped(response)
            .map_err(PriceFetchError::request)?
            .ok_or(PriceFetchError::TooLarge {
                limit: MAX_PRICE_DATA_BYTES,
            })
    }
}

/// Reads `reader` as UTF-8 text, stopping one byte past
/// [`MAX_PRICE_DATA_BYTES`]. Returns `Ok(None)` when the data is over the
/// limit, so an oversized source is never held in memory whole, and an
/// `InvalidData` error when it is not UTF-8.
fn read_capped(reader: impl Read) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_PRICE_DATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PRICE_DATA_BYTES {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

/// Parses a `LiteLLM`-shaped price JSON body into a [`PricingTable`].
///
/// Keeps only rows whose `litellm_provider` matches one of the vendors this
/// crate's providers serve (`anthropic`, `bedrock`, `bedrock_converse`,
/// `openai`) and skips — rather than erroring on — any row that doesn't carry
/// numeric `input_cost_per_token`/`output_cost_per_token` fields or whose
/// rates fail [`is_plausible`], so an upstream schema change or a bad row
/// degrades gracefully instead of losing every price.
fn parse_litellm_json(body: &str) -> Result<PricingTable, String> {
    const RELEVANT_PROVIDERS: &[&str] = &["anthropic", "bedrock", "bedrock_converse", "openai"];

    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid JSON: {err}"))?;
    let Some(object) = root.as_object() else {
        return Err("expected a top-level JSON object".to_string());
    };

    let mut table = PricingTable::new();
    let mut implausible = 0_usize;
    for (model_id, row) in object {
        let is_relevant = row
            .get("litellm_provider")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|provider| RELEVANT_PROVIDERS.contains(&provider));
        if !is_relevant {
            continue;
        }
        match parse_row(row) {
            Some(pricing) if is_plausible(&pricing) => {
                table.insert(model_id.clone(), pricing);
            }
            Some(_) => implausible += 1,
            None => {}
        }
    }
    if implausible > 0 {
        tracing::warn!(
            rows = implausible,
            "dropped fetched model prices with rates that are not finite, negative, all zero, or too high"
        );
    }
    Ok(table)
}

/// `LiteLLM` reports USD per single token; `ModelPricing` wants USD per million.
const PER_TOKEN_TO_PER_MILLION: f64 = 1_000_000.0;

/// Parses one `LiteLLM` row into a [`ModelPricing`], or `None` when the row is
/// missing the required input/output cost fields (tolerated, not an error).
/// The rates are not checked here; see [`is_plausible`].
fn parse_row(row: &serde_json::Value) -> Option<ModelPricing> {
    let input = row.get("input_cost_per_token")?.as_f64()?;
    let output = row.get("output_cost_per_token")?.as_f64()?;
    let mut pricing = ModelPricing::new(
        input * PER_TOKEN_TO_PER_MILLION,
        output * PER_TOKEN_TO_PER_MILLION,
    );
    let cache_read = row
        .get("cache_read_input_token_cost")
        .and_then(serde_json::Value::as_f64)
        .map_or(pricing.cache_read_per_million_usd, |v| {
            v * PER_TOKEN_TO_PER_MILLION
        });
    let cache_write = row
        .get("cache_creation_input_token_cost")
        .and_then(serde_json::Value::as_f64)
        .map_or(pricing.cache_write_per_million_usd, |v| {
            v * PER_TOKEN_TO_PER_MILLION
        });
    pricing = pricing.with_cache_rates(cache_read, cache_write);
    Some(pricing)
}

/// `true` when every rate in `pricing` is finite, not negative, and at most
/// [`MAX_PLAUSIBLE_RATE_PER_MILLION_USD`], and the input and output rates are
/// not both zero. Data from the network or the disk must pass this before a
/// [`PriceStore`] serves it, so a bad source cannot make costs zero,
/// negative, or absurd.
fn is_plausible(pricing: &ModelPricing) -> bool {
    let rates = [
        pricing.input_per_million_usd,
        pricing.output_per_million_usd,
        pricing.cache_read_per_million_usd,
        pricing.cache_write_per_million_usd,
    ];
    let in_range = rates
        .iter()
        .all(|rate| rate.is_finite() && (0.0..=MAX_PLAUSIBLE_RATE_PER_MILLION_USD).contains(rate));
    in_range && (pricing.input_per_million_usd > 0.0 || pricing.output_per_million_usd > 0.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Disk cache
// ─────────────────────────────────────────────────────────────────────────────

/// On-disk cache file shape: the already-parsed rows (not the raw upstream
/// JSON), so a cache read never re-runs the parser, plus the time they were
/// fetched so freshness survives a process restart.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    fetched_at_unix_secs: u64,
    entries: HashMap<String, StoredPricing>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct StoredPricing {
    input_per_million_usd: f64,
    output_per_million_usd: f64,
    cache_read_per_million_usd: f64,
    cache_write_per_million_usd: f64,
}

impl From<ModelPricing> for StoredPricing {
    fn from(pricing: ModelPricing) -> Self {
        Self {
            input_per_million_usd: pricing.input_per_million_usd,
            output_per_million_usd: pricing.output_per_million_usd,
            cache_read_per_million_usd: pricing.cache_read_per_million_usd,
            cache_write_per_million_usd: pricing.cache_write_per_million_usd,
        }
    }
}

impl From<StoredPricing> for ModelPricing {
    fn from(stored: StoredPricing) -> Self {
        ModelPricing::new(stored.input_per_million_usd, stored.output_per_million_usd)
            .with_cache_rates(
                stored.cache_read_per_million_usd,
                stored.cache_write_per_million_usd,
            )
    }
}

impl CacheFile {
    fn from_table(table: &PricingTable, fetched_at: SystemTime) -> Self {
        Self {
            fetched_at_unix_secs: fetched_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            entries: table
                .entries
                .iter()
                .map(|(prefix, pricing)| (prefix.clone(), StoredPricing::from(*pricing)))
                .collect(),
        }
    }

    /// Converts the file into a snapshot, dropping any row that fails
    /// [`is_plausible`] (the file may have been edited or damaged on disk).
    fn into_snapshot(self) -> Snapshot {
        let mut table = PricingTable::new();
        for (prefix, stored) in self.entries {
            let pricing = ModelPricing::from(stored);
            if is_plausible(&pricing) {
                table.insert(prefix, pricing);
            }
        }
        Snapshot {
            table,
            fetched_at: Some(UNIX_EPOCH + Duration::from_secs(self.fetched_at_unix_secs)),
        }
    }
}

/// The platform user-cache-dir path for the price cache file
/// (`<cache_dir>/polaris/model-prices.json`), when the platform's cache
/// directory is resolvable. [`PriceStore::live`] caches there.
#[must_use]
pub fn default_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("polaris").join(CACHE_FILE_NAME))
}

/// Reads and parses the cache file at `path`, or `None` when it is missing,
/// larger than [`MAX_PRICE_DATA_BYTES`], or not a valid cache file.
fn read_cache_file(path: &Path) -> Option<CacheFile> {
    let file = std::fs::File::open(path).ok()?;
    let Some(body) = read_capped(file).ok()? else {
        tracing::warn!(
            path = %path.display(),
            limit = MAX_PRICE_DATA_BYTES,
            "model-price cache file is over the size limit; ignoring it"
        );
        return None;
    };
    serde_json::from_str(&body).ok()
}

/// Writes the cache file at `path`, creating missing parent directories.
///
/// The body goes to a temp file beside `path` first and is then renamed over
/// it, so a reader (another process, or this one after a crash) sees the old
/// file or the new one, never a half-written one.
fn write_cache_file(
    path: &Path,
    table: &PricingTable,
    fetched_at: SystemTime,
) -> std::io::Result<()> {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&CacheFile::from_table(table, fetched_at))
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let temp_path = path.with_file_name(temp_name);
    std::fs::write(&temp_path, body)
        .and_then(|()| std::fs::rename(&temp_path, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp_path);
        })
}

// ─────────────────────────────────────────────────────────────────────────────
// PriceStore
// ─────────────────────────────────────────────────────────────────────────────

struct Snapshot {
    table: PricingTable,
    /// `None` means "never fetched / no cache" — always treated as expired.
    fetched_at: Option<SystemTime>,
}

impl Snapshot {
    fn is_expired(&self, ttl: Duration) -> bool {
        match self.fetched_at {
            None => true,
            Some(fetched_at) => SystemTime::now()
                .duration_since(fetched_at)
                .map_or(true, |age| age >= ttl),
        }
    }
}

/// The part of a [`PriceStore`] that every clone shares: the fetched prices
/// and the refresh state.
struct Inner {
    /// `None` for [`PriceStore::builtin`]: such a store never refreshes.
    fetcher: Option<Arc<dyn PriceFetcher>>,
    cache_path: Option<PathBuf>,
    ttl: Duration,
    retry_delay: Duration,
    snapshot: RwLock<Snapshot>,
    /// Guards against piling up concurrent background refreshes when several
    /// `get()` calls race past an expired snapshot at once.
    refreshing: AtomicBool,
    /// When the last refresh failed; cleared by a successful one. A refresh
    /// is not started again until `retry_delay` has passed since then.
    last_failure: Mutex<Option<Instant>>,
}

impl Inner {
    fn waiting_after_failure(&self) -> bool {
        self.last_failure
            .lock()
            .is_some_and(|failed_at| failed_at.elapsed() < self.retry_delay)
    }

    fn record_failure(&self) {
        *self.last_failure.lock() = Some(Instant::now());
    }
}

/// Clears [`Inner::refreshing`] when the refresh thread ends, including when
/// it unwinds from a panic in a custom [`PriceFetcher`]. Without it, a panic
/// would leave the flag set and no refresh would ever start again. A panic
/// also counts as a failed refresh, so a fetcher that always panics is
/// retried after the retry delay, not on every lookup.
struct RefreshGuard<'a>(&'a Inner);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::warn!("model-price refresh thread panicked; keeping previous prices");
            self.0.record_failure();
        }
        self.0.refreshing.store(false, Ordering::Release);
    }
}

/// Runtime price store: answers [`ModelPricing`] lookups synchronously.
///
/// A lookup tries, in order: this store's own overrides
/// ([`with_override`](Self::with_override)), the fetched or cached table (live
/// stores only), and the built-in [`fallback_table`]. Each step uses
/// longest-prefix matching.
///
/// Choose how a store gets its prices when you build it:
///
/// | Constructor | Network | Disk |
/// |-------------|---------|------|
/// | [`builtin`](Self::builtin) (the provider default) | never | never |
/// | [`live`](Self::live) | fetches [`DEFAULT_SOURCE_URL`] | reads and writes [`default_cache_path`] |
/// | [`new`](Self::new) | fetches the given URL | reads and writes the given path, if any |
/// | [`with_fetcher`](Self::with_fetcher) | whatever the [`PriceFetcher`] does | reads and writes the given path, if any |
///
/// See the [module docs](crate::pricing#side-effects-of-a-live-store) for exactly when
/// a live store fetches and writes.
///
/// Cheap to [`Clone`]. Clones share the fetched prices and the refresh state,
/// so many providers can hold one live store and cost one fetch. Overrides
/// belong to the handle they were added to: adding one does not change any
/// other clone.
///
/// # Examples
///
/// ```
/// use polaris_model_providers::pricing::PriceStore;
/// use polaris_models::llm::ModelPricing;
///
/// let store = PriceStore::builtin().with_override("my-fine-tune", ModelPricing::new(1.0, 2.0));
/// assert_eq!(store.get("my-fine-tune-v3"), Some(ModelPricing::new(1.0, 2.0)));
/// assert_eq!(store.get("claude-sonnet-4-6"), Some(ModelPricing::new(3.0, 15.0)));
/// ```
#[derive(Clone)]
pub struct PriceStore {
    inner: Arc<Inner>,
    overrides: Arc<PricingTable>,
}

impl std::fmt::Debug for PriceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriceStore")
            .field("live", &self.inner.fetcher.is_some())
            .field("cache_path", &self.inner.cache_path)
            .field("ttl", &self.inner.ttl)
            .field("overrides", &self.overrides.len())
            .finish_non_exhaustive()
    }
}

impl Default for PriceStore {
    /// Same as [`PriceStore::builtin`].
    fn default() -> Self {
        Self::builtin()
    }
}

impl PriceStore {
    /// A store that serves only the built-in [`fallback_table`] (plus any
    /// overrides). It never fetches and never reads or writes a file.
    ///
    /// This is the default price store of every provider in this crate.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::pricing::PriceStore;
    /// use polaris_models::llm::ModelPricing;
    ///
    /// let store = PriceStore::builtin();
    /// assert_eq!(store.get("claude-sonnet-4-6"), Some(ModelPricing::new(3.0, 15.0)));
    /// assert_eq!(store.get("unknown-model"), None);
    /// ```
    #[must_use]
    pub fn builtin() -> Self {
        Self::from_parts(None, None, DEFAULT_TTL)
    }

    /// Builds a live store that fetches `source` (a URL in `LiteLLM`'s
    /// format) with a blocking HTTP client, caching to `cache_path` (when
    /// `Some`) with freshness window `ttl`.
    ///
    /// Reads `cache_path` now; fetches and writes only on a later
    /// [`get`](Self::get). See the
    /// [module docs](crate::pricing#side-effects-of-a-live-store).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use polaris_model_providers::pricing::{PriceStore, DEFAULT_SOURCE_URL};
    /// use std::time::Duration;
    ///
    /// // Refresh every hour and keep no cache file.
    /// let store = PriceStore::new(DEFAULT_SOURCE_URL, None, Duration::from_secs(60 * 60));
    /// # let _ = store;
    /// ```
    #[must_use]
    pub fn new(source: impl Into<String>, cache_path: Option<PathBuf>, ttl: Duration) -> Self {
        Self::with_fetcher(
            Arc::new(HttpFetcher { url: source.into() }),
            cache_path,
            ttl,
        )
    }

    /// Builds a live store with an injected [`PriceFetcher`] — the way to
    /// read prices from your own source, and the seam tests use to avoid the
    /// network.
    ///
    /// Reads `cache_path` now; calls the fetcher and writes `cache_path`
    /// only on a later [`get`](Self::get). See the
    /// [module docs](crate::pricing#side-effects-of-a-live-store).
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::pricing::{PriceFetchError, PriceFetcher, PriceStore, DEFAULT_TTL};
    /// use std::sync::Arc;
    ///
    /// struct Offline;
    /// impl PriceFetcher for Offline {
    ///     fn fetch(&self) -> Result<String, PriceFetchError> {
    ///         Err(PriceFetchError::request("offline"))
    ///     }
    /// }
    ///
    /// let store = PriceStore::with_fetcher(Arc::new(Offline), None, DEFAULT_TTL);
    /// // Until a fetch succeeds, the built-in table answers.
    /// assert!(store.get("claude-sonnet-4-6").is_some());
    /// ```
    #[must_use]
    pub fn with_fetcher(
        fetcher: Arc<dyn PriceFetcher>,
        cache_path: Option<PathBuf>,
        ttl: Duration,
    ) -> Self {
        Self::from_parts(Some(fetcher), cache_path, ttl)
    }

    fn from_parts(
        fetcher: Option<Arc<dyn PriceFetcher>>,
        cache_path: Option<PathBuf>,
        ttl: Duration,
    ) -> Self {
        let snapshot = load_initial_snapshot(cache_path.as_deref());
        Self {
            inner: Arc::new(Inner {
                fetcher,
                cache_path,
                ttl,
                retry_delay: FAILED_REFRESH_RETRY_DELAY,
                snapshot: RwLock::new(snapshot),
                refreshing: AtomicBool::new(false),
                last_failure: Mutex::new(None),
            }),
            overrides: Arc::new(PricingTable::new()),
        }
    }

    /// The process-wide live store: [`DEFAULT_SOURCE_URL`],
    /// [`default_cache_path`], [`DEFAULT_TTL`]. Every call returns a clone of
    /// the same store, so all providers given it cost one fetch and one cache
    /// file, not several.
    ///
    /// Providers do not use it unless you pass it in. Passing it in opts the
    /// process into fetching from `raw.githubusercontent.com` and writing the
    /// cache file; see the [module docs](crate::pricing#side-effects-of-a-live-store).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use polaris_model_providers::pricing::PriceStore;
    ///
    /// // Hand this to a provider's or plugin's `with_pricing_store`.
    /// let store = PriceStore::live();
    /// # let _ = store;
    /// ```
    #[must_use]
    pub fn live() -> Self {
        static LIVE: OnceLock<PriceStore> = OnceLock::new();
        LIVE.get_or_init(|| PriceStore::new(DEFAULT_SOURCE_URL, default_cache_path(), DEFAULT_TTL))
            .clone()
    }

    /// Returns this store with a per-model override that beats whatever the
    /// fetched or built-in table says for models starting with `prefix`.
    ///
    /// The override applies to the returned handle and to clones made from
    /// it later. It does not change other clones, including the one
    /// [`live`](Self::live) hands out, so an override given to one provider
    /// stays with that provider. The fetched prices are still shared.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::pricing::PriceStore;
    /// use polaris_models::llm::ModelPricing;
    ///
    /// let base = PriceStore::builtin();
    /// let discounted = base
    ///     .clone()
    ///     .with_override("claude-sonnet-4-6", ModelPricing::new(1.0, 5.0));
    ///
    /// assert_eq!(discounted.get("claude-sonnet-4-6"), Some(ModelPricing::new(1.0, 5.0)));
    /// assert_eq!(base.get("claude-sonnet-4-6"), Some(ModelPricing::new(3.0, 15.0)));
    /// ```
    #[must_use]
    pub fn with_override(mut self, prefix: impl Into<String>, pricing: ModelPricing) -> Self {
        Arc::make_mut(&mut self.overrides).insert(prefix, pricing);
        self
    }

    /// Looks up `model`'s pricing: overrides first, then the fetched or
    /// cached table, then the built-in fallback, each by longest-prefix
    /// match. `None` when no step knows the model.
    ///
    /// Never blocks on the network. On a live store whose prices are missing
    /// or past their TTL, this starts a background refresh (at most one at a
    /// time, and not within [`FAILED_REFRESH_RETRY_DELAY`] of a failed one)
    /// and answers with the prices already in memory.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::pricing::PriceStore;
    ///
    /// let store = PriceStore::builtin();
    /// let pricing = store.get("claude-opus-4-7").expect("the built-in table has Claude Opus 4.7");
    /// // Ten thousand input tokens and one thousand output tokens, in USD.
    /// let cost = pricing.cost(10_000, 1_000);
    /// assert!(cost > 0.0);
    /// ```
    #[must_use]
    pub fn get(&self, model: &str) -> Option<ModelPricing> {
        if let Some(pricing) = self.overrides.get(model) {
            return Some(pricing);
        }

        // A started refresh runs on its own; this call does not wait for it.
        let _refresh = self.refresh_if_due();
        self.inner
            .snapshot
            .read()
            .table
            .get(model)
            .or_else(|| builtin_table().get(model))
    }

    /// Starts a background refresh when one is due, and returns its thread
    /// handle. `None` when no refresh started: the store is not live, its
    /// prices are fresh, it is waiting after a failed refresh, or a refresh
    /// is already running.
    fn refresh_if_due(&self) -> Option<JoinHandle<()>> {
        let inner = &self.inner;
        if inner.fetcher.is_none()
            || !inner.snapshot.read().is_expired(inner.ttl)
            || inner.waiting_after_failure()
        {
            return None;
        }
        if inner
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None; // a refresh is already in flight
        }
        // A refresh may have failed between the check above and the
        // compare-exchange; honor its retry delay too.
        if inner.waiting_after_failure() {
            inner.refreshing.store(false, Ordering::Release);
            return None;
        }

        let thread_inner = Arc::clone(inner);
        let spawned = std::thread::Builder::new()
            .name("polaris-price-refresh".to_string())
            .spawn(move || {
                let _guard = RefreshGuard(&thread_inner);
                refresh_now(&thread_inner);
            });
        match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!(error = %err, "failed to start the model-price refresh thread");
                inner.record_failure();
                inner.refreshing.store(false, Ordering::Release);
                None
            }
        }
    }

    /// Performs one fetch-and-swap synchronously on the calling thread,
    /// bypassing the background-thread trigger. Test-only: lets pricing
    /// tests assert on freshly fetched data without a timing-sensitive wait.
    #[cfg(test)]
    fn refresh_blocking(&self) {
        refresh_now(&self.inner);
    }

    /// Sets how long this store waits after a failed refresh. Test-only, and
    /// only on a store no other handle shares yet.
    #[cfg(test)]
    fn with_retry_delay(mut self, delay: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_retry_delay must be called on a freshly built store")
            .retry_delay = delay;
        self
    }
}

fn load_initial_snapshot(cache_path: Option<&Path>) -> Snapshot {
    if let Some(cache_file) = cache_path.and_then(read_cache_file) {
        return cache_file.into_snapshot();
    }
    // The built-in table is consulted on every miss, so an empty table here
    // already falls back to it.
    Snapshot {
        table: PricingTable::new(),
        fetched_at: None,
    }
}

/// Fetches, checks, and parses one price table, logging why when it cannot.
fn fetch_table(fetcher: &dyn PriceFetcher) -> Option<PricingTable> {
    let body = match fetcher.fetch() {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(error = %err, "failed to fetch model prices; keeping previous prices");
            return None;
        }
    };
    if body.len() > MAX_PRICE_DATA_BYTES {
        tracing::warn!(
            limit = MAX_PRICE_DATA_BYTES,
            "fetched model price data is over the size limit; keeping previous prices"
        );
        return None;
    }
    match parse_litellm_json(&body) {
        Ok(table) if !table.is_empty() => Some(table),
        Ok(_) => {
            tracing::warn!("model price fetch returned no usable rows; keeping previous prices");
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to parse model price data; keeping previous prices");
            None
        }
    }
}

/// Fetches, parses, and swaps in a fresh snapshot. On any failure (fetch,
/// size, parse, or an empty result), leaves the current snapshot — fetched,
/// cached, or empty — in place, so a price lookup never regresses because of
/// a transient network problem, and records the failure so the store waits
/// before trying again.
fn refresh_now(inner: &Inner) {
    let Some(fetcher) = &inner.fetcher else {
        return;
    };
    let Some(table) = fetch_table(fetcher.as_ref()) else {
        inner.record_failure();
        return;
    };

    let fetched_at = SystemTime::now();
    if let Some(path) = &inner.cache_path
        && let Err(err) = write_cache_file(path, &table, fetched_at)
    {
        tracing::warn!(error = %err, "failed to write model-price cache file");
    }
    *inner.snapshot.write() = Snapshot {
        table,
        fetched_at: Some(fetched_at),
    };
    *inner.last_failure.lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    // ── PricingTable: longest-prefix matching ──

    #[test]
    fn longest_prefix_wins_over_a_broader_family_row() {
        let table = PricingTable::new()
            .with_entry("claude-opus-5", ModelPricing::new(5.0, 25.0))
            .with_entry("claude-opus-5-5", ModelPricing::new(4.0, 20.0));

        assert_eq!(
            table.get("claude-opus-5-5-20260315"),
            Some(ModelPricing::new(4.0, 20.0)),
            "the more specific row should win"
        );
        assert_eq!(
            table.get("claude-opus-5-20260101"),
            Some(ModelPricing::new(5.0, 25.0)),
            "a dated id for the broader family should still match it, not the sibling row"
        );
    }

    #[test]
    fn unknown_model_returns_none() {
        let table = PricingTable::new().with_entry("claude-opus-5", ModelPricing::new(5.0, 25.0));
        assert_eq!(table.get("gpt-5.5"), None);
    }

    #[test]
    fn merge_lets_the_second_table_override_shared_prefixes() {
        let base = PricingTable::new().with_entry("claude-opus-5", ModelPricing::new(5.0, 25.0));
        let overlay = PricingTable::new().with_entry("claude-opus-5", ModelPricing::new(1.0, 1.0));
        let merged = base.merge(overlay);
        assert_eq!(
            merged.get("claude-opus-5"),
            Some(ModelPricing::new(1.0, 1.0))
        );
    }

    // ── LiteLLM JSON parsing ──

    fn litellm_fixture() -> String {
        serde_json::json!({
            "claude-opus-5-5": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000004,
                "output_cost_per_token": 0.00002,
                "cache_read_input_token_cost": 0.0000002,
            },
            "us.anthropic.claude-opus-5-5": {
                "litellm_provider": "bedrock_converse",
                "input_cost_per_token": 0.0000044,
                "output_cost_per_token": 0.000022,
            },
            "gpt-5.5": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.000005,
                "output_cost_per_token": 0.00003,
            },
            "gemini-3-pro": {
                "litellm_provider": "vertex_ai-language-models",
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.00001,
            },
            "sample_spec": {
                "input_cost_per_token": 0.0,
                "output_cost_per_token": 0.0,
            },
            "malformed-anthropic-row": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": "not a number",
            },
        })
        .to_string()
    }

    #[test]
    fn parser_keeps_only_relevant_providers_and_converts_units() {
        let table = parse_litellm_json(&litellm_fixture()).expect("fixture should parse");

        assert_price_close(
            table.get("claude-opus-5-5"),
            ModelPricing::new(4.0, 20.0).with_cache_rates(0.2, 5.0),
            "per-token costs convert to per-million, and explicit cache read overrides the default",
        );
        // Bedrock's regional row prices differently from the first-party row.
        assert_price_close(
            table.get("us.anthropic.claude-opus-5-5"),
            ModelPricing::new(4.4, 22.0),
            "the Bedrock-tagged row must be kept and read verbatim, not merged with the first-party row",
        );
        assert_price_close(
            table.get("gpt-5.5"),
            ModelPricing::new(5.0, 30.0),
            "the OpenAI row",
        );
    }

    #[test]
    fn parser_skips_rows_it_cannot_read() {
        let table = parse_litellm_json(&litellm_fixture()).expect("fixture should parse");

        assert_eq!(
            table.get("gemini-3-pro"),
            None,
            "a provider outside {{anthropic, bedrock, bedrock_converse, openai}} is not our vendor"
        );
        assert_eq!(
            table.get("sample_spec"),
            None,
            "a row with no litellm_provider field is skipped"
        );
        assert_eq!(
            table.get("malformed-anthropic-row"),
            None,
            "a row whose cost field is not a number is skipped, not an error"
        );
    }

    #[test]
    fn parser_rejects_a_non_object_top_level_value() {
        assert!(parse_litellm_json("[1, 2, 3]").is_err());
        assert!(parse_litellm_json("not json").is_err());
    }

    #[test]
    fn parser_drops_rows_with_implausible_rates() {
        let body = serde_json::json!({
            "negative": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": -0.000004,
                "output_cost_per_token": 0.00002,
            },
            "all-zero": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.0,
                "output_cost_per_token": 0.0,
            },
            "absurd": {
                "litellm_provider": "openai",
                "input_cost_per_token": 1.0,
                "output_cost_per_token": 2.0,
            },
            "negative-cache-read": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000004,
                "output_cost_per_token": 0.00002,
                "cache_read_input_token_cost": -0.0000002,
            },
            "overflows-to-infinity": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 1e308,
                "output_cost_per_token": 0.00002,
            },
            "sane": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000015,
            },
        })
        .to_string();

        let table = parse_litellm_json(&body).expect("body should parse");

        for model in [
            "negative",
            "all-zero",
            "absurd",
            "negative-cache-read",
            "overflows-to-infinity",
        ] {
            assert_eq!(
                table.get(model),
                None,
                "row {model:?} has an implausible rate and must be dropped"
            );
        }
        assert_price_close(
            table.get("sane"),
            ModelPricing::new(3.0, 15.0),
            "a plausible row next to bad ones is still kept",
        );
        assert_eq!(table.len(), 1, "only the plausible row survives");
    }

    #[test]
    fn plausibility_check_accepts_a_free_output_rate_but_not_all_zero() {
        assert!(is_plausible(&ModelPricing::new(0.02, 0.0)));
        assert!(!is_plausible(&ModelPricing::new(0.0, 0.0)));
        assert!(!is_plausible(&ModelPricing::new(f64::NAN, 1.0)));
        assert!(!is_plausible(&ModelPricing::new(1.0, f64::INFINITY)));
        assert!(!is_plausible(
            &ModelPricing::new(1.0, 1.0).with_cache_rates(0.1, -1.0)
        ));
        assert!(!is_plausible(&ModelPricing::new(
            MAX_PLAUSIBLE_RATE_PER_MILLION_USD + 1.0,
            1.0
        )));
    }

    // ── Size limits ──

    #[test]
    fn capped_read_accepts_data_at_the_limit_and_rejects_data_over_it() {
        // Streamed, so the test does not also hold a copy of the source data.
        let source = |len: usize| std::io::repeat(b'a').take(len as u64);

        let read = read_capped(source(MAX_PRICE_DATA_BYTES)).expect("reading cannot fail");
        assert_eq!(
            read.map(|body| body.len()),
            Some(MAX_PRICE_DATA_BYTES),
            "data exactly at the limit is read whole"
        );

        let read = read_capped(source(MAX_PRICE_DATA_BYTES + 1)).expect("reading cannot fail");
        assert!(read.is_none(), "data one byte over the limit is rejected");
    }

    #[test]
    fn capped_read_rejects_data_that_is_not_utf8() {
        let err = read_capped([0xff_u8, 0xfe].as_slice())
            .expect_err("bytes that are not UTF-8 must not be returned as text");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_fetched_body_is_a_failed_refresh() {
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(
                "x".repeat(MAX_PRICE_DATA_BYTES + 1)
            )])),
            None,
            DEFAULT_TTL,
        );
        store.refresh_blocking();

        assert!(
            store.inner.last_failure.lock().is_some(),
            "a body over the size limit must count as a failed refresh"
        );
        assert!(
            store.inner.snapshot.read().table.is_empty(),
            "nothing from an oversized body may be served"
        );
    }

    #[test]
    fn oversized_cache_file_is_ignored() {
        let dir = TempDir::new("oversized");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        // A sparse file of zero bytes: over the limit without writing 32 MiB.
        std::fs::File::create(&cache_path)
            .and_then(|file| file.set_len(MAX_PRICE_DATA_BYTES as u64 + 1))
            .expect("temp cache file should be writable");

        assert!(
            read_cache_file(&cache_path).is_none(),
            "a cache file over the size limit must not be parsed"
        );
    }

    // ── PriceStore: fetch / cache / fallback / override ──

    /// Returns the queued results in order, then fails once they run out.
    struct SequenceFetcher {
        results: Mutex<VecDeque<Result<String, PriceFetchError>>>,
    }
    impl SequenceFetcher {
        fn new(results: Vec<Result<String, PriceFetchError>>) -> Self {
            Self {
                results: Mutex::new(results.into()),
            }
        }
    }
    impl PriceFetcher for SequenceFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            self.results
                .lock()
                .pop_front()
                .unwrap_or_else(|| Err(PriceFetchError::request("no more queued results")))
        }
    }

    struct FailingFetcher;
    impl PriceFetcher for FailingFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            Err(PriceFetchError::request("connection refused"))
        }
    }

    /// Counts fetches and always fails, so tests can see whether a refresh
    /// was attempted.
    struct CountingFailingFetcher {
        calls: Arc<AtomicU32>,
    }
    impl PriceFetcher for CountingFailingFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(PriceFetchError::request("connection refused"))
        }
    }

    /// Counts fetches, then blocks until the test sends on the release
    /// channel, so a refresh stays in flight for as long as the test needs.
    struct GatedFetcher {
        body: String,
        calls: Arc<AtomicU32>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl PriceFetcher for GatedFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.release
                .lock()
                .recv()
                .map_err(|_| PriceFetchError::request("release channel closed"))?;
            Ok(self.body.clone())
        }
    }

    fn opus_fixture(input_per_token: f64, output_per_token: f64) -> String {
        serde_json::json!({
            "claude-opus-5-5": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": input_per_token,
                "output_cost_per_token": output_per_token,
            }
        })
        .to_string()
    }

    /// Counts fetches and always returns the same body.
    struct CountingFetcher {
        body: String,
        calls: Arc<AtomicU32>,
    }
    impl PriceFetcher for CountingFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }
    }

    /// Panics on every fetch, like a buggy custom fetcher.
    struct PanickingFetcher;
    impl PriceFetcher for PanickingFetcher {
        fn fetch(&self) -> Result<String, PriceFetchError> {
            panic!("the fetcher blew up");
        }
    }

    /// A fresh directory under the system temp dir, removed on drop even if
    /// an assertion panics mid-test.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT_ID: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "polaris-price-store-test-{label}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("temp dir should be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The names of the entries directly inside `dir`.
    fn file_names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("dir should be readable")
            .map(|entry| {
                entry
                    .expect("dir entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// Writes a cache file whose rows were fetched `age` ago.
    fn write_cache_fixture(path: &Path, age: Duration, rows: &[(&str, StoredPricing)]) {
        let file = CacheFile {
            fetched_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after the epoch")
                .saturating_sub(age)
                .as_secs(),
            entries: rows
                .iter()
                .map(|(prefix, stored)| ((*prefix).to_string(), *stored))
                .collect(),
        };
        std::fs::write(
            path,
            serde_json::to_string(&file).expect("cache file serializes"),
        )
        .expect("temp cache file should be writable");
    }

    fn stored(input: f64, output: f64) -> StoredPricing {
        ModelPricing::new(input, output).into()
    }

    /// Compares every rate with a small tolerance: fetched rates are
    /// per-token values scaled to per-million, which is not exact in `f64`.
    #[track_caller]
    fn assert_price_close(actual: Option<ModelPricing>, expected: ModelPricing, context: &str) {
        const TOLERANCE: f64 = 1e-9;
        let actual = actual.unwrap_or_else(|| panic!("{context}: no price, expected {expected:?}"));
        let rates = [
            (
                "input",
                actual.input_per_million_usd,
                expected.input_per_million_usd,
            ),
            (
                "output",
                actual.output_per_million_usd,
                expected.output_per_million_usd,
            ),
            (
                "cache read",
                actual.cache_read_per_million_usd,
                expected.cache_read_per_million_usd,
            ),
            (
                "cache write",
                actual.cache_write_per_million_usd,
                expected.cache_write_per_million_usd,
            ),
        ];
        for (name, got, want) in rates {
            assert!(
                (got - want).abs() < TOLERANCE,
                "{context}: {name} rate is {got}, expected {want} (got {actual:?}, expected {expected:?})"
            );
        }
    }

    #[test]
    fn builtin_store_never_refreshes() {
        let store = PriceStore::builtin();
        assert!(
            store.refresh_if_due().is_none(),
            "a built-in store has no fetcher, so it must never start a refresh"
        );
        assert_eq!(
            store.get("claude-sonnet-4-6"),
            Some(ModelPricing::new(3.0, 15.0)),
            "a built-in store answers from the built-in table"
        );
    }

    #[test]
    fn empty_store_falls_back_to_the_built_in_table() {
        let store = PriceStore::with_fetcher(Arc::new(FailingFetcher), None, DEFAULT_TTL);
        // Run the refresh this store is due for, and wait for it, so no
        // thread outlives the test.
        store
            .refresh_if_due()
            .expect("a store with nothing fetched is due for a refresh")
            .join()
            .expect("refresh thread should not panic");

        assert_eq!(
            store.get("claude-sonnet-4-6"),
            Some(ModelPricing::new(3.0, 15.0)),
            "with nothing fetched or cached, the built-in fallback table answers"
        );
        assert_eq!(
            store.get("global.openai.gpt-6-luna"),
            Some(ModelPricing::new(0.10, 0.50).with_cache_rates(0.01, 0.125)),
        );
    }

    #[test]
    fn refresh_replaces_the_fallback_table_with_fetched_data() {
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(opus_fixture(
                0.000004, 0.00002,
            ))])),
            None,
            DEFAULT_TTL,
        );
        store.refresh_blocking();

        assert_price_close(
            store.get("claude-opus-5-5"),
            ModelPricing::new(4.0, 20.0),
            "fetched data should win once refreshed",
        );
        assert_eq!(
            store.get("global.openai.gpt-6-luna"),
            Some(ModelPricing::new(0.10, 0.50).with_cache_rates(0.01, 0.125)),
            "a fetched table that lacks a new model retains its fallback price"
        );
    }

    #[test]
    fn failed_refresh_keeps_serving_the_previous_snapshot() {
        // The first fetch succeeds with a price the built-in table does not
        // have (the built-in row for claude-opus-5-5 is 4.0/20.0), the second
        // fails.
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![
                Ok(opus_fixture(0.000007, 0.000035)),
                Err(PriceFetchError::Status { status: 503 }),
            ])),
            None,
            DEFAULT_TTL,
        );

        store.refresh_blocking();
        assert_price_close(
            store.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "the first, successful refresh should be served",
        );

        store.refresh_blocking();
        assert!(
            store.inner.last_failure.lock().is_some(),
            "the second refresh should have failed"
        );
        assert_price_close(
            store.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "a failed refresh must keep serving the previously fetched price, not the built-in one",
        );
    }

    #[test]
    fn failed_refresh_waits_before_trying_again() {
        let calls = Arc::new(AtomicU32::new(0));
        let store = PriceStore::with_fetcher(
            Arc::new(CountingFailingFetcher {
                calls: Arc::clone(&calls),
            }),
            None,
            DEFAULT_TTL,
        );

        store
            .refresh_if_due()
            .expect("a store with nothing fetched is due for a refresh")
            .join()
            .expect("refresh thread should not panic");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(
            store.refresh_if_due().is_none(),
            "within the retry delay after a failure, no new refresh may start"
        );
        let _ = store.get("claude-opus-5-5");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a lookup within the retry delay must not fetch again"
        );
    }

    #[test]
    fn failed_refresh_tries_again_once_the_retry_delay_has_passed() {
        let calls = Arc::new(AtomicU32::new(0));
        let store = PriceStore::with_fetcher(
            Arc::new(CountingFailingFetcher {
                calls: Arc::clone(&calls),
            }),
            None,
            DEFAULT_TTL,
        )
        .with_retry_delay(Duration::ZERO);

        for attempt in 1..=2 {
            store
                .refresh_if_due()
                .expect("with no retry delay, a failed store is due again")
                .join()
                .expect("refresh thread should not panic");
            assert_eq!(calls.load(Ordering::SeqCst), attempt);
        }
    }

    #[test]
    fn successful_refresh_clears_the_failure() {
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![
                Err(PriceFetchError::request("connection refused")),
                Ok(opus_fixture(0.000004, 0.00002)),
            ])),
            None,
            DEFAULT_TTL,
        );

        store.refresh_blocking();
        assert!(store.inner.last_failure.lock().is_some());
        store.refresh_blocking();
        assert!(
            store.inner.last_failure.lock().is_none(),
            "a successful refresh must end the wait after the earlier failure"
        );
    }

    #[test]
    fn override_beats_fetched_data() {
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(opus_fixture(
                0.000004, 0.00002,
            ))])),
            None,
            DEFAULT_TTL,
        );
        store.refresh_blocking();
        let store = store.with_override("claude-opus-5-5", ModelPricing::new(999.0, 999.0));

        assert_eq!(
            store.get("claude-opus-5-5"),
            Some(ModelPricing::new(999.0, 999.0)),
            "an override must beat the fetched row for the same model"
        );
    }

    #[test]
    fn override_stays_with_its_own_handle() {
        let shared = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(opus_fixture(
                0.000004, 0.00002,
            ))])),
            None,
            DEFAULT_TTL,
        );
        let overridden = shared
            .clone()
            .with_override("claude-opus-5-5", ModelPricing::new(1.0, 1.0));

        // A refresh through one handle is seen by every clone...
        shared.refresh_blocking();
        assert_price_close(
            shared.get("claude-opus-5-5"),
            ModelPricing::new(4.0, 20.0),
            "the handle without the override must not see it",
        );
        // ...but the override is seen only by the handle it was added to.
        assert_eq!(
            overridden.get("claude-opus-5-5"),
            Some(ModelPricing::new(1.0, 1.0)),
        );
        assert_eq!(
            overridden.get("claude-opus-5-5-other"),
            Some(ModelPricing::new(1.0, 1.0)),
            "an override is a prefix, like every other row"
        );
    }

    #[test]
    fn cache_file_round_trips_across_stores() {
        let dir = TempDir::new("round-trip");
        let cache_path = dir.path().join(CACHE_FILE_NAME);

        // Every rate differs from the built-in `claude-opus-5-5` row
        // (4.0 / 20.0, cache 0.2 / 5.0), so a reader that ignored the cache
        // and fell back to the built-in table could not pass.
        let fixture = serde_json::json!({
            "claude-opus-5-5": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000007,
                "output_cost_per_token": 0.000035,
                "cache_read_input_token_cost": 0.0000003,
                "cache_creation_input_token_cost": 0.000009,
            }
        })
        .to_string();
        let writer = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(fixture)])),
            Some(cache_path.clone()),
            DEFAULT_TTL,
        );
        writer.refresh_blocking();
        assert!(cache_path.is_file(), "the refresh should write the cache");

        // A fresh store reading the same cache file answers from it, and its
        // fetcher (which would serve yet another price) is never called.
        let calls = Arc::new(AtomicU32::new(0));
        let reader = PriceStore::with_fetcher(
            Arc::new(CountingFetcher {
                body: opus_fixture(0.000099, 0.000099),
                calls: Arc::clone(&calls),
            }),
            Some(cache_path),
            DEFAULT_TTL,
        );
        assert!(
            reader.refresh_if_due().is_none(),
            "a fresh cache file means no refresh is due"
        );
        assert_price_close(
            reader.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0).with_cache_rates(0.3, 9.0),
            "a fresh store should load the previous store's cache file from disk",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a fresh cache needs no fetch"
        );
    }

    #[test]
    fn cache_file_rows_with_implausible_rates_are_dropped() {
        let dir = TempDir::new("implausible");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        write_cache_fixture(
            &cache_path,
            Duration::ZERO,
            &[
                ("bad-model", stored(-4.0, 20.0)),
                ("good-model", stored(4.0, 20.0)),
            ],
        );

        let store =
            PriceStore::with_fetcher(Arc::new(FailingFetcher), Some(cache_path), DEFAULT_TTL);
        assert_eq!(store.get("bad-model"), None);
        assert_eq!(store.get("good-model"), Some(ModelPricing::new(4.0, 20.0)));
    }

    // ── PriceStore: freshness ──

    #[test]
    fn snapshot_expiry_follows_the_ttl() {
        let ttl = Duration::from_secs(60 * 60);
        let fetched = |ago: Duration| Snapshot {
            table: PricingTable::new(),
            fetched_at: Some(SystemTime::now() - ago),
        };

        assert!(
            Snapshot {
                table: PricingTable::new(),
                fetched_at: None,
            }
            .is_expired(ttl),
            "a snapshot that was never fetched is always expired"
        );
        assert!(!fetched(Duration::from_secs(60)).is_expired(ttl));
        assert!(
            fetched(ttl + Duration::from_secs(1)).is_expired(ttl),
            "a snapshot older than the TTL is expired"
        );
        assert!(
            fetched(ttl).is_expired(ttl),
            "a snapshot exactly as old as the TTL is expired"
        );
        assert!(
            fetched(Duration::ZERO).is_expired(Duration::ZERO),
            "a zero TTL expires everything"
        );
        assert!(
            Snapshot {
                table: PricingTable::new(),
                fetched_at: Some(SystemTime::now() + Duration::from_secs(60 * 60)),
            }
            .is_expired(ttl),
            "a fetch time in the future (clock skew or a bad file) cannot be trusted, so it is expired"
        );
    }

    #[test]
    fn stale_cache_is_served_until_a_refresh_replaces_it() {
        let dir = TempDir::new("stale");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        // Both the cached and the fetched price differ from the built-in row.
        write_cache_fixture(
            &cache_path,
            DEFAULT_TTL + Duration::from_secs(60),
            &[("claude-opus-5-5", stored(8.0, 40.0))],
        );
        let calls = Arc::new(AtomicU32::new(0));
        let store = PriceStore::with_fetcher(
            Arc::new(CountingFetcher {
                body: opus_fixture(0.000007, 0.000035),
                calls: Arc::clone(&calls),
            }),
            Some(cache_path.clone()),
            DEFAULT_TTL,
        );

        // Read the snapshot directly: `get` would start the refresh itself.
        assert_eq!(
            store.inner.snapshot.read().table.get("claude-opus-5-5"),
            Some(ModelPricing::new(8.0, 40.0)),
            "an expired cache file still seeds the store"
        );
        store
            .refresh_if_due()
            .expect("a cache older than the TTL is due for a refresh")
            .join()
            .expect("refresh thread should not panic");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_price_close(
            store.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "the refreshed price replaces the stale one",
        );
        let rewritten = read_cache_file(&cache_path).expect("the refresh rewrites the cache file");
        assert_price_close(
            rewritten.into_snapshot().table.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "the cache file now holds the refreshed price",
        );
        assert!(
            store.refresh_if_due().is_none(),
            "right after a refresh the prices are fresh again"
        );
    }

    #[test]
    fn fresh_cache_does_not_trigger_a_refresh() {
        let dir = TempDir::new("fresh");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        write_cache_fixture(
            &cache_path,
            Duration::from_secs(60 * 60),
            &[("claude-opus-5-5", stored(8.0, 40.0))],
        );
        let calls = Arc::new(AtomicU32::new(0));
        let store = PriceStore::with_fetcher(
            Arc::new(CountingFetcher {
                body: opus_fixture(0.000007, 0.000035),
                calls: Arc::clone(&calls),
            }),
            Some(cache_path),
            DEFAULT_TTL,
        );

        assert!(
            store.refresh_if_due().is_none(),
            "a cache younger than the TTL is not due for a refresh"
        );
        assert_eq!(
            store.get("claude-opus-5-5"),
            Some(ModelPricing::new(8.0, 40.0)),
            "lookups are served from the fresh cache"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a fresh cache must not cause a fetch"
        );
    }

    #[test]
    fn cache_age_is_judged_against_the_stores_own_ttl() {
        let dir = TempDir::new("ttl");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        write_cache_fixture(
            &cache_path,
            Duration::from_secs(2 * 60 * 60),
            &[("claude-opus-5-5", stored(8.0, 40.0))],
        );
        let store_with_ttl =
            |ttl| PriceStore::with_fetcher(Arc::new(FailingFetcher), Some(cache_path.clone()), ttl);

        assert!(
            store_with_ttl(Duration::from_secs(3 * 60 * 60))
                .refresh_if_due()
                .is_none(),
            "a two-hour-old cache is fresh under a three-hour TTL"
        );
        store_with_ttl(Duration::from_secs(60 * 60))
            .refresh_if_due()
            .expect("a two-hour-old cache is stale under a one-hour TTL")
            .join()
            .expect("refresh thread should not panic");
    }

    // ── PriceStore: bad payloads keep the previous snapshot ──

    #[test]
    fn bad_payload_keeps_the_previous_snapshot_and_cache_file() {
        let out_of_range = serde_json::json!({
            "negative": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": -0.000004,
                "output_cost_per_token": 0.00002,
            },
            "all-zero": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.0,
                "output_cost_per_token": 0.0,
            },
            "absurd": {
                "litellm_provider": "openai",
                "input_cost_per_token": 1.0,
                "output_cost_per_token": 2.0,
            },
            "overflows-to-infinity": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 1e308,
                "output_cost_per_token": 0.00002,
            },
        })
        .to_string();
        let unrelated_vendor = serde_json::json!({
            "gemini-3-pro": {
                "litellm_provider": "vertex_ai-language-models",
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.00001,
            },
        })
        .to_string();
        let bad_payloads = [
            ("invalid JSON", "{not json".to_string()),
            ("a top-level array", "[1, 2, 3]".to_string()),
            ("an empty object", "{}".to_string()),
            ("only unrelated vendors", unrelated_vendor),
            ("only out-of-range rates", out_of_range),
        ];

        for (label, payload) in bad_payloads {
            let dir = TempDir::new("bad-payload");
            let cache_path = dir.path().join(CACHE_FILE_NAME);
            let store = PriceStore::with_fetcher(
                Arc::new(SequenceFetcher::new(vec![
                    Ok(opus_fixture(0.000007, 0.000035)),
                    Ok(payload),
                ])),
                Some(cache_path.clone()),
                DEFAULT_TTL,
            );

            store.refresh_blocking();
            let fetched_at = store.inner.snapshot.read().fetched_at;
            assert!(fetched_at.is_some(), "{label}: the first refresh succeeds");
            assert!(store.inner.last_failure.lock().is_none());

            store.refresh_blocking();

            assert!(
                store.inner.last_failure.lock().is_some(),
                "{label}: must count as a failed refresh"
            );
            assert_eq!(
                store.inner.snapshot.read().fetched_at,
                fetched_at,
                "{label}: the snapshot must not be replaced, nor look freshly fetched"
            );
            assert_price_close(
                store.get("claude-opus-5-5"),
                ModelPricing::new(7.0, 35.0),
                &format!("{label}: the previous snapshot must keep being served"),
            );
            let cached = read_cache_file(&cache_path)
                .unwrap_or_else(|| panic!("{label}: the cache file must stay readable"));
            assert_price_close(
                cached.into_snapshot().table.get("claude-opus-5-5"),
                ModelPricing::new(7.0, 35.0),
                &format!("{label}: the cache file must not be overwritten with bad data"),
            );
        }
    }

    #[test]
    fn bad_payload_on_an_empty_store_leaves_it_on_the_built_in_table() {
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok("{not json".to_string())])),
            None,
            DEFAULT_TTL,
        );

        store.refresh_blocking();

        assert!(store.inner.last_failure.lock().is_some());
        assert!(store.inner.snapshot.read().table.is_empty());
        assert!(
            store.inner.snapshot.read().is_expired(DEFAULT_TTL),
            "nothing was fetched, so the store stays due for a refresh"
        );
        assert_eq!(
            store.get("claude-opus-5-5"),
            Some(ModelPricing::new(4.0, 20.0).with_cache_rates(0.20, 5.0)),
            "the built-in row answers"
        );
    }

    // ── Refresh thread: a panicking fetcher ──

    #[test]
    fn panicking_fetcher_does_not_wedge_the_refresh_flag() {
        let store = PriceStore::with_fetcher(Arc::new(PanickingFetcher), None, DEFAULT_TTL)
            .with_retry_delay(Duration::ZERO);

        let outcome = store
            .refresh_if_due()
            .expect("a store with nothing fetched is due for a refresh")
            .join();

        assert!(
            outcome.is_err(),
            "the fetcher's panic ends the refresh thread"
        );
        assert!(
            !store.inner.refreshing.load(Ordering::Acquire),
            "the refresh flag must be cleared when the refresh thread panics"
        );
        assert!(
            store.inner.last_failure.lock().is_some(),
            "a panicking fetcher counts as a failed refresh"
        );
        store
            .refresh_if_due()
            .expect("with the flag cleared, a later lookup can refresh again")
            .join()
            .expect_err("the fetcher still panics");
    }

    #[test]
    fn panicking_fetcher_is_retried_only_after_the_retry_delay() {
        let store = PriceStore::with_fetcher(Arc::new(PanickingFetcher), None, DEFAULT_TTL);

        store
            .refresh_if_due()
            .expect("a store with nothing fetched is due for a refresh")
            .join()
            .expect_err("the fetcher panics");

        // Check why no refresh starts: the retry delay, not a stuck flag.
        assert!(
            !store.inner.refreshing.load(Ordering::Acquire),
            "no refresh is in flight after the thread ended"
        );
        assert!(
            store.inner.waiting_after_failure(),
            "a panic starts the retry delay, like any failed refresh"
        );
        assert!(
            store.refresh_if_due().is_none(),
            "after a panic the store waits out the retry delay, like after any failure"
        );
        assert_eq!(
            store.get("claude-sonnet-4-6"),
            Some(ModelPricing::new(3.0, 15.0)),
            "lookups keep working from the built-in table"
        );
    }

    // ── Cache file I/O ──

    #[test]
    fn reading_a_missing_cache_file_gives_none() {
        let dir = TempDir::new("read-missing");

        assert!(read_cache_file(&dir.path().join(CACHE_FILE_NAME)).is_none());
        assert!(
            read_cache_file(&dir.path().join("no-such-dir").join(CACHE_FILE_NAME)).is_none(),
            "a missing parent directory is just a missing file"
        );
    }

    #[test]
    fn reading_a_corrupt_cache_file_gives_none() {
        let dir = TempDir::new("read-corrupt");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        let good_rows = serde_json::json!({
            "claude-opus-5-5": {
                "input_per_million_usd": 7.0,
                "output_per_million_usd": 35.0,
                "cache_read_per_million_usd": 0.7,
                "cache_write_per_million_usd": 8.75,
            }
        });
        let corrupt_files: [(&str, Vec<u8>); 7] = [
            ("empty file", Vec::new()),
            ("not JSON", b"this is not json".to_vec()),
            (
                "truncated JSON",
                br#"{"fetched_at_unix_secs": 1, "entries": {"cl"#.to_vec(),
            ),
            ("not UTF-8", vec![0xff, 0xfe, 0x00, 0x7b]),
            ("wrong top-level type", b"[1, 2, 3]".to_vec()),
            (
                "missing field",
                serde_json::json!({ "entries": good_rows })
                    .to_string()
                    .into_bytes(),
            ),
            (
                "wrong field type",
                serde_json::json!({ "fetched_at_unix_secs": "yesterday", "entries": good_rows })
                    .to_string()
                    .into_bytes(),
            ),
        ];

        for (label, bytes) in corrupt_files {
            std::fs::write(&cache_path, bytes).expect("temp cache file should be writable");
            assert!(
                read_cache_file(&cache_path).is_none(),
                "{label}: a corrupt cache file must read as no cache"
            );
        }
    }

    #[test]
    fn corrupt_cache_file_leaves_the_store_empty_and_due_a_refresh_that_repairs_it() {
        let dir = TempDir::new("self-heal");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        std::fs::write(&cache_path, "{ definitely not a cache file")
            .expect("temp cache file should be writable");
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(opus_fixture(
                0.000007, 0.000035,
            ))])),
            Some(cache_path.clone()),
            DEFAULT_TTL,
        );

        assert!(
            store.inner.snapshot.read().table.is_empty(),
            "nothing may be taken from a corrupt cache file"
        );
        store
            .refresh_if_due()
            .expect("with no usable cache, the store is due for a refresh")
            .join()
            .expect("refresh thread should not panic");

        assert_price_close(
            read_cache_file(&cache_path)
                .expect("the refresh replaces the corrupt file with a valid one")
                .into_snapshot()
                .table
                .get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "the repaired cache file",
        );
    }

    #[test]
    fn written_cache_file_reads_back_with_its_rows_and_time() {
        let dir = TempDir::new("write-read");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        let table = PricingTable::new()
            .with_entry(
                "model-a",
                ModelPricing::new(1.5, 7.5).with_cache_rates(0.25, 2.0),
            )
            .with_entry("model-b", ModelPricing::new(3.0, 9.0));
        let fetched_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        write_cache_file(&cache_path, &table, fetched_at).expect("the cache should be writable");

        let snapshot = read_cache_file(&cache_path)
            .expect("a written cache file should read back")
            .into_snapshot();
        assert_eq!(snapshot.fetched_at, Some(fetched_at));
        assert_eq!(snapshot.table.len(), 2);
        assert_eq!(
            snapshot.table.get("model-a"),
            Some(ModelPricing::new(1.5, 7.5).with_cache_rates(0.25, 2.0))
        );
        assert_eq!(
            snapshot.table.get("model-b"),
            Some(ModelPricing::new(3.0, 9.0))
        );
    }

    #[test]
    fn writing_a_cache_file_creates_missing_parent_directories() {
        let dir = TempDir::new("write-nested");
        let cache_path = dir.path().join("a").join("b").join(CACHE_FILE_NAME);

        write_cache_file(
            &cache_path,
            &PricingTable::new().with_entry("model-a", ModelPricing::new(1.0, 2.0)),
            SystemTime::now(),
        )
        .expect("missing parent directories should be created");

        assert!(read_cache_file(&cache_path).is_some());
    }

    #[test]
    fn writing_a_cache_file_replaces_the_old_one_and_leaves_no_temp_file() {
        let dir = TempDir::new("write-replace");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        for input in [1.0, 2.0, 3.0] {
            write_cache_file(
                &cache_path,
                &PricingTable::new().with_entry("model-a", ModelPricing::new(input, 10.0)),
                SystemTime::now(),
            )
            .expect("the cache should be writable");
        }

        assert_eq!(
            file_names_in(dir.path()),
            [CACHE_FILE_NAME],
            "the temp file written beside the cache must be renamed away"
        );
        let table = read_cache_file(&cache_path)
            .expect("the cache should read back")
            .into_snapshot()
            .table;
        assert_eq!(table.get("model-a"), Some(ModelPricing::new(3.0, 10.0)));
    }

    #[test]
    fn writing_a_cache_file_never_exposes_a_partial_file() {
        let dir = TempDir::new("write-atomic");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        let table = PricingTable::new().with_entry("model-a", ModelPricing::new(1.0, 2.0));
        write_cache_file(&cache_path, &table, SystemTime::now()).expect("first write");
        let before = std::fs::read(&cache_path).expect("cache should be readable");

        // Hold the cache file open for reading while it is replaced: a write
        // in place would change what this handle reads, a rename does not.
        let mut reader = std::fs::File::open(&cache_path).expect("cache should open");
        let replacement = PricingTable::new().with_entry("model-b", ModelPricing::new(3.0, 4.0));
        write_cache_file(&cache_path, &replacement, SystemTime::now()).expect("second write");

        let mut seen = Vec::new();
        reader
            .read_to_end(&mut seen)
            .expect("old handle should read");
        assert_eq!(seen, before, "an open reader still sees the whole old file");
        let table = read_cache_file(&cache_path)
            .expect("the new file should read back")
            .into_snapshot()
            .table;
        assert_eq!(table.get("model-b"), Some(ModelPricing::new(3.0, 4.0)));
        assert_eq!(table.get("model-a"), None);
    }

    #[test]
    fn writing_a_cache_file_under_a_regular_file_fails() {
        let dir = TempDir::new("write-parent-is-file");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("temp file should be writable");

        let result = write_cache_file(
            &blocker.join(CACHE_FILE_NAME),
            &PricingTable::new().with_entry("model-a", ModelPricing::new(1.0, 2.0)),
            SystemTime::now(),
        );

        assert!(
            result.is_err(),
            "a parent that is a regular file cannot hold the cache"
        );
        assert_eq!(
            file_names_in(dir.path()),
            ["blocker"],
            "nothing else is left behind"
        );
    }

    #[test]
    fn writing_a_cache_file_over_a_directory_fails_without_leaving_a_temp_file() {
        let dir = TempDir::new("write-target-is-dir");
        let cache_path = dir.path().join(CACHE_FILE_NAME);
        std::fs::create_dir(&cache_path).expect("temp dir should be creatable");

        let result = write_cache_file(
            &cache_path,
            &PricingTable::new().with_entry("model-a", ModelPricing::new(1.0, 2.0)),
            SystemTime::now(),
        );

        assert!(
            result.is_err(),
            "a directory cannot be replaced by the cache file"
        );
        assert_eq!(
            file_names_in(dir.path()),
            [CACHE_FILE_NAME],
            "the failed write must clean up its temp file"
        );
        assert!(cache_path.is_dir(), "the directory is left as it was");
    }

    #[test]
    fn refresh_still_serves_fetched_prices_when_the_cache_cannot_be_written() {
        let dir = TempDir::new("unwritable");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("temp file should be writable");
        let store = PriceStore::with_fetcher(
            Arc::new(SequenceFetcher::new(vec![Ok(opus_fixture(
                0.000007, 0.000035,
            ))])),
            Some(blocker.join(CACHE_FILE_NAME)),
            DEFAULT_TTL,
        );

        store.refresh_blocking();

        assert_price_close(
            store.get("claude-opus-5-5"),
            ModelPricing::new(7.0, 35.0),
            "a cache write failure must not lose the fetched prices",
        );
        assert!(
            store.inner.last_failure.lock().is_none(),
            "the fetch itself succeeded"
        );
    }

    #[test]
    fn expired_snapshot_triggers_exactly_one_background_refresh() {
        let calls = Arc::new(AtomicU32::new(0));
        let (release, gate) = mpsc::channel();
        let store = PriceStore::with_fetcher(
            Arc::new(GatedFetcher {
                body: opus_fixture(0.000004, 0.00002),
                calls: Arc::clone(&calls),
                release: Mutex::new(gate),
            }),
            None,
            Duration::ZERO, // always expired
        );

        // The first lookup past the expired snapshot starts the refresh. The
        // fetcher then blocks until released, so the refresh is certainly
        // still in flight for every later lookup.
        let refresh = store
            .refresh_if_due()
            .expect("an expired store is due for a refresh");
        for _ in 0..4 {
            assert!(
                store.refresh_if_due().is_none(),
                "no second refresh may start while one is in flight"
            );
            let _ = store.get("claude-opus-5-5");
        }

        release
            .send(())
            .expect("the fetcher should still be waiting for release");
        refresh.join().expect("refresh thread should not panic");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "back-to-back lookups against an expired snapshot should share one in-flight refresh"
        );
    }
}

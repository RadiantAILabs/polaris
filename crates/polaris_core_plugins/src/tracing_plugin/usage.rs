//! Token usage aggregation over [`SpanRecord`]s.
//!
//! Token counts arrive on `chat` spans as OpenTelemetry `GenAI` attributes
//! recorded by [`TracingLlmProvider`](super::instrument::llm::TracingLlmProvider):
//! `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, the cache tiers
//! `gen_ai.usage.cache_read_tokens` / `gen_ai.usage.cache_creation_tokens`,
//! `gen_ai.request.model`, and `gen_ai.provider.name`. The aggregator walks
//! the dashboard's in-memory [`SpanBuffer`](super::SpanBuffer) and projects
//! totals and per-model / per-provider / per-`agent_type` breakdowns.
//!
//! When a [`UsagePricing`] table is supplied, each `(provider, model)`
//! bucket's tokens are multiplied by the registered per-million-token rate
//! and the result is summed into `cost_usd` on every level of the response.

use super::SpanRecord;
use super::UsagePricing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Field key carrying input-token count on `chat` spans.
const INPUT_TOKENS_KEY: &str = "gen_ai.usage.input_tokens";
/// Field key carrying output-token count on `chat` spans.
const OUTPUT_TOKENS_KEY: &str = "gen_ai.usage.output_tokens";
/// Field key carrying cache-read input-token count on `chat` spans.
const CACHE_READ_TOKENS_KEY: &str = "gen_ai.usage.cache_read_tokens";
/// Field key carrying cache-creation input-token count on `chat` spans.
const CACHE_CREATION_TOKENS_KEY: &str = "gen_ai.usage.cache_creation_tokens";
/// Field key carrying the model identifier on `chat` spans.
const MODEL_KEY: &str = "gen_ai.request.model";
/// Field key carrying the provider name on `chat` spans.
const PROVIDER_KEY: &str = "gen_ai.provider.name";
/// Label key carrying the agent type (sessions plugin convention).
const AGENT_TYPE_LABEL: &str = "agent_type";
/// Placeholder bucket key for records that lack one of the breakdown
/// attributes — surfaces attribution gaps explicitly rather than silently
/// dropping the tokens.
const UNKNOWN_KEY: &str = "unknown";

/// Per-bucket token totals (and optional computed cost).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct TokenUsageTotals {
    /// Sum of full-price (uncached) input tokens reported by the aggregated
    /// spans.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub input_tokens: u64,
    /// Sum of output tokens reported by the aggregated spans.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub output_tokens: u64,
    /// Sum of input tokens served from the prompt cache (billed at the
    /// cache-read rate). Zero for providers/calls without prompt caching.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub cache_read_tokens: u64,
    /// Sum of input tokens written to the prompt cache (billed at the
    /// cache-write rate). Zero for providers/calls without prompt caching.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub cache_creation_tokens: u64,
    /// `input_tokens + output_tokens + cache_read_tokens + cache_creation_tokens`.
    /// Pre-computed for convenience; matches each call's `Usage::total_tokens`.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub total_tokens: u64,
    /// Computed cost in USD when a [`UsagePricing`] table covered at least
    /// one of the aggregated `(provider, model)` pairs. `None` when no
    /// pricing was registered or no rate matched a contributing bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(type = "number | null"))]
    pub cost_usd: Option<f64>,
}

/// One row in a breakdown — totals attributed to a single key value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct TokenUsageBreakdown {
    /// Breakdown key — the model, provider, or `agent_type` the row totals
    /// belong to. The literal string `"unknown"` is used when the source
    /// span lacked the corresponding attribute.
    pub key: String,
    /// Totals attributed to this key.
    pub usage: TokenUsageTotals,
}

/// Wire response for the `/v1/.../usage` endpoint family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct TokenUsageResponse {
    /// Aggregate totals across every record that contributed.
    pub totals: TokenUsageTotals,
    /// Per-model breakdown, keyed by `gen_ai.request.model`. Sorted by
    /// descending `total_tokens`, then by key for stability.
    pub by_model: Vec<TokenUsageBreakdown>,
    /// Per-provider breakdown, keyed by `gen_ai.provider.name`. Same sort.
    pub by_provider: Vec<TokenUsageBreakdown>,
    /// Per-`agent_type` breakdown, keyed by the `agent_type` correlation
    /// label. Same sort.
    pub by_agent_type: Vec<TokenUsageBreakdown>,
    /// Number of span records that contributed at least one token. Useful
    /// when distinguishing "no LLM calls happened" from "buffer is empty".
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub source_span_count: usize,
}

/// Aggregates token usage across a stream of span records.
///
/// Records that do not carry either `gen_ai.usage.input_tokens` or
/// `gen_ai.usage.output_tokens` are skipped silently.
pub(super) fn aggregate<'a, I>(records: I, pricing: Option<&UsagePricing>) -> TokenUsageResponse
where
    I: IntoIterator<Item = &'a SpanRecord>,
{
    let mut totals = TokenAcc::default();
    let mut by_model: BTreeMap<String, TokenAcc> = BTreeMap::new();
    let mut by_provider: BTreeMap<String, TokenAcc> = BTreeMap::new();
    let mut by_agent_type: BTreeMap<String, TokenAcc> = BTreeMap::new();
    let mut source_span_count: usize = 0;

    for record in records {
        let input = record
            .fields
            .get(INPUT_TOKENS_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = record
            .fields
            .get(OUTPUT_TOKENS_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_read = record
            .fields
            .get(CACHE_READ_TOKENS_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation = record
            .fields
            .get(CACHE_CREATION_TOKENS_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if input == 0 && output == 0 && cache_read == 0 && cache_creation == 0 {
            continue;
        }

        let model = record
            .fields
            .get(MODEL_KEY)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN_KEY);
        let provider = record
            .fields
            .get(PROVIDER_KEY)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN_KEY);
        let agent_type = record
            .labels
            .get(AGENT_TYPE_LABEL)
            .map_or(UNKNOWN_KEY, String::as_str);

        let cost_usd = pricing
            .and_then(|pricing| pricing.get(provider, model))
            .map(|rate| rate.cost_with_cache(input, output, cache_read, cache_creation));

        let counts = TokenCounts {
            input,
            output,
            cache_read,
            cache_creation,
        };
        totals.add(counts, cost_usd);
        by_model
            .entry(model.to_owned())
            .or_default()
            .add(counts, cost_usd);
        by_provider
            .entry(provider.to_owned())
            .or_default()
            .add(counts, cost_usd);
        by_agent_type
            .entry(agent_type.to_owned())
            .or_default()
            .add(counts, cost_usd);

        source_span_count += 1;
    }

    TokenUsageResponse {
        totals: totals.into_totals(),
        by_model: finish_breakdown(by_model),
        by_provider: finish_breakdown(by_provider),
        by_agent_type: finish_breakdown(by_agent_type),
        source_span_count,
    }
}

fn finish_breakdown(map: BTreeMap<String, TokenAcc>) -> Vec<TokenUsageBreakdown> {
    let mut rows: Vec<TokenUsageBreakdown> = map
        .into_iter()
        .map(|(key, acc)| TokenUsageBreakdown {
            key,
            usage: acc.into_totals(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.usage
            .total_tokens
            .cmp(&a.usage.total_tokens)
            .then_with(|| a.key.cmp(&b.key))
    });
    rows
}

/// Token counts read from a single `chat` span, grouped so they travel through
/// the accumulators as one value.
#[derive(Clone, Copy)]
struct TokenCounts {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

#[derive(Default)]
struct TokenAcc {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: Option<f64>,
}

impl TokenAcc {
    fn add(&mut self, counts: TokenCounts, cost: Option<f64>) {
        self.input_tokens = self.input_tokens.saturating_add(counts.input);
        self.output_tokens = self.output_tokens.saturating_add(counts.output);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(counts.cache_read);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(counts.cache_creation);
        if let Some(amount) = cost {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + amount);
        }
    }

    fn into_totals(self) -> TokenUsageTotals {
        let total_tokens = self
            .input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens);
        TokenUsageTotals {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            total_tokens,
            cost_usd: self.cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_plugin::ModelPricing;
    use crate::tracing_plugin::SpanKind;
    use serde_json::json;

    fn chat_record(
        run_id: &str,
        provider: &str,
        model: &str,
        agent_type: Option<&str>,
        input: u64,
        output: u64,
    ) -> SpanRecord {
        let mut rec = SpanRecord::new(
            "2026-05-15T12:00:00.000Z",
            "info",
            "tests",
            "chat",
            SpanKind::SpanClose,
        )
        .with_started_at("2026-05-15T11:59:59.000Z")
        .with_duration_ms(10)
        .with_run_id(run_id)
        .with_field(MODEL_KEY, json!(model))
        .with_field(PROVIDER_KEY, json!(provider))
        .with_field(INPUT_TOKENS_KEY, json!(input))
        .with_field(OUTPUT_TOKENS_KEY, json!(output));
        if let Some(agent) = agent_type {
            rec = rec.with_label(AGENT_TYPE_LABEL, agent);
        }
        rec
    }

    #[test]
    fn empty_input_returns_zeroed_response() {
        let response = aggregate(std::iter::empty::<&SpanRecord>(), None);
        assert_eq!(response.totals, TokenUsageTotals::default());
        assert!(response.by_model.is_empty());
        assert!(response.by_provider.is_empty());
        assert!(response.by_agent_type.is_empty());
        assert_eq!(response.source_span_count, 0);
    }

    #[test]
    fn sums_tokens_into_totals_and_breakdowns() {
        let records = [
            chat_record("r1", "anthropic", "claude-opus-4-7", Some("react"), 100, 50),
            chat_record("r1", "anthropic", "claude-opus-4-7", Some("react"), 200, 75),
            chat_record("r2", "openai", "gpt-5", Some("rewoo"), 10, 20),
        ];
        let response = aggregate(records.iter(), None);

        assert_eq!(response.totals.input_tokens, 310);
        assert_eq!(response.totals.output_tokens, 145);
        assert_eq!(response.totals.total_tokens, 455);
        assert!(response.totals.cost_usd.is_none(), "no pricing → no cost");
        assert_eq!(response.source_span_count, 3);

        // Highest-volume model first.
        assert_eq!(response.by_model[0].key, "claude-opus-4-7");
        assert_eq!(response.by_model[0].usage.input_tokens, 300);
        assert_eq!(response.by_model[0].usage.output_tokens, 125);
        assert_eq!(response.by_model[1].key, "gpt-5");

        assert_eq!(response.by_provider[0].key, "anthropic");
        assert_eq!(response.by_agent_type[0].key, "react");
    }

    #[test]
    fn missing_attributes_attribute_to_unknown_bucket() {
        let mut rec = SpanRecord::new(
            "2026-05-15T12:00:00.000Z",
            "info",
            "tests",
            "chat",
            SpanKind::SpanClose,
        )
        .with_run_id("r")
        .with_field(INPUT_TOKENS_KEY, json!(5_u64))
        .with_field(OUTPUT_TOKENS_KEY, json!(7_u64));
        // No provider, no model, no agent_type label.
        rec.fields.remove(MODEL_KEY);
        rec.fields.remove(PROVIDER_KEY);

        let response = aggregate(std::iter::once(&rec), None);
        assert_eq!(response.totals.total_tokens, 12);
        assert_eq!(response.by_model[0].key, UNKNOWN_KEY);
        assert_eq!(response.by_provider[0].key, UNKNOWN_KEY);
        assert_eq!(response.by_agent_type[0].key, UNKNOWN_KEY);
    }

    #[test]
    fn skips_records_with_no_token_attributes() {
        let mut rec = SpanRecord::new(
            "2026-05-15T12:00:00.000Z",
            "info",
            "tests",
            "noisy",
            SpanKind::Event,
        )
        .with_field("unrelated", json!("yes"));
        rec.run_id = Some("r".into());
        let response = aggregate(std::iter::once(&rec), None);
        assert_eq!(response.source_span_count, 0);
        assert_eq!(response.totals, TokenUsageTotals::default());
    }

    #[test]
    fn pricing_lookup_multiplies_per_million_rate() {
        let pricing = UsagePricing::new();
        pricing.set(
            "anthropic",
            "claude-opus-4-7",
            ModelPricing::new(15.0, 75.0),
        );
        let records = [chat_record(
            "r",
            "anthropic",
            "claude-opus-4-7",
            Some("react"),
            1_000_000,
            500_000,
        )];
        let response = aggregate(records.iter(), Some(&pricing));
        // 1M input * $15/M + 500k output * $75/M = $15 + $37.5 = $52.5.
        let cost = response.totals.cost_usd.expect("cost present");
        assert!((cost - 52.5).abs() < 1e-9, "expected $52.50, got {cost}");
        // Pricing also enriches breakdown rows.
        assert!(response.by_model[0].usage.cost_usd.is_some());
    }

    #[test]
    fn cache_tokens_count_toward_totals_and_are_priced_at_cache_tiers() {
        let pricing = UsagePricing::new();
        // new() seeds Anthropic ephemeral ratios: cache-read 0.1x, write 1.25x.
        pricing.set(
            "anthropic",
            "claude-opus-4-7",
            ModelPricing::new(15.0, 75.0),
        );
        let rec = chat_record(
            "r",
            "anthropic",
            "claude-opus-4-7",
            Some("react"),
            1_000_000,
            500_000,
        )
        .with_field(CACHE_READ_TOKENS_KEY, json!(2_000_000_u64))
        .with_field(CACHE_CREATION_TOKENS_KEY, json!(100_000_u64));

        let response = aggregate(std::iter::once(&rec), Some(&pricing));
        let totals = &response.totals;

        assert_eq!(totals.input_tokens, 1_000_000);
        assert_eq!(totals.output_tokens, 500_000);
        assert_eq!(totals.cache_read_tokens, 2_000_000);
        assert_eq!(totals.cache_creation_tokens, 100_000);
        // total_tokens now folds in the cache tiers (matches Usage::total_tokens).
        assert_eq!(totals.total_tokens, 3_600_000);

        // input 1M*$15/M = $15 ; output 500k*$75/M = $37.5 ;
        // cache-read 2M*$1.5/M = $3 ; cache-write 100k*$18.75/M = $1.875.
        let cost = totals.cost_usd.expect("cost present");
        assert!(
            (cost - 57.375).abs() < 1e-9,
            "expected $57.375 incl. cache tiers, got {cost}"
        );
        // The breakdown rows carry the cache tiers too.
        assert_eq!(response.by_model[0].usage.cache_read_tokens, 2_000_000);
    }

    #[test]
    fn cache_only_record_is_not_skipped() {
        // A span with zero fresh input/output but non-zero cache tokens must
        // still be aggregated — the skip guard keys off all token tiers.
        let rec = chat_record("r", "anthropic", "claude-opus-4-7", None, 0, 0)
            .with_field(CACHE_READ_TOKENS_KEY, json!(1_000_u64));
        let response = aggregate(std::iter::once(&rec), None);
        assert_eq!(response.source_span_count, 1);
        assert_eq!(response.totals.cache_read_tokens, 1_000);
        assert_eq!(response.totals.total_tokens, 1_000);
    }

    #[test]
    fn pricing_with_no_matching_rate_leaves_cost_none() {
        let pricing = UsagePricing::new();
        pricing.set(
            "anthropic",
            "claude-opus-4-7",
            ModelPricing::new(15.0, 75.0),
        );
        let records = [chat_record("r", "openai", "gpt-5", Some("react"), 100, 100)];
        let response = aggregate(records.iter(), Some(&pricing));
        assert!(response.totals.cost_usd.is_none());
        assert!(response.by_provider[0].usage.cost_usd.is_none());
    }

    #[test]
    fn breakdown_rows_sorted_by_descending_total_tokens() {
        let records = [
            chat_record("r", "anthropic", "claude-haiku-4-5", None, 1, 1),
            chat_record("r", "anthropic", "claude-opus-4-7", None, 100, 100),
            chat_record("r", "anthropic", "claude-sonnet-4-6", None, 10, 10),
        ];
        let response = aggregate(records.iter(), None);
        let model_keys: Vec<&str> = response
            .by_model
            .iter()
            .map(|row| row.key.as_str())
            .collect();
        assert_eq!(
            model_keys,
            vec!["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]
        );
    }
}

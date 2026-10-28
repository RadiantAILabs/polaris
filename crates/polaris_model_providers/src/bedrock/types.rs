//! Primitive type conversions between Polaris and Bedrock types.

use aws_sdk_bedrockruntime::types as bedrock;
use aws_smithy_types::Document;
use polaris_models::llm::{self as polaris_llm, GenerationError};

/// Converts a Polaris image media type to a Bedrock image format.
pub fn convert_image_format(
    media_type: &polaris_llm::ImageMediaType,
) -> Result<bedrock::ImageFormat, GenerationError> {
    match media_type {
        polaris_llm::ImageMediaType::JPEG => Ok(bedrock::ImageFormat::Jpeg),
        polaris_llm::ImageMediaType::PNG => Ok(bedrock::ImageFormat::Png),
        polaris_llm::ImageMediaType::GIF => Ok(bedrock::ImageFormat::Gif),
        polaris_llm::ImageMediaType::WEBP => Ok(bedrock::ImageFormat::Webp),
        other => Err(GenerationError::UnsupportedContent(format!(
            "unsupported image media type for Bedrock: {other:?}"
        ))),
    }
}

/// Converts a Polaris document media type to a Bedrock document format.
pub fn convert_document_format(
    media_type: &polaris_llm::DocumentMediaType,
) -> bedrock::DocumentFormat {
    match media_type {
        polaris_llm::DocumentMediaType::PDF => bedrock::DocumentFormat::Pdf,
        polaris_llm::DocumentMediaType::TXT => bedrock::DocumentFormat::Txt,
        polaris_llm::DocumentMediaType::HTML => bedrock::DocumentFormat::Html,
        polaris_llm::DocumentMediaType::CSV => bedrock::DocumentFormat::Csv,
        polaris_llm::DocumentMediaType::MARKDOWN => bedrock::DocumentFormat::Md,
    }
}

/// Converts a Polaris tool result status to a Bedrock tool result status.
pub fn convert_tool_result_status(
    status: &polaris_llm::ToolResultStatus,
) -> bedrock::ToolResultStatus {
    match status {
        polaris_llm::ToolResultStatus::Success => bedrock::ToolResultStatus::Success,
        polaris_llm::ToolResultStatus::Error => bedrock::ToolResultStatus::Error,
    }
}

/// Decodes a base64-encoded document source into raw bytes.
pub fn decode_base64_source(
    source: &polaris_llm::DocumentSource,
) -> Result<Vec<u8>, GenerationError> {
    match source {
        polaris_llm::DocumentSource::Base64(data) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|err| {
                    GenerationError::InvalidRequest(format!("failed to decode base64 data: {err}"))
                })
        }
    }
}

/// Converts a `serde_json::Value` to an AWS Smithy `Document`.
pub fn json_to_document(value: &serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(f) = n.as_f64() {
                Document::Number(aws_smithy_types::Number::Float(f))
            } else {
                tracing::warn!("cannot convert {n} to Document number, using null");
                Document::Null
            }
        }
        serde_json::Value::String(s) => Document::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Document::Array(arr.iter().map(json_to_document).collect())
        }
        serde_json::Value::Object(obj) => Document::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect(),
        ),
    }
}

/// Converts an AWS Smithy `Document` to a `serde_json::Value`.
pub fn document_to_json(doc: &Document) -> serde_json::Value {
    match doc {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(*b),
        Document::Number(n) => match n {
            aws_smithy_types::Number::PosInt(i) => serde_json::json!(*i),
            aws_smithy_types::Number::NegInt(i) => serde_json::json!(*i),
            aws_smithy_types::Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "cannot convert {f} to JSON (NaN/Infinity not supported), using null"
                    );
                    serde_json::Value::Null
                }),
        },
        Document::String(s) => serde_json::Value::String(s.clone()),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(document_to_json).collect())
        }
        Document::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), document_to_json(v)))
                .collect(),
        ),
    }
}

/// Whether a Bedrock model id accepts explicit `CachePoint` blocks.
///
/// Bedrock documents explicit prompt caching for Anthropic Claude and Amazon
/// Nova models. Other vendors' models (`OpenAI`, Moonshot Kimi, xAI Grok) cache
/// prefixes automatically and reject a request that carries a cache point, so
/// the provider must leave the markers out for them.
pub fn accepts_cache_points(id: &str) -> bool {
    id.contains("anthropic.") || id.contains("amazon.nova")
}

/// Whether the model accepts its earlier reasoning back in the message
/// history. Only Claude round-trips reasoning; `OpenAI` models on Bedrock
/// reject a `reasoningText` block in assistant messages.
pub fn accepts_reasoning_history(id: &str) -> bool {
    id.contains("anthropic.")
}

/// Strips a Bedrock cross-region-inference-profile prefix (`us.`, `eu.`,
/// `apac.`, `global.`, ...) and the `anthropic.` vendor prefix, and a
/// trailing Bedrock version suffix (`-v1:0`), leaving a bare id shaped like a
/// first-party Anthropic model id (e.g. `us.anthropic.claude-opus-4-1-20250805-v1:0`
/// -> `claude-opus-4-1-20250805`).
///
/// Returns `id` unchanged when it contains no `anthropic.` substring (not a
/// Bedrock Anthropic id), so passing an already-bare id is a no-op rather
/// than mangling it.
#[must_use]
pub fn normalize_bedrock_model_id(id: &str) -> &str {
    const VENDOR_PREFIX: &str = "anthropic.";
    let without_prefix = match id.find(VENDOR_PREFIX) {
        Some(idx) => &id[idx + VENDOR_PREFIX.len()..],
        None => return id,
    };
    strip_bedrock_version_suffix(without_prefix)
}

/// Strips a trailing `-v<digits>:<digits>` Bedrock version suffix (e.g.
/// `-v1:0`), if present; returns `id` unchanged otherwise.
fn strip_bedrock_version_suffix(id: &str) -> &str {
    let Some(colon_idx) = id.rfind(':') else {
        return id;
    };
    let (before_colon, minor) = id.split_at(colon_idx);
    let minor = &minor[1..];
    if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
        return id;
    }
    let Some(v_idx) = before_colon.rfind("-v") else {
        return id;
    };
    let major = &before_colon[v_idx + 2..];
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return id;
    }
    &before_colon[..v_idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_region_and_vendor_prefix_and_version_suffix() {
        assert_eq!(
            normalize_bedrock_model_id("us.anthropic.claude-opus-4-1-20250805-v1:0"),
            "claude-opus-4-1-20250805"
        );
        assert_eq!(
            normalize_bedrock_model_id("global.anthropic.claude-fable-5-1"),
            "claude-fable-5-1"
        );
        assert_eq!(
            normalize_bedrock_model_id("anthropic.claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn leaves_a_bare_first_party_id_unchanged() {
        assert_eq!(
            normalize_bedrock_model_id("claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn leaves_an_unversioned_id_unchanged_after_stripping_vendor_prefix() {
        assert_eq!(
            normalize_bedrock_model_id("us.anthropic.claude-opus-5-5"),
            "claude-opus-5-5"
        );
    }

    #[test]
    fn does_not_strip_a_colon_that_is_not_a_version_suffix() {
        // No `-v<digits>` immediately before the colon segment: left alone.
        assert_eq!(
            normalize_bedrock_model_id("anthropic.claude-weird:latest"),
            "claude-weird:latest"
        );
    }

    #[test]
    fn only_claude_and_nova_accept_cache_points() {
        assert!(accepts_cache_points("global.anthropic.claude-sonnet-5"));
        assert!(accepts_cache_points(
            "anthropic.claude-haiku-4-5-20251001-v1:0"
        ));
        assert!(accepts_cache_points("global.amazon.nova-2-lite-v1:0"));
        assert!(!accepts_cache_points("global.openai.gpt-6-luna"));
        assert!(accepts_reasoning_history(
            "global.anthropic.claude-sonnet-5"
        ));
        assert!(!accepts_reasoning_history("global.openai.gpt-6-luna"));
        assert!(!accepts_cache_points("global.moonshotai.kimi-k3"));
        assert!(!accepts_cache_points("deepseek.v3.2"));
    }
}

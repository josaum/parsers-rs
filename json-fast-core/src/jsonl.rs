//! JSONL / NDJSON — newline-delimited JSON streaming.
//!
//! Each line holds one standalone JSON value. JSONL is the gold standard for
//! log and big-data pipelines: it can be consumed line by line without loading
//! the whole document. Every helper here is streaming-safe and never allocates
//! a full value list unless asked to.

use crate::{JsonError, JsonLimits, JsonResource, Result};

/// Count non-blank lines in a JSONL document (a cheap streaming pass).
pub fn count_lines(jsonl: &str) -> usize {
    jsonl.lines().filter(|line| !line.trim().is_empty()).count()
}

/// Parse every non-blank line of a JSONL document into typed values.
///
/// Blank lines are skipped; malformed lines fail loud with the offending line
/// number so no partial result is silently accepted.
pub fn parse_many<T: serde::de::DeserializeOwned>(jsonl: &str) -> Result<Vec<T>> {
    parse_many_with_limits(jsonl, &JsonLimits::default())
}

/// Parse every non-blank JSONL record under explicit aggregate limits.
///
/// Input, physical-line, record, token, and retained-value limits are checked
/// across the whole borrowed input before any value is parsed or retained.
pub fn parse_many_with_limits<T: serde::de::DeserializeOwned>(
    jsonl: &str,
    limits: &JsonLimits,
) -> Result<Vec<T>> {
    preflight_jsonl(jsonl, limits)?;

    jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            crate::parse_preflighted::<T>(line.trim())
                .map_err(|error| JsonError::Parse(format!("line {}: {error}", idx + 1)))
        })
        .collect()
}

fn preflight_jsonl(jsonl: &str, limits: &JsonLimits) -> Result<()> {
    crate::check_limit(
        JsonResource::InputBytes,
        jsonl.len(),
        limits.max_input_bytes,
    )?;
    crate::check_limit(JsonResource::Lines, jsonl.lines().count(), limits.max_lines)?;

    let mut records = 0usize;
    let mut tokens = 0usize;
    let mut retained_bytes = 0usize;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        records = records.saturating_add(1);
        crate::check_limit(JsonResource::Records, records, limits.max_records)?;
        let usage = crate::scan_json(line, limits)?;
        tokens = tokens.saturating_add(usage.tokens);
        crate::check_limit(JsonResource::Tokens, tokens, limits.max_tokens)?;
        retained_bytes = retained_bytes.saturating_add(usage.retained_bytes);
        crate::check_limit(
            JsonResource::RetainedBytes,
            retained_bytes,
            limits.max_retained_bytes,
        )?;
    }
    Ok(())
}

/// Lazily iterate parsed JSON values in a JSONL document without building a
/// `Vec`. Secure default aggregate limits are preflighted before the first
/// record can be yielded. Line numbers are reported on parse failure via
/// [`JsonError::Parse`].
pub fn iter<T: serde::de::DeserializeOwned>(jsonl: &str) -> impl Iterator<Item = Result<T>> + '_ {
    iter_with_limits(jsonl, &JsonLimits::default())
}

/// Lazily iterate parsed JSONL values under explicit aggregate limits.
///
/// The whole borrowed input is scanned for input-byte, physical-line, record,
/// token, nesting, and conservative retained-value limits before the first
/// value is yielded. A failed preflight yields its single typed
/// [`JsonError::ResourceLimit`] and then remains exhausted. Deserialization is
/// still lazy after a successful preflight.
pub fn iter_with_limits<'a, T: serde::de::DeserializeOwned>(
    jsonl: &'a str,
    limits: &JsonLimits,
) -> impl Iterator<Item = Result<T>> + 'a {
    let mut preflight_error = preflight_jsonl(jsonl, limits).err();
    let rejected = preflight_error.is_some();
    let mut lines = jsonl.lines().enumerate();

    std::iter::from_fn(move || {
        if rejected {
            return preflight_error.take().map(Err);
        }

        for (idx, line) in lines.by_ref() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            return Some(
                crate::parse_preflighted::<T>(line)
                    .map_err(|error| JsonError::Parse(format!("line {}: {error}", idx + 1))),
            );
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn counts_and_parses_lines() {
        let doc = "{\"a\":1}\n\n{\"a\":2}\n{\"a\":3}";
        assert_eq!(count_lines(doc), 3);
        let vals: Vec<Value> = parse_many(doc).unwrap();
        assert_eq!(vals.len(), 3);
        assert_eq!(vals[1]["a"], 2);
    }

    #[test]
    fn fails_loud_on_bad_line() {
        let doc = "{\"a\":1}\n{not json}\n";
        assert!(parse_many::<Value>(doc).is_err());
    }

    #[test]
    fn iter_is_lazy_and_reports_line_numbers() {
        let doc = "{\"a\":1}\n{broken}\n{\"a\":3}\n";
        let mut it = iter::<Value>(doc);
        assert_eq!(it.next().unwrap().unwrap()["a"], 1);
        let error = it.next().unwrap().unwrap_err();
        assert!(matches!(
            error,
            JsonError::Parse(message) if message.contains("line 2:")
        ));
        assert_eq!(it.next().unwrap().unwrap()["a"], 3);
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_rejects_aggregate_input_before_yielding_and_then_fuses() {
        let doc = "{\"a\":1}\n{\"a\":2}\n";
        let limits = crate::JsonLimits {
            max_input_bytes: doc.len() - 1,
            ..Default::default()
        };
        let mut values = iter_with_limits::<Value>(doc, &limits);

        let error = values
            .next()
            .expect("preflight must emit its typed error")
            .expect_err("no record may precede an aggregate preflight failure");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: JsonResource::InputBytes,
                limit,
                observed,
            } if limit == doc.len() - 1 && observed == doc.len()
        ));
        assert!(values.next().is_none(), "a rejected iterator must be fused");
    }

    #[test]
    fn iter_rejects_aggregate_record_token_and_retained_budgets_before_yielding() {
        let doc = "{\"a\":1}\n{\"a\":2}\n";
        let cases = [
            (
                crate::JsonLimits {
                    max_records: 1,
                    ..Default::default()
                },
                JsonResource::Records,
            ),
            (
                crate::JsonLimits {
                    max_tokens: 3,
                    ..Default::default()
                },
                JsonResource::Tokens,
            ),
            (
                crate::JsonLimits {
                    max_retained_bytes: 199,
                    ..Default::default()
                },
                JsonResource::RetainedBytes,
            ),
        ];

        for (limits, expected_resource) in cases {
            let mut values = iter_with_limits::<Value>(doc, &limits);
            let error = values
                .next()
                .expect("preflight must emit its typed error")
                .expect_err("no record may precede an aggregate preflight failure");
            assert!(matches!(
                error,
                JsonError::ResourceLimit { resource, .. } if resource == expected_resource
            ));
            assert!(values.next().is_none(), "a rejected iterator must be fused");
        }
    }

    #[test]
    fn iter_rejects_physical_line_budget_before_yielding() {
        let doc = "{\"a\":1}\n\n{\"a\":2}\n";
        let limits = crate::JsonLimits {
            max_lines: 2,
            ..Default::default()
        };
        let mut values = iter_with_limits::<Value>(doc, &limits);

        let error = values
            .next()
            .expect("preflight must emit its typed error")
            .expect_err("no record may precede an aggregate preflight failure");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: JsonResource::Lines,
                limit: 2,
                ..
            }
        ));
        assert!(values.next().is_none());
    }

    #[test]
    fn parse_many_enforces_physical_line_budget() {
        let limits = crate::JsonLimits {
            max_lines: 2,
            ..Default::default()
        };
        let error = parse_many_with_limits::<Value>("{\"a\":1}\n\n{\"a\":2}\n", &limits)
            .expect_err("physical lines must be bounded independently of records");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: crate::JsonResource::Lines,
                limit: 2,
                ..
            }
        ));
    }

    #[test]
    fn parse_many_enforces_retained_record_budget_before_push() {
        let limits = crate::JsonLimits {
            max_records: 1,
            ..Default::default()
        };
        let error = parse_many_with_limits::<Value>("{\"a\":1}\n{\"a\":2}\n", &limits)
            .expect_err("record count must be bounded before retaining the next value");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: crate::JsonResource::Records,
                limit: 1,
                ..
            }
        ));
    }
}

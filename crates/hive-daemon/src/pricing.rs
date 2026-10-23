//! Optional models.dev prices are snapshots, never a source of token counts.

use std::{collections::HashMap, sync::OnceLock, time::Duration};

use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    now_millis,
    usage::{Estimate, Tokens},
};

const URL: &str = "https://models.dev/api.json";
const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Rate {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    pub retrieved_at: u64,
}

#[derive(Default)]
struct Catalog {
    checked_at: u64,
    rates: HashMap<(String, String), Rate>,
}

fn price(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)?
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn parse(value: &Value, retrieved_at: u64) -> HashMap<(String, String), Rate> {
    let mut rates = HashMap::new();
    if let Some(providers) = value.as_object() {
        for (provider, listing) in providers {
            if let Some(models) = listing.get("models").and_then(Value::as_object) {
                for (model, spec) in models {
                    if let Some(cost) = spec.get("cost") {
                        rates.insert(
                            (provider.clone(), model.clone()),
                            Rate {
                                input: price(cost, "input"),
                                output: price(cost, "output"),
                                cache_read: price(cost, "cache_read"),
                                cache_write: price(cost, "cache_write"),
                                retrieved_at,
                            },
                        );
                    }
                }
            }
        }
    }
    rates
}

async fn download() -> Option<HashMap<(String, String), Rate>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    let mut response = client.get(URL).send().await.ok()?.error_for_status().ok()?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let rates = parse(&value, now_millis());
    (!rates.is_empty()).then_some(rates)
}

pub(crate) async fn lookup(provider: &str, model: &str) -> Option<Rate> {
    static CATALOG: OnceLock<Mutex<Catalog>> = OnceLock::new();
    let mut cache = CATALOG
        .get_or_init(|| Mutex::new(Catalog::default()))
        .lock()
        .await;
    let now = now_millis();
    // Never quote a stale price after a network failure; throttle retries independently.
    if now.saturating_sub(cache.checked_at)
        >= if cache.rates.is_empty() {
            300_000
        } else {
            86_400_000
        }
    {
        cache.checked_at = now;
        if let Some(rates) = download().await {
            cache.rates = rates;
        }
    }
    cache
        .rates
        .get(&(provider.to_owned(), model.to_owned()))
        .filter(|rate| now.saturating_sub(rate.retrieved_at) < 86_400_000)
        .cloned()
}

impl Rate {
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        tokens: &Tokens,
        cached_input_included: bool,
    ) -> Estimate {
        let cached_read = tokens.cached_read_tokens.unwrap_or(0);
        let cached_write = tokens.cached_write_tokens.unwrap_or(0);
        // ACP doesn't specify whether cache tokens are included in inputTokens. Avoid
        // double-charging until the agent profile explicitly declares its convention.
        let plain_input = tokens.input_tokens.and_then(|input| {
            if cached_read == 0 && cached_write == 0 {
                Some(input)
            } else if cached_input_included {
                input.checked_sub(cached_read)?.checked_sub(cached_write)
            } else {
                None
            }
        });
        let input = plain_input
            .zip(self.input)
            .map(|(count, rate)| count as f64 * rate / 1_000_000.0);
        let output = tokens
            .output_tokens
            .zip(self.output)
            .map(|(count, rate)| count as f64 * rate / 1_000_000.0);
        let cache_read = tokens
            .cached_read_tokens
            .zip(self.cache_read)
            .map(|(count, rate)| count as f64 * rate / 1_000_000.0);
        let cache_write = tokens
            .cached_write_tokens
            .zip(self.cache_write)
            .map(|(count, rate)| count as f64 * rate / 1_000_000.0);
        let total = input.zip(output).and_then(|(input, output)| {
            if tokens.cached_read_tokens.unwrap_or(0) > 0 && cache_read.is_none() {
                return None;
            }
            if tokens.cached_write_tokens.unwrap_or(0) > 0 && cache_write.is_none() {
                return None;
            }
            Some(input + output + cache_read.unwrap_or(0.0) + cache_write.unwrap_or(0.0))
        });
        Estimate {
            input,
            output,
            cache_read,
            cache_write,
            total,
            currency: "USD".into(),
            provider: provider.into(),
            model: model.into(),
            retrieved_at: self.retrieved_at,
            source: URL.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_apply_once_to_cached_input_and_keep_the_catalog_timestamp() {
        let rates = parse(
            &serde_json::json!({"anthropic":{"models":{"example":{"cost":{
                "input": 2.0, "output": 8.0, "cache_read": 0.2, "cache_write": 2.5
            }}}}}),
            123,
        );
        let rate = &rates[&("anthropic".into(), "example".into())];
        let tokens = Tokens {
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cached_read_tokens: Some(200),
            cached_write_tokens: Some(100),
            ..Tokens::default()
        };
        let priced = rate.estimate("anthropic", "example", &tokens, true);
        assert_eq!(priced.input, Some(0.0014));
        assert_eq!(priced.cache_read, Some(0.00004));
        assert!((priced.total.unwrap() - 0.00569).abs() < 1e-10);
        assert_eq!(priced.retrieved_at, 123);
        assert_eq!(
            rate.estimate("anthropic", "example", &tokens, false).total,
            None
        );
    }
}

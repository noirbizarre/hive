//! Optional models.dev prices are snapshots, never a source of token counts.

use std::{collections::HashMap, future::Future, sync::OnceLock, time::Duration};

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

async fn download(client: &reqwest::Client, url: &str) -> Option<HashMap<(String, String), Rate>> {
    let mut response = client.get(url).send().await.ok()?.error_for_status().ok()?;
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

impl Catalog {
    async fn lookup_with(
        &mut self,
        provider: &str,
        model: &str,
        now: u64,
        refresh: impl Future<Output = Option<HashMap<(String, String), Rate>>>,
    ) -> Option<Rate> {
        // Never quote a stale price after a network failure; throttle retries independently.
        if now.saturating_sub(self.checked_at)
            >= if self.rates.is_empty() {
                300_000
            } else {
                86_400_000
            }
        {
            self.checked_at = now;
            if let Some(rates) = refresh.await {
                self.rates = rates;
            }
        }
        self.rates
            .get(&(provider.to_owned(), model.to_owned()))
            .filter(|rate| now.saturating_sub(rate.retrieved_at) < 86_400_000)
            .cloned()
    }
}

pub(crate) async fn lookup(provider: &str, model: &str) -> Option<Rate> {
    static CATALOG: OnceLock<Mutex<Catalog>> = OnceLock::new();
    let refresh = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .ok()?;
        download(&client, URL).await
    };
    CATALOG
        .get_or_init(|| Mutex::new(Catalog::default()))
        .lock()
        .await
        .lookup_with(provider, model, now_millis(), refresh)
        .await
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
    use axum::{
        Router,
        body::Body,
        http::{Response, StatusCode, header},
        routing::get,
    };

    async fn catalog_response(
        status: StatusCode,
        body: &str,
        length: usize,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api.json", listener.local_addr().unwrap());
        let body = body.to_owned();
        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/api.json",
                get(move || {
                    let body = body.clone();
                    async move {
                        Response::builder()
                            .status(status)
                            .header(header::CONTENT_LENGTH, length.to_string())
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        (url, server)
    }

    #[tokio::test]
    async fn a_valid_catalog_is_downloaded_but_errors_and_oversized_responses_are_ignored() {
        let fixture = r#"{"anthropic":{"models":{"example":{"cost":{"input":2,"output":8}}}}}"#;
        // CI runners may install a system proxy; these fixtures are always loopback.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap();
        let (url, server) = catalog_response(StatusCode::OK, fixture, fixture.len()).await;
        let prices = download(&client, &url).await.unwrap();
        assert_eq!(
            prices[&("anthropic".into(), "example".into())].output,
            Some(8.0)
        );
        server.abort();

        for (status, body, length) in [
            (StatusCode::SERVICE_UNAVAILABLE, "", 0),
            (StatusCode::OK, "", MAX_BYTES + 1),
            (StatusCode::OK, "not json", 8),
            (StatusCode::OK, "{}", 2),
        ] {
            let (url, server) = catalog_response(status, body, length).await;
            assert!(download(&client, &url).await.is_none());
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_failed_refresh_never_returns_stale_or_unmatched_prices() {
        let mut catalog = Catalog::default();
        let prices = parse(
            &serde_json::json!({"anthropic": {"models": {"example": {"cost": {"input": 2}}}}}),
            300_000,
        );
        let first = catalog
            .lookup_with(
                "anthropic",
                "example",
                300_000,
                std::future::ready(Some(prices)),
            )
            .await
            .unwrap();
        assert_eq!(first.input, Some(2.0));
        assert!(
            catalog
                .lookup_with("anthropic", "missing", 300_001, async {
                    panic!("a warm cache must not fetch again")
                })
                .await
                .is_none()
        );
        assert!(
            catalog
                .lookup_with("anthropic", "example", 86_700_000, std::future::ready(None))
                .await
                .is_none()
        );
        assert_eq!(catalog.checked_at, 86_700_000);
        let refreshed_at = 173_100_000;
        let prices = parse(
            &serde_json::json!({"anthropic": {"models": {"new": {"cost": {"output": 4}}}}}),
            refreshed_at,
        );
        assert!(
            catalog
                .lookup_with(
                    "anthropic",
                    "example",
                    refreshed_at,
                    std::future::ready(Some(prices))
                )
                .await
                .is_none()
        );
        assert!(
            catalog
                .lookup_with(
                    "anthropic",
                    "new",
                    refreshed_at + 1,
                    std::future::ready(None)
                )
                .await
                .is_some()
        );
    }

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

    #[test]
    fn missing_cache_rates_and_inconsistent_counts_cannot_produce_a_total() {
        let rates = parse(
            &serde_json::json!({"provider":{"models":{"model":{"cost":{"input":1,"output":3}}}}}),
            42,
        );
        let rate = &rates[&("provider".into(), "model".into())];
        let tokens = Tokens {
            input_tokens: Some(10),
            output_tokens: Some(5),
            cached_read_tokens: Some(8),
            cached_write_tokens: Some(4),
            ..Tokens::default()
        };
        assert_eq!(
            rate.estimate("provider", "model", &tokens, true).total,
            None
        );
        let tokens = Tokens {
            cached_write_tokens: Some(0),
            ..tokens
        };
        let estimate = rate.estimate("provider", "model", &tokens, true);
        assert_eq!(estimate.input, Some(0.000002));
        assert_eq!(estimate.output, Some(0.000015));
        assert!(estimate.total.is_none());
        let ordinary = Tokens {
            cached_read_tokens: None,
            ..tokens
        };
        assert!(
            (rate
                .estimate("provider", "model", &ordinary, false)
                .total
                .unwrap()
                - 0.000025)
                .abs()
                < 1e-10
        );
    }
}

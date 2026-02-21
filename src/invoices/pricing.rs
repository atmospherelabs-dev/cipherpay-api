use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ZecRates {
    pub zec_eur: f64,
    pub zec_usd: f64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct PriceService {
    api_url: String,
    cache_secs: u64,
    cached: Arc<RwLock<Option<ZecRates>>>,
    http: reqwest::Client,
}

impl PriceService {
    pub fn new(api_url: &str, cache_secs: u64) -> Self {
        Self {
            api_url: api_url.to_string(),
            cache_secs,
            cached: Arc::new(RwLock::new(None)),
            http: reqwest::Client::new(),
        }
    }

    pub async fn get_rates(&self) -> anyhow::Result<ZecRates> {
        // Check cache
        {
            let cache = self.cached.read().await;
            if let Some(rates) = &*cache {
                let age = (Utc::now() - rates.updated_at).num_seconds() as u64;
                if age < self.cache_secs {
                    return Ok(rates.clone());
                }
            }
        }

        // Try to fetch from CoinGecko
        match self.fetch_live_rates().await {
            Ok(rates) => {
                let mut cache = self.cached.write().await;
                *cache = Some(rates.clone());
                tracing::debug!(zec_eur = rates.zec_eur, zec_usd = rates.zec_usd, "Price feed updated");
                Ok(rates)
            }
            Err(e) => {
                tracing::warn!(error = %e, "CoinGecko unavailable, using fallback rate");
                Ok(ZecRates {
                    zec_eur: 220.0,
                    zec_usd: 240.0,
                    updated_at: Utc::now(),
                })
            }
        }
    }

    async fn fetch_live_rates(&self) -> anyhow::Result<ZecRates> {
        let url = format!(
            "{}/simple/price?ids=zcash&vs_currencies=eur,usd",
            self.api_url
        );

        let resp: serde_json::Value = self.http
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?
            .json()
            .await?;

        let zec_eur = resp["zcash"]["eur"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("Missing ZEC/EUR rate"))?;
        let zec_usd = resp["zcash"]["usd"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("Missing ZEC/USD rate"))?;

        Ok(ZecRates {
            zec_eur,
            zec_usd,
            updated_at: Utc::now(),
        })
    }
}

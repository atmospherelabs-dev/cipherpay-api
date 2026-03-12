use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ZecRates {
    pub zec_eur: f64,
    pub zec_usd: f64,
    pub zec_brl: f64,
    pub zec_gbp: f64,
    pub zec_cad: f64,
    pub zec_jpy: f64,
    pub zec_mxn: f64,
    pub zec_ars: f64,
    pub zec_ngn: f64,
    pub zec_chf: f64,
    pub zec_inr: f64,
    pub updated_at: DateTime<Utc>,
}

impl ZecRates {
    pub fn rate_for_currency(&self, currency: &str) -> Option<f64> {
        match currency {
            "EUR" => Some(self.zec_eur),
            "USD" => Some(self.zec_usd),
            "BRL" => Some(self.zec_brl),
            "GBP" => Some(self.zec_gbp),
            "CAD" => Some(self.zec_cad),
            "JPY" => Some(self.zec_jpy),
            "MXN" => Some(self.zec_mxn),
            "ARS" => Some(self.zec_ars),
            "NGN" => Some(self.zec_ngn),
            "CHF" => Some(self.zec_chf),
            "INR" => Some(self.zec_inr),
            _ => None,
        }
    }
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
        let http = reqwest::Client::builder()
            .user_agent("CipherPay/1.0")
            .build()
            .expect("Failed to build HTTP client");
        Self {
            api_url: api_url.to_string(),
            cache_secs,
            cached: Arc::new(RwLock::new(None)),
            http,
        }
    }

    pub async fn get_rates(&self) -> anyhow::Result<ZecRates> {
        {
            let cache = self.cached.read().await;
            if let Some(rates) = &*cache {
                let age = (Utc::now() - rates.updated_at).num_seconds() as u64;
                if age < self.cache_secs {
                    return Ok(rates.clone());
                }
            }
        }

        match self.fetch_live_rates().await {
            Ok(rates) => {
                let mut cache = self.cached.write().await;
                *cache = Some(rates.clone());
                tracing::info!(zec_eur = rates.zec_eur, zec_usd = rates.zec_usd, zec_brl = rates.zec_brl, zec_gbp = rates.zec_gbp, "Price feed updated");
                Ok(rates)
            }
            Err(e) => {
                let cache = self.cached.read().await;
                if let Some(stale) = &*cache {
                    tracing::warn!(error = %e, age_secs = (Utc::now() - stale.updated_at).num_seconds(), "CoinGecko unavailable, using last known rate");
                    return Ok(stale.clone());
                }
                tracing::error!(error = %e, "CoinGecko unavailable and no cached rate — prices will be inaccurate");
                anyhow::bail!("No price data available: {}", e)
            }
        }
    }

    async fn fetch_live_rates(&self) -> anyhow::Result<ZecRates> {
        let url = format!(
            "{}/simple/price?ids=zcash&vs_currencies=eur,usd,brl,gbp,cad,jpy,mxn,ars,ngn,chf,inr",
            self.api_url
        );

        let response = self.http
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("CoinGecko returned HTTP {}: {}", status, &body[..body.len().min(200)]);
        }

        let resp: serde_json::Value = response.json().await?;
        let zec = &resp["zcash"];

        let zec_eur = zec["eur"].as_f64()
            .ok_or_else(|| anyhow::anyhow!("Missing ZEC/EUR rate in response: {}", resp))?;
        let zec_usd = zec["usd"].as_f64()
            .ok_or_else(|| anyhow::anyhow!("Missing ZEC/USD rate in response: {}", resp))?;

        Ok(ZecRates {
            zec_eur,
            zec_usd,
            zec_brl: zec["brl"].as_f64().unwrap_or(0.0),
            zec_gbp: zec["gbp"].as_f64().unwrap_or(0.0),
            zec_cad: zec["cad"].as_f64().unwrap_or(0.0),
            zec_jpy: zec["jpy"].as_f64().unwrap_or(0.0),
            zec_mxn: zec["mxn"].as_f64().unwrap_or(0.0),
            zec_ars: zec["ars"].as_f64().unwrap_or(0.0),
            zec_ngn: zec["ngn"].as_f64().unwrap_or(0.0),
            zec_chf: zec["chf"].as_f64().unwrap_or(0.0),
            zec_inr: zec["inr"].as_f64().unwrap_or(0.0),
            updated_at: Utc::now(),
        })
    }
}

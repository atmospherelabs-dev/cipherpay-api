use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub cipherscan_api_url: String,
    pub network: String,
    pub api_host: String,
    pub api_port: u16,
    pub mempool_poll_interval_secs: u64,
    pub block_poll_interval_secs: u64,
    #[allow(dead_code)]
    pub encryption_key: String,
    pub invoice_expiry_minutes: i64,
    #[allow(dead_code)]
    pub data_purge_days: i64,
    pub coingecko_api_url: String,
    pub price_cache_secs: u64,
    pub allowed_origins: Vec<String>,
    pub cookie_domain: Option<String>,
    pub frontend_url: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:cipherpay.db".into()),
            cipherscan_api_url: env::var("CIPHERSCAN_API_URL")
                .unwrap_or_else(|_| "https://api.testnet.cipherscan.app".into()),
            network: env::var("NETWORK").unwrap_or_else(|_| "testnet".into()),
            api_host: env::var("API_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            api_port: env::var("API_PORT")
                .unwrap_or_else(|_| "3080".into())
                .parse()?,
            mempool_poll_interval_secs: env::var("MEMPOOL_POLL_INTERVAL_SECS")
                .unwrap_or_else(|_| "5".into())
                .parse()?,
            block_poll_interval_secs: env::var("BLOCK_POLL_INTERVAL_SECS")
                .unwrap_or_else(|_| "15".into())
                .parse()?,
            encryption_key: env::var("ENCRYPTION_KEY").unwrap_or_default(),
            invoice_expiry_minutes: env::var("INVOICE_EXPIRY_MINUTES")
                .unwrap_or_else(|_| "30".into())
                .parse()?,
            data_purge_days: env::var("DATA_PURGE_DAYS")
                .unwrap_or_else(|_| "30".into())
                .parse()?,
            coingecko_api_url: env::var("COINGECKO_API_URL")
                .unwrap_or_else(|_| "https://api.coingecko.com/api/v3".into()),
            price_cache_secs: env::var("PRICE_CACHE_SECS")
                .unwrap_or_else(|_| "300".into())
                .parse()?,
            allowed_origins: env::var("ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            cookie_domain: env::var("COOKIE_DOMAIN").ok().filter(|s| !s.is_empty()),
            frontend_url: env::var("FRONTEND_URL").ok().filter(|s| !s.is_empty()),
        })
    }

    pub fn is_testnet(&self) -> bool {
        self.network == "testnet"
    }
}

mod addresses;
mod api;
mod billing;
mod config;
mod crypto;
mod db;
mod email;
mod events;
mod invoices;
mod luma;
mod merchants;
mod prices;
mod products;
mod scanner;
mod subscriptions;
mod tickets;
mod validation;
mod webhooks;

use actix_cors::Cors;
use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{web, App, HttpServer, middleware};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cipherpay=info".into()),
        )
        .init();

    let config = config::Config::from_env()?;
    let pool = db::create_pool(&config.database_url).await?;
    db::migrate_encrypt_ufvks(&pool, &config.encryption_key).await?;
    db::migrate_ufvk_to_uivk(&pool, &config.encryption_key).await?;
    db::migrate_encrypt_webhook_secrets(&pool, &config.encryption_key).await?;
    db::migrate_encrypt_recovery_emails(&pool, &config.encryption_key).await?;
    let mut default_headers = reqwest::header::HeaderMap::new();
    default_headers.insert("User-Agent", reqwest::header::HeaderValue::from_static("CipherPay/1.0"));
    if let Ok(key) = std::env::var("CIPHERSCAN_SERVICE_KEY") {
        if !key.is_empty() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&key) {
                default_headers.insert("X-Service-Key", val);
            }
        }
    }
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .default_headers(default_headers)
        .build()?;

    let price_service = invoices::pricing::PriceService::new(
        &config.coingecko_api_url,
        config.price_cache_secs,
    );

    tracing::info!(
        network = %config.network,
        api = %format!("{}:{}", config.api_host, config.api_port),
        cipherscan = %config.cipherscan_api_url,
        "CipherPay starting"
    );

    let scanner_config = config.clone();
    let scanner_pool = pool.clone();
    let scanner_http = http_client.clone();
    tokio::spawn(async move {
        scanner::run(scanner_config, scanner_pool, scanner_http).await;
    });

    let retry_pool = pool.clone();
    let retry_http = http_client.clone();
    let retry_enc_key = config.encryption_key.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let _ = webhooks::retry_failed(&retry_pool, &retry_http, &retry_enc_key).await;
        }
    });

    let purge_pool = pool.clone();
    let purge_days = config.data_purge_days;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = db::run_data_purge(&purge_pool, purge_days).await {
                tracing::error!(error = %e, "Data purge error");
            }
        }
    });

    let events_pool = pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match events::mark_past_events(&events_pool).await {
                Ok(count) if count > 0 => tracing::info!(count, "Marked active events as past"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "Event status sweep error"),
            }
        }
    });

    if config.fee_enabled() {
        let billing_pool = pool.clone();
        let billing_config = config.clone();
        let billing_prices = price_service.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            tracing::info!(
                fee_rate = billing_config.fee_rate,
                fee_address = ?billing_config.fee_address,
                "Billing system enabled"
            );
            loop {
                interval.tick().await;
                let (zec_eur, zec_usd) = match billing_prices.get_rates().await {
                    Ok(r) => (r.zec_eur, r.zec_usd),
                    Err(_) => (0.0, 0.0),
                };
                if let Err(e) = billing::process_billing_cycles(&billing_pool, &billing_config, zec_eur, zec_usd).await {
                    tracing::error!(error = %e, "Billing cycle processing error");
                }
            }
        });
    }

    // Subscription lifecycle engine (hourly)
    let sub_pool = pool.clone();
    let sub_http = http_client.clone();
    let sub_config = config.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            // Build merchant UFVK map for draft invoice address derivation
            let merchants = match crate::merchants::get_all_merchants(&sub_pool, &sub_config.encryption_key).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "Subscription engine: failed to load merchants");
                    continue;
                }
            };
            let ufvk_map: std::collections::HashMap<String, String> = merchants
                .into_iter()
                .map(|m| (m.id, m.ufvk))
                .collect();

            let fee_config = if sub_config.fee_enabled() {
                sub_config.fee_address.as_ref().map(|addr| crate::invoices::FeeConfig {
                    fee_address: addr.clone(),
                    fee_rate: sub_config.fee_rate,
                })
            } else {
                None
            };

            if let Err(e) = subscriptions::process_renewals(
                &sub_pool, &sub_http, &sub_config.encryption_key, &ufvk_map, fee_config.as_ref(),
            ).await {
                tracing::error!(error = %e, "Subscription renewal error");
            }
        }
    });

    let bind_addr = format!("{}:{}", config.api_host, config.api_port);

    let rate_limit = GovernorConfigBuilder::default()
        .seconds_per_request(1)
        .burst_size(60)
        .finish()
        .expect("Failed to build rate limiter");

    HttpServer::new(move || {
        let cors = if config.is_testnet() || config.allowed_origins.is_empty() {
            Cors::default()
                .allowed_origin_fn(|_origin, _req_head| true)
                .allow_any_method()
                .allow_any_header()
                .supports_credentials()
                .max_age(3600)
        } else {
            let mut cors = Cors::default()
                .allow_any_method()
                .allow_any_header()
                .supports_credentials()
                .max_age(3600);
            for origin in &config.allowed_origins {
                cors = cors.allowed_origin(origin);
            }
            cors
        };

        App::new()
            .wrap(cors)
            .wrap(Governor::new(&rate_limit))
            .wrap(middleware::DefaultHeaders::new()
                .add(("X-Content-Type-Options", "nosniff"))
                .add(("X-Frame-Options", "DENY"))
                .add(("Referrer-Policy", "strict-origin-when-cross-origin"))
                .add(("Strict-Transport-Security", "max-age=63072000; includeSubDomains; preload"))
                .add(("Permissions-Policy", "camera=(), microphone=(), geolocation=()"))
            )
            .app_data(web::JsonConfig::default().limit(65_536))
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(price_service.clone()))
            .app_data(web::Data::new(http_client.clone()))
            .configure(api::configure)
            .route("/", web::get().to(serve_ui))
            .service(web::resource("/widget/{filename}")
                .route(web::get().to(serve_widget)))
    })
    .bind(&bind_addr)?
    .run()
    .await?;

    Ok(())
}

async fn serve_ui() -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok()
        .content_type("text/html")
        .body(include_str!("../ui/index.html"))
}

async fn serve_widget(path: web::Path<String>) -> actix_web::HttpResponse {
    let filename = path.into_inner();
    let (content, content_type) = match filename.as_str() {
        "cipherpay.js" => (include_str!("../widget/cipherpay.js"), "application/javascript"),
        "cipherpay.css" => (include_str!("../widget/cipherpay.css"), "text/css"),
        _ => return actix_web::HttpResponse::NotFound().finish(),
    };

    actix_web::HttpResponse::Ok()
        .content_type(content_type)
        .body(content)
}

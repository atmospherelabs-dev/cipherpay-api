mod api;
mod config;
mod db;
mod invoices;
mod merchants;
mod scanner;
mod webhooks;

use actix_cors::Cors;
use actix_web::{web, App, HttpServer};

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
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let price_service = invoices::pricing::PriceService::new(
        &config.coingecko_api_url,
        config.price_cache_secs,
    );

    tracing::info!(
        network = %config.network,
        api = %format!("{}:{}", config.api_host, config.api_port),
        cipherscan = %config.cipherscan_api_url,
        db = %config.database_url,
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
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let _ = webhooks::retry_failed(&retry_pool, &retry_http).await;
        }
    });

    let bind_addr = format!("{}:{}", config.api_host, config.api_port);

    HttpServer::new(move || {
        let cors = if config.is_testnet() || config.allowed_origins.is_empty() {
            Cors::default()
                .allow_any_origin()
                .allow_any_method()
                .allow_any_header()
                .max_age(3600)
        } else {
            let mut cors = Cors::default()
                .allow_any_method()
                .allow_any_header()
                .max_age(3600);
            for origin in &config.allowed_origins {
                cors = cors.allowed_origin(origin);
            }
            cors
        };

        App::new()
            .wrap(cors)
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(price_service.clone()))
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

use actix_web::{web, HttpResponse};

pub async fn health() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "cipherpay",
    }))
}

pub async fn well_known_payment(config: web::Data<crate::config::Config>) -> HttpResponse {
    let network = if config.is_testnet() {
        "zcash:testnet"
    } else {
        "zcash:mainnet"
    };
    HttpResponse::Ok()
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .json(serde_json::json!({
            "version": "1.0",
            "methods": ["zcash"],
            "currencies": ["ZEC"],
            "network": network,
            "protocols": ["x402", "mpp"],
            "capabilities": {
                "sessions": true,
                "streaming": true,
                "replay_protection": true,
            },
            "facilitator": "https://api.cipherpay.app",
            "documentation": "https://cipherpay.app/docs",
        }))
}

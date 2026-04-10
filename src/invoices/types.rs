use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Invoice {
    pub id: String,
    pub merchant_id: String,
    pub memo_code: String,
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    pub price_eur: f64,
    pub price_usd: Option<f64>,
    pub currency: Option<String>,
    pub price_zec: f64,
    pub zec_rate_at_creation: f64,
    pub amount: Option<f64>,
    pub price_id: Option<String>,
    pub payment_address: String,
    pub zcash_uri: String,
    pub merchant_name: Option<String>,
    pub refund_address: Option<String>,
    pub status: String,
    pub detected_txid: Option<String>,
    pub detected_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub refunded_at: Option<String>,
    pub refund_txid: Option<String>,
    pub expires_at: String,
    pub purge_after: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing)]
    pub orchard_receiver_hex: Option<String>,
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    pub diversifier_index: Option<i64>,
    pub price_zatoshis: i64,
    pub received_zatoshis: i64,
    pub subscription_id: Option<String>,
    pub payment_link_id: Option<String>,
    pub is_donation: i32,
    pub campaign_counted: i32,
}

#[derive(Debug, Serialize, FromRow)]
pub struct InvoiceStatus {
    #[sqlx(rename = "id")]
    pub invoice_id: String,
    pub status: String,
    pub detected_txid: Option<String>,
    pub received_zatoshis: i64,
    pub price_zatoshis: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub product_id: Option<String>,
    pub price_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    #[serde(alias = "price_eur")]
    pub amount: f64,
    pub currency: Option<String>,
    pub refund_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateInvoiceResponse {
    pub invoice_id: String,
    pub memo_code: String,
    pub amount: f64,
    pub currency: String,
    pub price_eur: f64,
    pub price_usd: f64,
    pub price_zec: f64,
    pub zec_rate: f64,
    pub price_id: Option<String>,
    pub payment_address: String,
    pub zcash_uri: String,
    pub expires_at: String,
}

pub struct FeeConfig {
    pub fee_address: String,
    pub fee_rate: f64,
}

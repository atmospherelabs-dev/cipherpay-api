use serde::Serialize;

pub const MIN_SETTLEMENT_ZATOSHIS: i64 = 5_000_000;

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct FeeEntry {
    pub id: String,
    pub invoice_id: String,
    pub merchant_id: String,
    pub fee_amount_zec: f64,
    #[serde(skip_serializing)]
    pub fee_amount_zatoshis: i64,
    pub auto_collected: i32,
    pub collected_at: Option<String>,
    pub billing_cycle_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BillingCycle {
    pub id: String,
    pub merchant_id: String,
    pub period_start: String,
    pub period_end: String,
    pub total_fees_zec: f64,
    pub auto_collected_zec: f64,
    pub outstanding_zec: f64,
    #[serde(skip_serializing)]
    pub total_fees_zatoshis: i64,
    #[serde(skip_serializing)]
    pub auto_collected_zatoshis: i64,
    #[serde(skip_serializing)]
    pub outstanding_zatoshis: i64,
    pub settlement_invoice_id: Option<String>,
    pub status: String,
    pub grace_until: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct BillingSummary {
    pub fee_rate: f64,
    pub trust_tier: String,
    pub billing_status: String,
    pub current_cycle: Option<BillingCycle>,
    pub total_fees_zec: f64,
    pub auto_collected_zec: f64,
    pub outstanding_zec: f64,
    pub total_fees_zatoshis: i64,
    pub auto_collected_zatoshis: i64,
    pub outstanding_zatoshis: i64,
    pub settlement_invoice_status: Option<String>,
}

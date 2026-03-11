# CipherPay Backend - Agent Guidelines

> Compiled rules for AI agents working on the CipherPay Rust backend.

## Quick Reference

| Category | Impact | Key Points |
|----------|--------|------------|
| Security | Critical | Encrypt UFVKs, HMAC webhooks, never log secrets |
| Invoices | Critical | Diversified addresses, ZIP-321, expiry handling |
| Billing | Critical | Fee ledger, settlement, minimum threshold (0.05 ZEC) |
| Scanner | High | CipherScan API polling, trial decryption, mempool detection |
| Database | High | SQLite, parameterized queries, migrations in db.rs |

---

## Critical Rules

### 1. Never Log Secrets (CRITICAL)
```rust
// ❌ NEVER
tracing::info!("Merchant UFVK: {}", merchant.ufvk);
tracing::info!("API key: {}", api_key);

// ✅ Log identifiers only
tracing::info!("Merchant {} registered", merchant.id);
```

### 2. Parameterized SQL (CRITICAL)
```rust
// ✅ ALWAYS parameterized
sqlx::query("SELECT * FROM merchants WHERE id = ?")
    .bind(&merchant_id)
    .fetch_optional(&pool).await?;

// ❌ NEVER string interpolation
sqlx::query(&format!("SELECT * FROM merchants WHERE id = '{}'", merchant_id))
```

### 3. Diversified Addresses (CRITICAL)
```rust
// Each invoice gets a unique diversified address from the merchant's UFVK
// Never reuse addresses across invoices
let (address, diversifier_index) = derive_next_address(&ufvk, &last_index)?;
```

### 4. ZIP-321 Dual-Output URIs
```rust
// When fees enabled, payment URI has two outputs:
// Output 0: merchant payment
// Output 1: platform fee
format!(
    "zcash:?address={}&amount={:.8}&memo={}&address.1={}&amount.1={:.8}&memo.1={}",
    merchant_address, price_zec, memo,
    fee_address, fee_amount, fee_memo
)
```

### 5. Webhook HMAC Signing
```rust
// Outbound webhooks include:
// x-cipherpay-signature: HMAC-SHA256(timestamp.body, webhook_secret)
// x-cipherpay-timestamp: ISO 8601 timestamp
// Replay protection: reject webhooks older than 5 minutes
```

---

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `DATABASE_URL` | Yes | SQLite path |
| `CIPHERSCAN_API_URL` | Yes | CipherScan API for blockchain data |
| `NETWORK` | Yes | `testnet` or `mainnet` |
| `ENCRYPTION_KEY` | Yes | 32-byte hex key for UFVK encryption |
| `FEE_ADDRESS` | No | Platform fee collection address |
| `FEE_UFVK` | No | Platform fee viewing key |
| `FEE_RATE` | No | Fee rate (default 0.01 = 1%) |

---

## API Structure

| Endpoint | Auth | Description |
|----------|------|-------------|
| `POST /api/invoices` | API key | Create invoice |
| `GET /api/invoices/{id}` | - | Get invoice status |
| `GET /api/invoices/{id}/stream` | - | SSE real-time status |
| `POST /api/merchants/register` | - | Register with UFVK |
| `POST /api/auth/login` | Dashboard token | Login to dashboard |
| `GET /api/billing/summary` | Session | Billing overview |
| `GET /api/products` | Session | Product catalog |

---

## Billing System

- **Fee rate**: 1% of each payment (configurable via `FEE_RATE`)
- **Collection**: ZIP-321 dual-output — fee collected atomically with payment
- **Billing cycles**: 7 days (new merchants), 30 days (standard)
- **Minimum threshold**: 0.05 ZEC — below this, balance carries over
- **Settlement**: Outstanding fees → settlement invoice → merchant pays
- **Grace period**: 7 days (new), 3 days (standard) before enforcement

---

## Scanner Flow

1. Poll CipherScan API for mempool transactions
2. For each merchant with active invoices, trial-decrypt Orchard outputs using UFVK
3. Match decrypted memo against invoice memo codes
4. On match: update invoice status (`detected` → `confirmed`)
5. Fire webhook to merchant's webhook URL
6. Repeat for mined blocks

---

*Generated from cipherpay skill rules*

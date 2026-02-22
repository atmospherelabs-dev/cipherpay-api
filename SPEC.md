# CipherPay Technical Specification

> Shielded Zcash payment service with mempool detection.

## Overview

CipherPay is a standalone Rust microservice for accepting shielded Zcash payments. It uses CipherScan's existing APIs as its blockchain data source, performs trial decryption with merchant viewing keys, matches payments to invoices via memo fields, and exposes a REST API + embeddable checkout widget.

### Key Differentiators

- **Orchard + Sapling from day one** (BTCPay's Zcash plugin only supports Sapling)
- **Mempool detection** for near-instant payment feedback (~5s vs ~75s+ for competitors)
- **No extra infrastructure** -- uses CipherScan APIs, no Zebra node needed
- **Embeddable widget** -- drop-in JS for any website, not just WooCommerce
- **Privacy-first** -- no buyer PII stored, pure payment processor

---

## Architecture

```
CipherScan (existing)          CipherPay (this service)
┌─────────────────────┐        ┌──────────────────────────┐
│  Zebra Node         │        │  Scanner                 │
│  Lightwalletd       │◄──────►│  ├── Mempool poller (5s) │
│  REST API           │        │  ├── Block poller (15s)  │
│  ├── /api/mempool   │        │  └── Trial decryptor     │
│  ├── /api/tx/:id/raw│        │                          │
│  ├── /api/block/:h  │        │  Invoice Engine          │
│  └── /api/blockchain│        │  ├── Create / match      │
└─────────────────────┘        │  └── Expire / purge      │
                               │                          │
                               │  REST API (actix-web)    │
                               │  ├── POST /api/merchants │
                               │  ├── POST /api/invoices  │
                               │  ├── GET  /api/invoices  │
                               │  ├── GET  /api/status    │
                               │  └── GET  /api/rates     │
                               │                          │
                               │  Webhook Dispatcher      │
                               │  PostgreSQL              │
                               └──────────────────────────┘
```

---

## API Reference

### `GET /api/health`

Health check.

**Response:**
```json
{ "status": "ok", "service": "cipherpay", "version": "0.1.0" }
```

### `POST /api/merchants`

Register a new merchant.

**Request:**
```json
{
  "ufvk": "uview1...",
  "webhook_url": "https://example.com/webhook"
}
```

**Response (201):**
```json
{
  "merchant_id": "uuid",
  "api_key": "cpay_abc123..."
}
```

> Store the `api_key` securely -- it cannot be retrieved again.

### `POST /api/invoices`

Create a payment invoice.

**Request:**
```json
{
  "product_name": "[REDACTED] Tee",
  "size": "L",
  "price_eur": 65.00
}
```

**Response (201):**
```json
{
  "invoice_id": "uuid",
  "memo_code": "CP-A7F3B2C1",
  "price_eur": 65.00,
  "price_zec": 1.835,
  "zec_rate": 35.42,
  "expires_at": "2026-02-21T15:30:00Z"
}
```

### `GET /api/invoices/{id}`

Get full invoice details.

**Response (200):**
```json
{
  "id": "uuid",
  "merchant_id": "uuid",
  "memo_code": "CP-A7F3B2C1",
  "product_name": "[REDACTED] Tee",
  "size": "L",
  "price_eur": 65.00,
  "price_zec": 1.835,
  "status": "pending",
  "detected_txid": null,
  "expires_at": "2026-02-21T15:30:00Z",
  "created_at": "2026-02-21T15:00:00Z"
}
```

### `GET /api/invoices/{id}/status`

Lightweight status endpoint for widget polling.

**Response (200):**
```json
{
  "invoice_id": "uuid",
  "status": "detected",
  "detected_txid": "abc123...",
  "confirmations": null
}
```

Status values: `pending` → `detected` → `confirmed` (or `expired` / `refunded`)

### `GET /api/rates`

Current ZEC exchange rates (cached 5 minutes).

**Response (200):**
```json
{
  "zec_eur": 35.42,
  "zec_usd": 38.10,
  "updated_at": "2026-02-21T15:00:00Z"
}
```

---

## Embeddable Widget

Drop this into any HTML page:

```html
<div id="cipherpay"
     data-invoice-id="your-invoice-uuid"
     data-api="https://pay.cipherscan.app">
</div>
<script src="https://pay.cipherscan.app/widget/cipherpay.js"></script>
```

The widget:
- Displays ZEC amount, EUR equivalent, QR code, and memo code
- Polls for payment status every 5 seconds
- Shows real-time status transitions: Waiting → Detected → Confirmed
- Auto-expires when the invoice timer runs out
- Styled in CipherScan's dark monospace aesthetic

---

## Payment Detection Flow

1. **Invoice created** -- merchant calls `POST /api/invoices`, gets memo code
2. **Buyer sends ZEC** -- includes the memo code in the shielded memo field
3. **Mempool detection (~5s)** -- scanner polls CipherScan's mempool API, fetches raw tx hex for new txids, trial-decrypts with merchant UFVK, matches memo to pending invoice
4. **Status: detected** -- invoice updated, webhook fired, widget shows "Payment detected!"
5. **Block confirmation (~75s)** -- scanner checks if the detected txid is now in a block
6. **Status: confirmed** -- invoice updated, webhook fired, widget shows "Confirmed!"

---

## Database Schema

### merchants
| Column | Type | Description |
|--------|------|-------------|
| id | UUID | Primary key |
| api_key_hash | TEXT | SHA-256 hash of API key |
| ufvk | TEXT | Unified Full Viewing Key |
| webhook_url | TEXT | URL for payment event webhooks |
| created_at | TIMESTAMPTZ | Registration timestamp |

### invoices
| Column | Type | Description |
|--------|------|-------------|
| id | UUID | Primary key |
| merchant_id | UUID | FK to merchants |
| memo_code | TEXT | Unique memo code (e.g. CP-A7F3B2C1) |
| product_name | TEXT | Product description |
| size | TEXT | Product size |
| price_eur | FLOAT | Price in EUR |
| price_zec | FLOAT | Price in ZEC at creation |
| zec_rate_at_creation | FLOAT | ZEC/EUR rate when invoice was created |
| refund_address | TEXT | Buyer's optional Zcash refund address |
| status | TEXT | pending/detected/confirmed/expired/refunded |
| detected_txid | TEXT | Transaction ID when payment detected |
| detected_at | TIMESTAMPTZ | When payment was first seen |
| confirmed_at | TIMESTAMPTZ | When block confirmation received |
| refunded_at | TIMESTAMPTZ | When merchant marked as refunded |
| expires_at | TIMESTAMPTZ | Invoice expiration time |
| created_at | TIMESTAMPTZ | Invoice creation timestamp |

### webhook_deliveries
| Column | Type | Description |
|--------|------|-------------|
| id | UUID | Primary key |
| invoice_id | UUID | FK to invoices |
| url | TEXT | Webhook endpoint URL |
| payload | TEXT | JSON payload sent |
| status | TEXT | pending/delivered/failed |
| attempts | INT | Number of delivery attempts |
| last_attempt_at | TIMESTAMPTZ | Last attempt timestamp |
| next_retry_at | TIMESTAMPTZ | When to retry next |

---

## Deployment

### Quick Start (Development)

```bash
# Clone and setup
git clone https://github.com/your-org/cipherpay
cd cipherpay
cp .env.example .env

# Start PostgreSQL
docker-compose up -d

# Run the service
cargo run
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| DATABASE_URL | postgres://...localhost:5433/cipherpay | PostgreSQL connection |
| CIPHERSCAN_API_URL | https://api.testnet.cipherscan.app | CipherScan API endpoint |
| NETWORK | testnet | testnet or mainnet |
| API_HOST | 127.0.0.1 | Bind address |
| API_PORT | 3080 | Bind port |
| MEMPOOL_POLL_INTERVAL_SECS | 5 | How often to check mempool |
| BLOCK_POLL_INTERVAL_SECS | 15 | How often to check for new blocks |
| INVOICE_EXPIRY_MINUTES | 30 | Default invoice expiration |
| DATA_PURGE_DAYS | 30 | Reserved for future data retention policy |

### Docker (Production)

```bash
docker build -t cipherpay .
docker run -d \
  --env-file .env \
  -p 3080:3080 \
  cipherpay
```

---

## Webhook Events

CipherPay fires HTTP POST requests to the merchant's `webhook_url` on status changes.

**Payload:**
```json
{
  "event": "detected",
  "invoice_id": "uuid",
  "txid": "abc123...",
  "timestamp": "2026-02-21T15:05:00Z"
}
```

**Events:**
- `detected` -- payment seen in mempool
- `confirmed` -- payment included in a block

**Retry policy:** Up to 5 attempts with exponential backoff (1min, 5min, 25min, 2h, 10h).

---

## Trial Decryption (Technical Details)

The scanner performs trial decryption on shielded transaction outputs:

1. Fetch raw transaction hex from CipherScan API
2. Parse the transaction using `zcash_primitives::transaction::Transaction`
3. For **Orchard** outputs (V5 transactions):
   - Extract `OrchardBundle` from the transaction
   - Derive `IncomingViewingKey` from the merchant's UFVK
   - For each action, attempt `orchard::note_encryption::try_note_decryption()`
   - On success, extract the 512-byte memo field
4. For **Sapling** outputs (V4/V5 transactions):
   - Extract `SaplingBundle` from the transaction
   - Derive Sapling IVK from the UFVK
   - For each output, attempt `sapling_crypto::note_encryption::try_sapling_note_decryption()`
   - On success, extract the memo field
5. Parse memo bytes as UTF-8, match against pending invoice memo codes

### Performance

- Trial decryption per output: ~microseconds (ChaCha20-Poly1305)
- Typical block: 10-100 Orchard actions
- Full block scan: single-digit milliseconds
- Mempool: usually fewer than 50 pending transactions

---

## Security Considerations

- **Viewing keys**: Stored on the server. Use a dedicated store wallet, not a personal wallet
- **API keys**: SHA-256 hashed before storage, never stored in plaintext
- **No buyer PII**: CipherPay does not store shipping addresses, names, or other buyer personal data
- **Webhooks**: Delivery tracked with retry, failed deliveries logged
- **CORS**: Configurable, defaults to allow all origins for widget embedding
- **No private keys**: CipherPay never holds spending keys. It's watch-only
- **Rate limiting**: Should be added before production deployment

---

## License

MIT

# CipherPay

**Shielded Zcash payment gateway.** Accept private ZEC payments on any website — like Stripe, but for Zcash.

Built on [CipherScan](https://cipherscan.app) APIs. No full node required.

## Features

- **Shielded payments** — Orchard + Sapling trial decryption
- **Mempool detection** — payments detected in ~5 seconds, confirmed after 1 block
- **Multi-currency pricing** — prices in EUR, USD, BRL, GBP with real-time ZEC conversion
- **Products & prices** — Stripe-like product catalog with multiple price points per product
- **Subscriptions** — recurring billing with automatic invoice generation
- **Hosted checkout** — embeddable checkout flow via companion frontend
- **Buy links** — direct product purchase via slug-based URLs (`/buy/my-product`)
- **HTTP 402 (x402)** — machine-to-machine payment verification
- **REST API** — invoices, products, prices, subscriptions, SSE streaming
- **HMAC-signed webhooks** — `invoice.confirmed`, `invoice.expired`, `invoice.cancelled`
- **Usage-based billing** — 1% fee on confirmed payments, settled in ZEC
- **Auto-purge** — customer data purged after configurable period (default 30 days)
- **Account recovery** — encrypted recovery emails via Resend
- **Self-hostable** — single binary, SQLite, no external dependencies beyond CipherScan

## Architecture

```
┌──────────────┐         ┌──────────────┐         ┌──────────────┐
│  Your Store  │──API───▶│  CipherPay   │──API───▶│  CipherScan  │
│  (frontend)  │         │   (this)     │◀──poll──│  (blockchain) │
└──────────────┘         └──────────────┘         └──────────────┘
                               │                        ▲
                         ┌─────┴─────┐                  │
                         │  Scanner  │──mempool/blocks───┘
                         └───────────┘
```

## Quick Start

```bash
# Clone and configure
cp .env.example .env
# Set ENCRYPTION_KEY: openssl rand -hex 32

# Run
cargo run
```

The server starts on `http://localhost:3080`.

## API Overview

### Merchant Registration

```bash
curl -X POST http://localhost:3080/api/merchants \
  -H "Content-Type: application/json" \
  -d '{"ufvk": "uview1...", "name": "My Store"}'
```

Returns `api_key` and `dashboard_token` — save these, they're shown only once.

### Create a Product with Prices

```bash
# Create product
curl -X POST http://localhost:3080/api/products \
  -H "Authorization: Bearer <api_key>" \
  -H "Content-Type: application/json" \
  -d '{"name": "T-Shirt", "slug": "t-shirt"}'

# Add a price
curl -X POST http://localhost:3080/api/prices \
  -H "Authorization: Bearer <api_key>" \
  -H "Content-Type: application/json" \
  -d '{"product_id": "<product_id>", "unit_amount": 29.99, "currency": "USD"}'
```

### Checkout (Create Invoice from Product)

```bash
curl -X POST http://localhost:3080/api/checkout \
  -H "Content-Type: application/json" \
  -d '{"product_id": "<product_id>", "price_id": "<price_id>"}'
```

### Create Invoice Directly

```bash
curl -X POST http://localhost:3080/api/invoices \
  -H "Authorization: Bearer <api_key>" \
  -H "Content-Type: application/json" \
  -d '{"product_name": "T-Shirt", "size": "L", "price_eur": 65.00}'
```

### Payment Status (SSE)

```bash
curl -N http://localhost:3080/api/invoices/<id>/stream
```

### Webhooks

Configure your webhook URL in the dashboard. CipherPay sends POST requests signed with HMAC-SHA256:

| Event | When |
|-------|------|
| `invoice.confirmed` | Payment confirmed (1 block) |
| `invoice.expired` | Invoice timed out |
| `invoice.cancelled` | Invoice cancelled |

Headers: `X-CipherPay-Signature`, `X-CipherPay-Timestamp`

Signature = HMAC-SHA256(`timestamp.body`, `webhook_secret`)

## API Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/health` | — | Health check |
| `GET` | `/api/rates` | — | Current ZEC exchange rates |
| **Merchants** | | | |
| `POST` | `/api/merchants` | — | Register merchant |
| `GET` | `/api/merchants/me` | Session | Get merchant profile |
| `PATCH` | `/api/merchants/me` | Session | Update profile |
| `POST` | `/api/merchants/me/delete` | Session | Delete account |
| **Auth** | | | |
| `POST` | `/api/auth/session` | — | Login (dashboard token) |
| `POST` | `/api/auth/logout` | Session | Logout |
| `POST` | `/api/auth/recover` | — | Request recovery email |
| `POST` | `/api/auth/recover/confirm` | — | Confirm recovery |
| **Products** | | | |
| `POST` | `/api/products` | API key | Create product |
| `GET` | `/api/products` | API key | List products |
| `PATCH` | `/api/products/{id}` | API key | Update product |
| `DELETE` | `/api/products/{id}` | API key | Deactivate product |
| `GET` | `/api/products/{id}/public` | — | Public product info (supports slug lookup) |
| **Prices** | | | |
| `POST` | `/api/prices` | API key | Create price |
| `PATCH` | `/api/prices/{id}` | API key | Update price |
| `DELETE` | `/api/prices/{id}` | API key | Deactivate price |
| `GET` | `/api/prices/{id}/public` | — | Public price info |
| `GET` | `/api/products/{id}/prices` | API key | List prices for product |
| **Invoices** | | | |
| `POST` | `/api/invoices` | API key | Create invoice |
| `POST` | `/api/checkout` | — | Create invoice from product/price |
| `GET` | `/api/invoices` | API key | List invoices |
| `GET` | `/api/invoices/{id}` | — | Get invoice |
| `POST` | `/api/invoices/{id}/finalize` | — | Lock exchange rate |
| `POST` | `/api/invoices/{id}/cancel` | API key | Cancel invoice |
| `POST` | `/api/invoices/{id}/refund` | API key | Refund invoice |
| `PATCH` | `/api/invoices/{id}/refund-address` | — | Set refund address |
| `GET` | `/api/invoices/{id}/stream` | — | SSE payment stream |
| `GET` | `/api/invoices/{id}/status` | — | Poll payment status |
| `GET` | `/api/invoices/{id}/qr` | — | QR code image |
| **Subscriptions** | | | |
| `POST` | `/api/subscriptions` | API key | Create subscription |
| `GET` | `/api/subscriptions` | API key | List subscriptions |
| `POST` | `/api/subscriptions/{id}/cancel` | API key | Cancel subscription |
| **Billing** | | | |
| `GET` | `/api/merchants/me/billing` | Session | Billing summary |
| `GET` | `/api/merchants/me/billing/history` | Session | Billing history |
| `POST` | `/api/merchants/me/billing/settle` | Session | Settle outstanding fees |
| **API Keys** | | | |
| `GET` | `/api/merchants/me/keys` | Full / Session | List full + restricted API keys |
| `POST` | `/api/merchants/me/keys` | Full / Session | Mint a new key (`{type, label}`) — returns raw key once |
| `DELETE` | `/api/merchants/me/keys/{id}` | Full / Session | Revoke a key immediately |
| **x402** | | | |
| `POST` | `/api/x402/verify` | API key | Verify HTTP 402 payment |
| `GET` | `/api/merchants/me/x402/history` | Session | x402 verification history |

## Project Structure

```
src/
├── main.rs                    # Server setup, scanner spawn
├── config.rs                  # Environment configuration
├── db.rs                      # SQLite pool + migrations
├── crypto.rs                  # AES-256-GCM encryption, key derivation
├── email.rs                   # Recovery emails via Resend HTTP API
├── addresses.rs               # Zcash address derivation from UFVK
├── validation.rs              # Input validation
├── api/
│   ├── mod.rs                 # Route config, checkout, SSE, billing, refunds
│   ├── admin.rs               # Admin dashboard endpoints
│   ├── auth.rs                # Sessions, recovery, API key management
│   ├── invoices.rs            # Invoice CRUD + finalization
│   ├── merchants.rs           # Merchant registration
│   ├── prices.rs              # Price management
│   ├── products.rs            # Product CRUD + public lookup (ID or slug)
│   ├── rates.rs               # ZEC exchange rates
│   ├── status.rs              # Invoice status polling
│   ├── subscriptions.rs       # Subscription management
│   └── x402.rs                # HTTP 402 payment verification
├── billing/
│   └── mod.rs                 # Usage fee calculation + settlement
├── invoices/
│   ├── mod.rs                 # Invoice logic, expiry, purge
│   ├── matching.rs            # Memo-to-invoice matching
│   └── pricing.rs             # Price feed + ZEC conversion cache
├── merchants/
│   └── mod.rs                 # Merchant data access
├── prices/
│   └── mod.rs                 # Price data access + validation
├── products/
│   └── mod.rs                 # Product data access + slug lookup
├── scanner/
│   ├── mod.rs                 # Mempool + block polling loop
│   ├── mempool.rs             # Mempool tx fetching
│   ├── blocks.rs              # Block scanning
│   └── decrypt.rs             # Orchard trial decryption
├── subscriptions/
│   └── mod.rs                 # Recurring billing logic
└── webhooks/
    └── mod.rs                 # HMAC dispatch + retry
```

## Configuration

See [`.env.example`](.env.example) for all options. Key settings:

| Variable | Description |
|----------|-------------|
| `DATABASE_URL` | SQLite path (default: `sqlite:cipherpay.db`) |
| `CIPHERSCAN_API_URL` | CipherScan API endpoint |
| `NETWORK` | `testnet` or `mainnet` |
| `ENCRYPTION_KEY` | 32-byte hex key for UFVK encryption at rest |
| `RESEND_API_KEY` | Resend API key for recovery emails |
| `RESEND_FROM` | Sender email address |
| `MEMPOOL_POLL_INTERVAL_SECS` | How often to scan mempool (default: 5s) |
| `BLOCK_POLL_INTERVAL_SECS` | How often to scan blocks (default: 15s) |
| `INVOICE_EXPIRY_MINUTES` | Invoice TTL (default: 30min) |
| `DATA_PURGE_DAYS` | Days before customer data is purged (default: 30) |
| `BILLING_FEE_RATE` | Fee rate on confirmed payments (default: 0.01 = 1%) |
| `BILLING_FEE_ADDRESS` | Zcash address for fee settlement |

## Deployment

Recommended: systemd + Caddy on a VPS.

```bash
cargo build --release
# Binary at target/release/cipherpay
```

See the companion frontend at [cipherpay-web](https://github.com/atmospherelabs-dev/cipherpay-web) for the hosted checkout and merchant dashboard.

## Related

- **[CipherPay Web](https://github.com/atmospherelabs-dev/cipherpay-web)** — Next.js frontend
- **[CipherPay Shopify](https://github.com/atmospherelabs-dev/cipherpay-shopify)** — Shopify integration
- **[CipherScan](https://cipherscan.app)** — Zcash blockchain explorer

## License

MIT

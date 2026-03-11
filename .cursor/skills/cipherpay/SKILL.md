---
name: cipherpay
description: CipherPay Rust backend — Zcash payment processor. Non-custodial, shielded-first.
metadata:
  author: cipherpay
  version: "1.0.0"
---

# CipherPay Backend Rules

Project-specific guidelines for the CipherPay Rust backend — a non-custodial Zcash payment processor.

## When to Use

These rules apply to ALL work on the CipherPay backend:
- API development (Actix-web)
- Invoice and payment logic
- Blockchain scanning (via CipherScan API)
- Billing and fee collection
- Merchant management
- Database schema changes (SQLite)

## Categories

| Category | Priority | Description |
|----------|----------|-------------|
| Security | Critical | UFVK handling, webhook HMAC, encryption at rest |
| Invoice Logic | Critical | Diversified addresses, ZIP-321 URIs, expiry |
| Billing | Critical | Fee ledger, settlement invoices, billing cycles |
| Scanner | High | Mempool polling, trial decryption, payment detection |
| API Conventions | High | Auth, rate limiting, CORS |
| Database | High | SQLite, migrations, parameterized queries |

## Architecture

```
src/
├── main.rs           # Actix-web server + scanner spawn
├── config.rs         # Environment-based configuration
├── db.rs             # SQLite pool + migrations
├── crypto.rs         # AES-256-GCM encryption for UFVKs at rest
├── validation.rs     # Input validation
├── email.rs          # SMTP notifications
├── api/              # REST API routes (invoices, merchants, products, billing)
├── invoices/         # Invoice creation, pricing, diversified addresses
├── merchants/        # Merchant CRUD, UFVK registration
├── products/         # Product catalog
├── billing/          # Fee ledger, billing cycles, settlement
├── scanner/          # Mempool + block polling, trial decryption
├── addresses.rs      # UFVK → diversified address derivation
└── webhooks/         # Outbound webhook delivery with HMAC signing
```

## Critical Rules

1. **Never log UFVKs, API keys, or webhook secrets** — these are encrypted at rest (AES-256-GCM)
2. **Always use parameterized queries** — never string interpolation for SQL
3. **Invoices use per-invoice diversified addresses** — no address reuse, ever
4. **ZIP-321 payment URIs** — dual-output (merchant + fee) when fees enabled
5. **Webhook HMAC** — all outbound webhooks signed with `x-cipherpay-signature` and `x-cipherpay-timestamp`
6. **Fee collection** — 1% via ZIP-321 dual-output, tracked in fee_ledger, settled via billing cycles

## Related Projects

- **cipherpay-web**: Frontend dashboard and checkout page (Next.js)
- **cipherpay-shopify**: Shopify app integration
- **cipherscan / cipherscan-rust**: Blockchain data source (API)

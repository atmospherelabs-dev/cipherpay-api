# CipherPay Roadmap

Privacy-preserving Zcash payment gateway. Non-custodial, shielded-only.

---

## Phase 0 -- Testnet MVP (Done)

- [x] Rust/Actix-web service with SQLite
- [x] Merchant registration with UFVK + payment address
- [x] API key authentication (Bearer token, SHA-256 hashed)
- [x] Invoice creation with locked ZEC price at creation time
- [x] Unique memo codes (CP-XXXXXXXX) for payment matching
- [x] QR code generation with zcash: URI (amount + hex-encoded memo)
- [x] Orchard trial decryption ported from zcash-explorer WASM module
- [x] Mempool polling scanner (configurable interval)
- [x] Block scanner for missed/confirmed transactions
- [x] Webhook delivery with retry logic (up to 5 attempts)
- [x] Invoice expiry and automatic status transitions
- [x] Data purge (shipping PII nullified after configurable window)
- [x] CoinGecko price feed with caching and fallback
- [x] Test console UI for end-to-end flow testing
- [x] Testnet guide documentation

## Phase 1 -- Security Hardening (Current)

- [x] Gate simulation endpoints behind `is_testnet()` (prevent free-inventory exploit)
- [x] Payment amount verification with 0.5% slippage tolerance (penny exploit fix)
- [x] Webhook HMAC-SHA256 signing with `X-CipherPay-Signature` + `X-CipherPay-Timestamp`
- [x] Per-merchant `webhook_secret` generated on registration
- [x] Conditional CORS (allow-any on testnet, restricted origins on mainnet)
- [x] `ALLOWED_ORIGINS` config for production deployment
- [x] Concurrent batch raw tx fetching (futures::join_all, batches of 20)
- [x] CipherScan raw tx endpoint (`GET /api/tx/{txid}/raw`)
- [ ] Rate limiting on public endpoints (actix-web-middleware or tower)
- [ ] Invoice lookup auth (merchant can only see own invoices)
- [ ] Merchant registration guard (admin key or invite-only in production)
- [ ] Input validation hardening (UFVK format check, address validation)
- [x] **Switch from UFVK to UIVK storage** — accept UFVK or UIVK at registration, derive and store only the UIVK (discard FVK). Existing merchants migrated on startup. Reduces data exposure per principle of least privilege.
- [ ] **Account deletion cooldown** — schedule deletion for 48h instead of immediate hard-delete. Protects against compromised sessions. Merchant can cancel within the window. After 48h, purge all data (viewing keys, invoices, products, sessions).

## Phase 2 -- Performance & Real-Time

- [ ] **Parallel trial decryption** with `rayon` (.par_iter() over merchants x actions)
- [ ] **CipherScan WebSocket stream** (`ws://api.cipherscan.app/mempool/stream`)
  - Push raw tx hex as Zebra sees new txids
  - Eliminates polling latency entirely
  - Single persistent connection per CipherPay instance
- [ ] **CipherScan batch raw tx endpoint** (`POST /api/tx/raw/batch`)
  - Accept array of txids, return array of hex
  - Single HTTP round-trip instead of N calls
- [ ] Mempool deduplication improvements (bloom filter for seen txids)
- [ ] Sapling trial decryption support (currently Orchard-only)
- [ ] Scanner metrics (Prometheus endpoint: decryption rate, latency, match rate)

## Phase 3 -- Integrations & Go-to-Market

- [ ] **Hosted checkout page** (`pay.cipherpay.app/{invoice_id}`)
  - Standalone payment page merchants can redirect to
  - Mobile-optimized with QR code and deep-link to Zashi/YWallet
- [ ] **Shopify Custom App integration**
  - Merchant installs Custom App in Shopify admin
  - CipherPay marks orders as paid via Shopify Admin REST API
  - (`POST /admin/api/2024-10/orders/{id}/transactions.json`)
  - Avoids Shopify App Store approval process
- [ ] **WooCommerce plugin** (WordPress/PHP webhook receiver)
- [ ] **Embeddable widget** (JS snippet for any website)
  - `<script src="https://pay.cipherpay.app/widget.js">`
  - Drop-in payment button with modal checkout
- [ ] Multi-currency display (EUR, USD, GBP with locked ZEC rate)
- [ ] Email notifications (optional, privacy-conscious: no PII in email body)

## Phase 4 -- Production Infrastructure

- [ ] **Encryption at rest** for UFVKs (AES-256-GCM with HSM-backed key)
- [ ] **Data minimization enforcement**
  - Automatic PII overwrite (shipping data) after 14-day return window
  - Cron job with cryptographic erasure (not just NULL, but overwrite)
  - Zero-knowledge order fulfillment: merchant gets shipping label, not raw address
- [ ] **PostgreSQL migration** for production (multi-tenant, proper indexing)
- [ ] **Multi-node CipherScan infrastructure**
  - Load balancer in front of multiple Zebra nodes
  - Benefits CipherScan explorer, CipherPay, and network decentralization
  - Geographic distribution for latency reduction
- [ ] **Redis caching layer** (rate data, merchant lookup, invoice status)
- [ ] Docker / docker-compose production deployment
- [ ] Kubernetes manifests for auto-scaling
- [ ] Audit logging (who accessed what, when -- without logging PII)

## Phase 5 -- AI Agent Infrastructure

- [x] **x402 facilitator** — verify shielded Zcash payments for HTTP 402 paywalls
- [x] **MPP support** — Machine Payments Protocol (WWW-Authenticate / Authorization headers)
- [x] **Replay protection** — per-merchant txid tracking, prevents double-use of payment proofs
- [x] **@cipherpay/x402 SDK** — Express middleware for resource servers, supports x402 + MPP + sessions (`npm install @cipherpay/x402`)
- [x] **@cipherpay/mcp** — MCP server for Claude/Cursor (invoices, rates, x402 verify, sessions)
- [x] **Agent sessions** — prepaid credit system: deposit ZEC, get bearer token, pay per-request
- [x] **Agent wallet CLI** (`@cipherpay/zipher-cli`) — headless Zcash wallet for agents (pay, sessions, x402, MPP)
- [x] **UIVK uniqueness enforcement** — reject registration if viewing key already belongs to a merchant (prevents duplicate scanning, double billing, cross-merchant payment confusion). Applies to both dashboard and programmatic registration.
- [ ] **Programmatic merchant registration** — agents create their own merchant accounts via API
  - `POST /api/merchants/register` with `{ ufvk, payment_address }`
  - Requires ~$10 USD deposit in shielded ZEC (anti-spam)
  - Deposit split: portion kept as CipherPay activation fee, remainder credited to merchant fee balance
  - Returns `{ merchant_id, api_key }` — no dashboard, no password, API-only
  - Agent merchants have no dashboard access by design (no credentials to leak via prompt injection)
  - If human wants dashboard: register normally, hand API key to agent
  - Enables fully autonomous agent-to-agent commerce
  - Rate limited + UFVK validation before scanner activation
- [ ] **@cipherpay/wallet-mcp** — MCP server wrapping `zipher-cli` so AI agents can send ZEC
- [ ] **Multi-recipient send** — enable batch payments from a single agent transaction

## Phase 6 -- Referral Program (requires Phase 5: operational wallet)

- [ ] **Merchant referral program** — referrers earn 0.5% of referred merchants' volume for 12 months
  - Referred merchants get 0.5% fee (instead of 1%) for first 3 months
  - Commissions paid via 3-way ZIP 321 split (auto-collected) or operational wallet payout (fallback)
  - Anti-gaming: 7-day account age + 3 invoices to refer, 0.5 ZEC minimum volume to activate
  - Referral dashboard tab with code generation, stats, and earnings
  - Depends on `zipher-cli` for CipherPay operational wallet (automated payouts)

## Phase 7 -- Monetization (Open Core + SaaS)

- [ ] **Self-hosted** (free, open source, BTCPay Server model)
  - Full feature parity, run your own CipherPay + Zebra node
  - Community support via GitHub issues
- [ ] **Hosted SaaS tiers**
  - Free tier: 50 invoices/month, community support
  - Pro tier: unlimited invoices, priority webhook delivery, dashboard
  - Enterprise: dedicated infrastructure, SLA, custom integrations
- [ ] API key rate limiting per tier
- [ ] Merchant dashboard (invoice history, analytics, webhook logs)
- [ ] Multi-merchant management (agencies managing multiple stores)

---

## CipherScan Improvements (for CipherPay support)

These are changes needed in the CipherScan explorer/indexer to support CipherPay at scale:

- [x] `GET /api/tx/{txid}/raw` -- raw hex endpoint for trial decryption
- [ ] `POST /api/tx/raw/batch` -- batch raw hex endpoint (Phase 2)
- [ ] `ws://api.cipherscan.app/mempool/stream` -- WebSocket mempool stream (Phase 2)
- [ ] Multi-Zebra-node infrastructure with load balancing (Phase 4)
- [ ] Rate limit tiers for API consumers (Phase 5)
- [ ] Dedicated CipherPay API key with higher rate limits (Phase 5)
- [ ] Raw tx hex storage in Postgres indexer (avoid RPC round-trips) (Phase 4)

---

## Design Principles

1. **Non-custodial**: CipherPay never holds funds. Merchants provide viewing keys (UIVK migration planned — store only incoming viewing keys).
2. **Privacy-first**: Shielded transactions only. No transparent address support.
3. **Data minimization**: Delete what you don't need. Encrypt what you must keep.
4. **Self-hostable**: Any merchant can run their own instance.
5. **CipherScan-native**: Leverages existing infrastructure, doesn't duplicate it.

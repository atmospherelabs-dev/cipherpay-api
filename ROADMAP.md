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

## Phase 1 -- Security Hardening

- [x] Gate simulation endpoints behind `is_testnet()` (prevent free-inventory exploit)
- [x] Payment amount verification with 0.5% slippage tolerance (penny exploit fix)
- [x] Webhook HMAC-SHA256 signing with `X-CipherPay-Signature` + `X-CipherPay-Timestamp`
- [x] Per-merchant `webhook_secret` generated on registration
- [x] Conditional CORS (allow-any on testnet, restricted origins on mainnet)
- [x] `ALLOWED_ORIGINS` config for production deployment
- [x] Concurrent batch raw tx fetching (futures::join_all, batches of 20)
- [x] CipherScan raw tx endpoint (`GET /api/tx/{txid}/raw`)
- [x] **Switch from UFVK to UIVK storage** — accept UFVK or UIVK at registration, derive and store only the UIVK (discard FVK). Existing merchants migrated on startup. Reduces data exposure per principle of least privilege.
- [x] **Encryption at rest** — AES-256-GCM encryption for viewing keys, webhook secrets, Luma API keys, and attendee PII. Mandatory `ENCRYPTION_KEY` on mainnet.
- [x] **Webhook delivery ID** — `X-CipherPay-Delivery-Id` header on outbound webhooks for retry deduplication
- [x] **Dust fee threshold** — fees below 25,000 zatoshis (≈$0.05) waived to avoid uneconomical dust outputs
- [x] **SQLite busy_timeout** — 5s timeout to prevent read failures when scanner holds the write lock
- [x] **Targeted rate limiting** — strict limits on auth and session setup (`prepare` / `open`), without throttling high-throughput commerce and agent hot paths (`checkout`, `payment-links/checkout`, `x402/verify`, session consume)
- [x] **UFVK/UIVK network validation** — prefix-based network check prevents testnet keys on mainnet and vice versa
- [ ] **Payment link server-side resolution** — move invoice creation from public API endpoint (`POST /api/payment-links/{slug}/checkout`) into the Next.js server component. Invoice creation happens server-to-server (authenticated with internal key), removing the unauthenticated public endpoint entirely. Follows Stripe's model: buyers load a web page, not an API. Page-level protection via Vercel/Cloudflare.
- [ ] Per-API-key / per-merchant rate limiting — keyed rate limit buckets for authenticated routes, per-IP for unauthenticated setup flows. Preserve high-throughput commerce and agent hot paths while isolating abusive tenants. `429` with `Retry-After`.
- [ ] Invoice lookup auth (merchant can only see own invoices via API; checkout page unaffected)
- [ ] **Account deletion cooldown** — schedule deletion for 48h instead of immediate hard-delete. Protects against compromised sessions. Merchant can cancel within the window. After 48h, purge all data (viewing keys, invoices, products, sessions).

## Phase 2 -- Performance & Real-Time

- [x] **CipherScan WebSocket stream** (real-time mempool push via service key)
  - CipherScan receives mempool events from Zebra gRPC indexer
  - Service clients subscribe to `raw_mempool` channel via `X-Service-Key` header
  - Raw tx hex pushed to CipherPay on every mempool event — zero HTTP overhead
  - Sub-second payment detection (was 5s polling)
  - 30s polling retained as resilience fallback
  - Auto-reconnect with exponential backoff (3s → 30s cap)
- [ ] **hasOrchard early filter** — skip non-Orchard txs before trial decryption (CipherPay side). Quick win at scale.
- [ ] **Cached pending invoices + merchant keys** — refresh every 2–5s instead of per-WS-push DB query. Removes SQLite bottleneck under high mempool throughput.
- [ ] **Parallel trial decryption** with `rayon` (.par_iter() over merchants × actions). Near-linear speedup across CPU cores.
- [ ] **CipherScan batch raw tx endpoint** (`POST /api/tx/raw/batch`)
  - Accept array of txids, return array of hex
  - Single HTTP round-trip instead of N calls (for polling fallback path)
- [ ] Mempool deduplication improvements (bloom filter for seen txids)
- [ ] Sapling trial decryption support (currently Orchard-only)
- [ ] Scanner metrics (Prometheus endpoint: decryption rate, latency, match rate)
- [ ] **Scanner/API worker split** — separate the request-serving API from scanner/background workers so traffic spikes cannot starve payment detection or webhook retries
- [ ] **Durable job queue for webhook + async tasks** — move retries and long-running side effects out of ad hoc in-process spawning so restarts do not become backlog or observability blind spots
- [ ] **Operational metrics + alerts** — scanner lag, webhook backlog, DB contention, x402/session verification latency, and rate-limit hit rates

## Phase 3 -- Integrations & Go-to-Market

- [x] **Hosted checkout page** (`pay.cipherpay.app/{invoice_id}`)
  - Standalone payment page merchants can redirect to
  - Mobile-optimized with QR code and deep-link to Zashi/YWallet
  - Checkout API returns `checkout_url` with optional `success_url` redirect baked in
- [x] **Shopify Custom App integration**
  - Merchant installs Custom App in Shopify admin
  - CipherPay marks orders as paid via Shopify Admin REST API
  - (`POST /admin/api/2024-10/orders/{id}/transactions.json`)
  - Avoids Shopify App Store approval process
- [x] **WooCommerce plugin** (WordPress/PHP webhook receiver)
- [x] **Payment Links (no-code)**
  - Reusable URLs tied to a price — visit creates invoice, redirects to checkout
  - Dashboard tab for managing links; public resolve endpoint with rate limiting
- [x] **Subscriptions / recurring payments**
  - Full CRUD API for subscription lifecycle (create, cancel, pause, resume)
  - Automatic period advancement on payment confirmation
  - Renewal invoice generation in scanner loop
  - Dashboard management for merchants
- [x] **Luma event ticketing integration**
  - Merchants link Luma events to CipherPay products
  - On payment confirmation: auto-register buyer on Luma via API
  - Retry with exponential backoff (5 attempts, transient error detection)
  - PII wiped after successful registration
  - Private event tickets with unique codes (non-Luma events)
- [x] **Multi-currency pricing** — 11 currencies supported (EUR, USD, BRL, GBP, CAD, JPY, MXN, ARS, NGN, CHF, INR) with locked ZEC rate at invoice creation
- [x] **Account recovery** — email-based recovery flow via Resend API
- [x] **i18n** — dashboard and checkout in English, Spanish, and Portuguese
- [ ] **Embeddable widget** (JS snippet for any website)
  - `<script src="https://pay.cipherpay.app/widget.js">`
  - Drop-in payment button with modal checkout
- [ ] Email notifications (payment confirmations, invoice reminders, subscription renewals — privacy-conscious: no PII in body)
- [ ] **Blog** (`/blog`) — product updates, integration guides, campaign spotlights, privacy commentary. MDX-based, built into Next.js app.
- [ ] **Developer changelog** (`/changelog`) — running log of features, fixes, and updates. Signals active development.
- [ ] **Use cases page** (`/use-cases`) — e-commerce, donations, ticketing, AI agents, subscriptions. Conversion page for new visitors.

## Phase 3.5 -- Donation Infrastructure

- [x] **Donation mode for payment links** — `mode` column on `payment_links` ('payment' | 'donation')
  - Donation links have no `price_id` — amount chosen by donor at checkout
  - `donation_config` JSON: mission statement, suggested amounts, thank-you message, campaign name/goal
  - `total_raised` counter incremented atomically on confirmation (idempotent via `campaign_counted` flag)
  - `payment_link_id` FK on invoices for campaign progress tracking
  - `is_donation` flag on invoices for checkout UI adaptation
  - Same fee structure as commerce (no bypass vector)
- [x] **Donor-facing campaign page** (`/donate/{slug}`)
  - Preset amount buttons (configurable per link) + custom amount input
  - Campaign progress bar (capped at 100%, continues accepting after goal)
  - Org name + mission statement + cover image with position control
  - Min/max amount validation ($1–$10K default, configurable)
  - Human-readable slugs with anti-phishing blocklist
  - Dynamic Open Graph meta tags (campaign image + title for link previews)
  - Resolves to standard checkout flow (`/pay/{id}`)
- [x] **Donation checkout UX** — conditional UI in existing checkout page
  - Campaign name as heading with "by [Org]" subtitle
  - Custom thank-you message from org (replaces generic receipt)
  - "DONATION" tag in header during pending state
  - Hide refund address field (donations aren't refundable)
  - Campaign name links back to campaign page
  - "Share on X" button with configurable social share text
  - Thank-you receipt shown immediately on detection (no separate confirming state)
- [x] **Dashboard donation management** — integrated into existing tabs
  - Donation link creation/editing in Links tab (toggle: Payment Links | Donation Links)
  - Full campaign editing (name, mission, goal, amounts, images, thank-you message)
  - `DONATION` type badge in Invoices tab with filter
  - Campaign name as `product_name` on invoices for multi-campaign clarity
  - Confirmed donation count (not just created invoices)
- [x] **Public donation link info endpoint** (`GET /api/payment-links/{slug}/info`)
  - Returns donation config, campaign progress, org name (no invoice creation)
  - Separate generous rate limiter (read-only endpoint)
  - Powers campaign page and future embeddable widget
- [x] **Documentation** — donation mode setup, API endpoints, campaign lifecycle
- [ ] **Campaign directory** (`/campaigns`) — public opt-in listing of active donation campaigns. Cover images, progress bars, category filters. Each campaign is a landing page for organic discovery.
- [ ] **"The Statement" page** (`/statement`) — merchants publicly declare they accept private payments. Opt-in cards with logo, quote, and store link. Embeddable "Accepts Zcash" badge for merchant websites (backlinks + organic distribution).
- [ ] **Verification badges** — verified nonprofit/humanitarian org status
  - Manual verification process initially (org applies, we verify)
  - Badge displayed on campaign page and checkout
  - Unlocks fee waivers and visibility in future org directory
- [ ] **Fee waivers for verified nonprofits** — `fee_exempt` flag on verified merchants
- [ ] **Round-up donations at checkout** — merchants opt in, customers round up for a cause
  - Third output added to ZIP-321 URI (merchant + fee + charity)
  - Org selectable from verified CipherPay nonprofits
  - Depends on Phase 3.6 wallet testing for 3-output support

## Phase 3.6 -- Charity Split (requires Phase 3.5 + wallet testing)

- [ ] **3-output ZIP-321 wallet testing** — verify Zashi/YWallet handle 3+ output URIs on testnet
  - CipherPay already uses 2-output (merchant + fee); charity adds a 3rd
  - Blocking test: if wallets don't support 3 outputs, this phase is deferred
- [ ] **Merchant charity pledge** — dashboard setting: "Donate X% of sales to [Org]"
  - Third output added to ZIP-321 URI at invoice creation (merchant + fee + charity)
  - Org must be registered on CipherPay with a viewing key
  - Badge on checkout page: "This merchant supports [Org Name]"
- [ ] **Donor opt-in at checkout** — "Add $1 to [Org]?" toggle on payment page
  - Adjusts total and adds third output to zcash URI
  - Org selectable from CipherPay-registered nonprofits
- [ ] **Round-up for charity** — round to nearest dollar, difference goes to org
  - Classic retail pattern adapted to shielded payments

## Phase 4 -- Production Infrastructure

- [x] **Encryption at rest** — AES-256-GCM for viewing keys, secrets, and PII (see Phase 1). HSM-backed key management is a future hardening step.
- [ ] **Data minimization enforcement**
  - Automatic PII overwrite (shipping data) after 14-day return window
  - Cron job with cryptographic erasure (not just NULL, but overwrite)
  - Zero-knowledge order fulfillment: merchant gets shipping label, not raw address
- [ ] **PostgreSQL migration** for production (multi-tenant, proper indexing)
- [ ] **Read/write role separation after Postgres** — reserve migration path for scanner-heavy reads, webhook writes, and dashboard queries so one hot connection pool does not do everything
- [ ] **Multi-node CipherScan infrastructure**
  - Load balancer in front of multiple Zebra nodes
  - Benefits CipherScan explorer, CipherPay, and network decentralization
  - Geographic distribution for latency reduction
- [ ] **Redis caching layer** (rate data, merchant lookup, invoice status)
- [ ] Docker / docker-compose production deployment
- [ ] Kubernetes manifests for auto-scaling
- [ ] Audit logging (who accessed what, when -- without logging PII)
- [x] **Protocol-scoped uniqueness on x402 verification log** — partial unique index on `(merchant_id, txid, protocol)` for verified proofs, preventing replay from creating duplicate accepted verifications

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
- [x] **Challenge expiry (`valid_until`)** — 402 responses include expiry timestamp; verification rejects stale challenges (prevents agents paying outdated prices after ZEC rate changes)
- [x] **`/.well-known/payment` discovery** — standardized endpoint for agents to auto-detect payment methods, currencies, protocols, session support, and facilitator URL
- [x] **Streaming (pay-per-token)** — SSE metering on top of sessions; middleware deducts in batches (~100 tokens), sends `event: payment_required` when balance insufficient. Non-custodial (same session model, different metering)
- [x] **Address-based session deposits** — generate unique deposit address per session (via diversifier), eliminating memo dependency. Future-proofs against NU7/Tachyon memo changes. Includes cleanup of abandoned prepare requests (30 min expiry)
- [x] **Session address binding** — address-based session opening now credits only outputs sent to the prepared receiver, preventing over-credit when a tx contains multiple outputs for the same merchant
- [x] **Billing enforcement on agent/facilitator flows** — `x402` verification and session setup/consume flows now respect `past_due` / `suspended` merchant status instead of only invoice creation
- [ ] **@cipherpay/wallet-mcp** — MCP server wrapping `zipher-cli` so AI agents can send ZEC
- [ ] **Multi-recipient send** — enable batch payments from a single agent transaction
- [ ] **Session refunds on close** — automatically refund unused balance when agent closes a session
  - Send remaining zatoshis to the `refund_address` provided at session open
  - Refund memo: `cipherpay-refund:{session_id}` for audit trail
  - Use persistent wallet instance (`Arc<TokioMutex<Wallet>>`) to avoid per-refund sync overhead
  - Depends on operational wallet (Phase 6)
- [ ] **Account pool for operational wallet** — ZIP-32 multi-account rotation within a single wallet
  - Each account has independent note pools, so back-to-back sends don't block on pending confirmations
  - Round-robin account selection (`AtomicU32` counter, mod N accounts)
  - Required for session refunds, referral payouts, and any high-throughput send path
  - Pattern from [zimppy `AccountPool`](https://github.com/betterclever/zimppy/blob/main/crates/zimppy-rs/src/pool.rs)

## Phase 6 -- Referral Program (requires Phase 5: operational wallet)

- [ ] **Merchant referral program** — referrers earn 0.5% of referred merchants' volume for 12 months
  - Referred merchants get 0.5% fee (instead of 1%) for first 3 months
  - Anti-gaming: 7-day account age + 3 invoices to refer, 0.5 ZEC minimum volume to activate
  - Referral dashboard tab with code generation, stats, and earnings
  - **Payout model (3 paths):**
    1. **Referrer is a merchant** → fee credit (deduct from owed fees). Pure ledger, no wallet needed.
    2. **Referrer is a non-merchant** → accumulate earnings in ledger, periodic payout via operational wallet when above threshold (e.g. 0.5 ZEC). Referrer registers with just a destination address.
    3. **3-way ZIP 321 split** (future, depends on Phase 3.6 wallet testing) → real-time on-chain: merchant + fee + referrer in one tx.
  - Commissions come from fees already collected — CipherPay always has the balance.
  - Depends on `zipher-cli` for operational wallet (path 2 payouts)

## Phase 7 -- Monetization (Open Core + SaaS)

- [ ] **Extract `cipherpay-core` crate** — split Orchard trial decryption, replay tracking, and Zebra RPC client out of the monolithic binary into a standalone reusable library
  - Enables self-hosted verification without running the full CipherPay server
  - Decouples verification from Actix/SQLite — usable in any Rust project
  - Prerequisite for NAPI bindings and self-hosted mode
  - Prior art: [zimppy-core](https://github.com/betterclever/zimppy/tree/main/crates/zimppy-core) — same pattern (verification engine as separate crate)
- [ ] **`@cipherpay/core-napi`** — NAPI-RS bindings exposing `cipherpay-core` to Node.js
  - Native Orchard decryption + replay protection in Node.js (no HTTP round-trip to CipherPay API)
  - Prebuilt binaries for darwin-arm64, linux-x64
  - Enables `@cipherpay/x402` to verify payments locally in self-hosted mode
  - Prior art: [@zimppy/core-napi](https://github.com/betterclever/zimppy/tree/main/crates/zimppy-napi) — same approach
- [ ] **Self-hosted** (free, open source, BTCPay Server model)
  - Full feature parity, run your own CipherPay + Zebra node
  - `cipherpay-core` + NAPI bindings make local verification possible without the full backend
  - Community support via GitHub issues
- [ ] **Hosted SaaS tiers**
  - Free tier: 50 invoices/month, community support
  - Pro tier: unlimited invoices, priority webhook delivery, dashboard
  - Enterprise: dedicated infrastructure, SLA, custom integrations
- [ ] API key rate limiting per tier
- [ ] Merchant dashboard (invoice history, analytics, webhook logs)
- [ ] Multi-merchant management (agencies managing multiple stores)
- [ ] **Evaluate `mpp-rs` crate adoption** — replace custom MPP challenge/credential handling in `@cipherpay/x402` with the first-party [mpp-rs](https://github.com/nicholasgasior/mpp) Rust crate
  - Reduces protocol maintenance burden as MPP spec evolves
  - Provides `ChargeMethod`, `SessionMethod`, `PaymentProvider` traits out of the box
  - CipherPay implements `ChargeMethod` for Zcash; agents use `PaymentProvider` for auto-pay
  - Prior art: [zimppy-rs](https://github.com/betterclever/zimppy/tree/main/crates/zimppy-rs) uses `mpp-rs` for all protocol plumbing

---

## CipherScan Improvements (for CipherPay support)

These are changes needed in the CipherScan explorer/indexer to support CipherPay at scale:

- [x] `GET /api/tx/{txid}/raw` -- raw hex endpoint for trial decryption
- [x] **WebSocket mempool stream with tiered broadcast** -- service clients (authenticated via `X-Service-Key`) subscribe to `raw_mempool` channel and receive `mempool_tx` events enriched with `raw_hex`. Regular browser clients receive the slim payload. Powered by Zebra gRPC indexer (`MempoolChange` + `ChainTipChange` streams).
- [ ] `POST /api/tx/raw/batch` -- batch raw hex endpoint (Phase 2)
- [ ] Multi-Zebra-node infrastructure with load balancing (Phase 4)
- [ ] Rate limit tiers for API consumers (Phase 5)
- [ ] Dedicated CipherPay API key with higher rate limits (Phase 5)
- [ ] Raw tx hex storage in Postgres indexer (avoid RPC round-trips) (Phase 4)

---

## Near-Term Scale Priorities

If the goal is safe growth before the Postgres cutover, the best next infrastructure order is:

1. Pending invoice / merchant-key caching in the scanner
2. Scanner metrics + alerting
3. Scanner/API worker split
4. Durable webhook / async job queue
5. PostgreSQL migration
6. Per-merchant keyed rate limiting

This sequence improves correctness and observability first, then unlocks higher sustained throughput without prematurely overcomplicating the stack.

---

## Design Principles

1. **Non-custodial**: CipherPay never holds funds. Merchants provide viewing keys (UIVK-only storage — only incoming viewing keys are kept).
2. **Privacy-first**: Shielded transactions only. No transparent address support.
3. **Data minimization**: Delete what you don't need. Encrypt what you must keep.
4. **Self-hostable**: Any merchant can run their own instance.
5. **CipherScan-native**: Leverages existing infrastructure, doesn't duplicate it.

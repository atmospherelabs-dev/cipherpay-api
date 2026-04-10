# Referral Program — Technical Spec

**Status:** Backlog
**Depends on:** Operational wallet (Phase 5) for non-merchant payouts
**Effort estimate:** ~5–7 days total

---

## Overview

Referrers earn a share of referred merchants' fees. Referred merchants get a temporary discount. Two referrer types: existing merchants (fee credit) and non-merchants (operational wallet payout).

---

## 1. Account Types

### Merchant referrer (no new account needed)

Existing merchants opt in to the referral program. A `referral_code` is generated on their merchant record.

- Earnings are deducted from owed fees (pure ledger operation, no wallet needed).
- Dashboard shows referral tab with code, stats, and earnings.

### Non-merchant referrer (new account type)

Lightweight account for promoters, influencers, community members who don't process payments.

- Register via `POST /api/referrers/register` with `{ payout_address, name? }`.
- No viewing key, no dashboard, no scanner entry.
- Gets a `referral_code` and an `api_key` (read-only: view stats and earnings).
- Payouts sent via operational wallet when accumulated balance exceeds threshold (e.g. 0.5 ZEC).
- **Depends on** `zipher-cli` operational wallet (Phase 5). Until then, earnings accumulate in ledger.

### Database

```sql
CREATE TABLE referrers (
    id TEXT PRIMARY KEY,
    name TEXT DEFAULT '',
    referral_code TEXT UNIQUE NOT NULL,
    api_key_hash TEXT NOT NULL,
    payout_address TEXT NOT NULL,           -- Zcash shielded address
    total_earned_zats INTEGER DEFAULT 0,
    total_paid_zats INTEGER DEFAULT 0,
    created_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    is_active INTEGER DEFAULT 1
);
```

For merchant referrers, add columns to `merchants`:

```sql
ALTER TABLE merchants ADD COLUMN referral_code TEXT UNIQUE;
ALTER TABLE merchants ADD COLUMN referral_earnings_zats INTEGER DEFAULT 0;
```

---

## 2. Referral Code System

### Code format

`CPREF-{8 alphanumeric}` — e.g. `CPREF-A3K9M2X7`. Short, human-shareable, URL-safe.

### Generation

- **Merchants:** `POST /api/account/referral-code` (authenticated). Generates code if none exists. Returns existing code if already generated.
- **Non-merchants:** Auto-generated on registration.
- Custom codes: allow merchants/referrers to request a custom code (e.g. `CPREF-MYNAME`) subject to uniqueness + blocklist.

### Lookup

Unified lookup across both tables:

```sql
-- Check merchants first, then referrers
SELECT id, 'merchant' AS type FROM merchants WHERE referral_code = ?
UNION ALL
SELECT id, 'referrer' AS type FROM referrers WHERE referral_code = ?
LIMIT 1;
```

---

## 3. Referral Application

### At registration (new merchants)

Add optional `referral_code` to `CreateMerchantRequest`:

```rust
pub struct CreateMerchantRequest {
    pub name: Option<String>,
    pub ufvk: String,
    pub webhook_url: Option<String>,
    pub email: Option<String>,
    pub referral_code: Option<String>,  // NEW
}
```

On registration:
1. Validate the referral code exists and belongs to an active referrer/merchant.
2. Reject self-referral (code owner == new merchant's viewing key).
3. Store `referred_by` and `referred_by_type` on the new merchant record.
4. Activate reduced fee period (0.5% instead of 1% for 3 months).

### Retroactive (existing merchants)

`PATCH /api/account` with `{ "referral_code": "CPREF-..." }`:
- Only allowed once (`referred_by` must be NULL).
- Only within first 30 days of account creation.
- Same validation as registration.

### Database

```sql
ALTER TABLE merchants ADD COLUMN referred_by TEXT;           -- referrer id
ALTER TABLE merchants ADD COLUMN referred_by_type TEXT;      -- 'merchant' | 'referrer'
ALTER TABLE merchants ADD COLUMN referral_fee_rate REAL;     -- NULL = default, 0.005 = discounted
ALTER TABLE merchants ADD COLUMN referral_fee_expires TEXT;  -- ISO 8601 expiry for discount period
```

---

## 4. Earnings Tracking

### When earnings accrue

On every confirmed invoice for a referred merchant, the scanner calculates:

```
referral_commission = invoice_fee_zats * 0.5   // 50% of CipherPay's fee goes to referrer
```

This is NOT additional cost to the merchant — it comes out of CipherPay's fee.

### Ledger table

```sql
CREATE TABLE referral_earnings (
    id TEXT PRIMARY KEY,
    referrer_id TEXT NOT NULL,
    referrer_type TEXT NOT NULL,              -- 'merchant' | 'referrer'
    referred_merchant_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL UNIQUE,          -- one entry per invoice, idempotent
    fee_zats INTEGER NOT NULL,               -- total fee charged on the invoice
    commission_zats INTEGER NOT NULL,         -- referrer's share
    created_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
```

### Integration point

In the billing/fee logic (after invoice confirmation):
1. Check if the merchant has a `referred_by`.
2. Check if referral is still within the 12-month earning window.
3. Calculate commission and insert into `referral_earnings`.
4. For merchant referrers: increment `referral_earnings_zats` on the merchant record and deduct from next billing cycle.
5. For non-merchant referrers: increment `total_earned_zats` on the referrer record.

---

## 5. Payout Model

### Path 1: Merchant referrers (fee credit)

- Commission is deducted from the referrer-merchant's owed fees.
- If owed fees < commission, credit carries forward.
- Pure ledger — no on-chain transaction, no wallet needed.
- Available immediately (no threshold).

### Path 2: Non-merchant referrers (operational wallet)

- Earnings accumulate in `referrers.total_earned_zats`.
- Monthly payout job checks all referrers where `(total_earned_zats - total_paid_zats) >= threshold`.
- Sends ZEC via operational wallet (`zipher-cli`) to `payout_address`.
- Updates `total_paid_zats` after confirmed send.
- **Requires:** Phase 5 operational wallet. Until then, earnings tracked but not paid out.

### Path 3: ZIP 321 real-time split (future)

- 3-output payment URI: merchant + CipherPay fee + referrer commission.
- Requires Phase 3.6 wallet testing (3-output URI support in Zashi/YWallet).
- Most elegant solution but depends on wallet ecosystem.

---

## 6. Anti-Gaming Rules

| Rule | Purpose |
|---|---|
| Self-referral blocked | Code owner's UIVK/address cannot match the new merchant |
| 7-day account age to generate code | Prevent sybil accounts from immediately farming |
| 3 confirmed invoices to activate code | Ensure referrer is a real merchant, not just a signup |
| 0.5 ZEC minimum referred volume | Referrer only earns after referred merchant has real activity |
| 12-month earning window | Commissions expire after 1 year per referred merchant |
| One referral code per merchant | Can't stack multiple codes |
| 30-day retroactive window | Existing merchants can only apply codes early in their lifecycle |

---

## 7. API Endpoints

### Merchant referral

| Method | Path | Auth | Description |
|---|---|---|---|
| POST | `/api/account/referral-code` | Dashboard/API key | Generate or retrieve referral code |
| GET | `/api/account/referrals` | Dashboard/API key | List referred merchants + earnings |

### Non-merchant referrer

| Method | Path | Auth | Description |
|---|---|---|---|
| POST | `/api/referrers/register` | None | Create referrer account (returns api_key) |
| GET | `/api/referrers/stats` | Referrer API key | View earnings, referred merchants, payout history |

### Registration

| Method | Path | Change |
|---|---|---|
| POST | `/api/merchants` | Add optional `referral_code` field |
| PATCH | `/api/account` | Add optional `referral_code` field (retroactive, one-time) |

---

## 8. Dashboard UI

### Referral tab (merchant dashboard)

- Generate/view referral code with copy button
- Shareable referral link: `https://cipherpay.app/ref/{code}`
- Table: referred merchants (name, signup date, volume, your earnings)
- Total earned / total credited
- Status badge: "Active" / "Pending" (waiting for 3 invoices)

### Referrer portal (non-merchant, minimal)

- Separate login at `/referrers` with API key
- Or: lightweight read-only page, no full dashboard needed
- Shows: code, referred merchants count, total earned, total paid, next payout estimate

---

## 9. Implementation Order

1. **Schema + merchant referral code generation** (half day)
   - Add columns to `merchants`, create `referral_earnings` table
   - `POST /api/account/referral-code` endpoint

2. **Referral code on registration** (half day)
   - Optional `referral_code` in `CreateMerchantRequest`
   - Validation, self-referral check, `referred_by` storage
   - Retroactive `PATCH /api/account`

3. **Earnings tracking in scanner** (1–2 days)
   - Hook into billing/fee logic after invoice confirmation
   - Commission calculation + ledger insert
   - Fee credit for merchant referrers

4. **Non-merchant referrer accounts** (1 day)
   - `referrers` table + register endpoint
   - Referrer API key (read-only stats)

5. **Dashboard referral tab** (1–2 days)
   - Code display, referral stats, earnings table
   - Frontend in cipherpay-web

6. **Operational wallet payouts** (depends on Phase 5)
   - Monthly payout job
   - `zipher-cli` integration for sends

---

## 10. Open Questions

- **Custom referral codes** — allow vanity codes (e.g. `CPREF-ZCASH`)? If so, need a blocklist for offensive/misleading terms.
- **Referral landing page** — should `cipherpay.app/ref/{code}` be a dedicated page explaining CipherPay with the code pre-filled? Or just redirect to signup with `?ref=` param?
- **Commission rate** — 0.5% of referred volume (50% of CipherPay's 1% fee) is the current plan. Should this be configurable per partnership?
- **Tiered referrals** — should high-volume referrers (10+ merchants) get a higher commission rate?
- **Non-merchant referrer verification** — any KYC/verification for non-merchant referrers before payout? Or is the threshold + Zcash address sufficient?

# CipherPay Referral Program

**Private draft -- not for public distribution**

---

## What is it?

A referral program that rewards people who bring new merchants to CipherPay. Referrers earn a percentage of the payment volume processed by the merchants they refer. Referred merchants get a reduced fee to start.

---

## Who can refer?

### Existing merchants

Any CipherPay merchant can generate a referral code from their dashboard and share it. Their earnings are credited against their own CipherPay fees.

### Advocates (non-merchants)

Community members, influencers, or Zcash enthusiasts who don't process payments themselves. They register with a Zcash payout address and receive their earnings directly in ZEC.

Non-merchant referrers are invite-only during the initial rollout.

---

## What does the referrer get?

- **0.5% of referred merchant's payment volume** for **12 months**
- Earnings are calculated on every confirmed payment the referred merchant receives
- **Choose your payout method:**
  - **Fee credit** (default) -- earnings are deducted from your CipherPay fees
  - **ZEC payout** -- earnings are paid out directly in ZEC to a wallet address you provide
- Minimum payout threshold for ZEC payouts: 0.5 ZEC

### Example

Bob refers Ledger. Ledger processes 100 ZEC in payments over 12 months.

Bob earns 0.5% of 100 ZEC = **0.50 ZEC** total over the year.

---

## What does the referred merchant get?

- **Half-price fees for the first 3 months**: 0.5% instead of the standard 1%
- After 3 months, the standard 1% fee applies
- The discount is automatic -- applied at signup when the referral code is used

### Existing merchants

Already using CipherPay but don't have a referral code? You can apply one at any time. You get the full discount (0.5% for 3 months from the date you apply it), and the referrer earns their commission for 12 months.

One code per merchant, one time, no exceptions.

### Example

Ledger uses Bob's referral code when signing up.

- Months 1--3: Ledger pays **0.5%** on every payment received
- Month 4 onward: Ledger pays the standard **1%**

---

## How it works

1. **Referrer generates a code** -- from the dashboard (merchants) or on registration (advocates)
2. **New merchant signs up with the code** -- entered during registration
3. **Discount activates immediately** -- 0.5% fee for 3 months
4. **Commissions accrue on every payment** -- 0.5% of volume for 12 months
5. **Earnings are credited or paid out** -- fee credits for merchants, ZEC payouts for advocates

Referral codes look like `CPREF-A3K9M2X7`. Custom vanity codes (e.g. `CPREF-ZCASH`) are available on request.

Referrers get a shareable link: `cipherpay.app/ref/{code}`

---

## Who pays for this?

The referrer's commission comes from CipherPay's fee -- not from the merchant. Merchants never pay more than the standard 1%.

During the 3-month discount period, CipherPay absorbs the full cost as a merchant acquisition investment.

| Period | Merchant pays | Referrer earns | CipherPay keeps |
|---|---|---|---|
| Months 1--3 | 0.5% | 0.5% | 0% |
| Months 4--12 | 1.0% | 0.5% | 0.5% |
| After month 12 | 1.0% | 0% | 1.0% |

---

## When do commissions activate?

To prevent gaming, commissions become payable only after the referred merchant shows real activity:

- At least **3 confirmed payments** received
- Activity spread over at least **14 days**
- At least **0.5 ZEC in fees** generated

Until these conditions are met, commissions are tracked but not payable.

---

## Fair use rules

- **No self-referrals** -- you cannot refer your own account
- **One code per merchant** -- apply once, anytime, full discount
- **Existing merchants** can apply a referral code at any time and get the full 3-month discount
- **12-month limit** -- commissions expire after one year per referred merchant
- **Minimum activity to refer** -- merchants need at least 7 days of account history and 3 confirmed payments before they can generate a referral code

---

## Tracking and transparency

**For merchant referrers:**
- Referral tab in the dashboard
- See referred merchants, their status, volume, and your earnings
- Total earned, total credited to fees or paid out in ZEC
- Switch between fee credit and ZEC payout at any time

**For non-merchant referrers:**
- Lightweight portal with API key login
- See referral count, total earned, total paid, and pending balance
- Payout history

---

## Questions?

This is an early draft -- we're looking for feedback before building it. Let us know what you think.

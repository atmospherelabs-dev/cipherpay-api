# AI Agents & Zcash Payments

## The Big Question

Will AI use crypto?

Say an instance of ChatGPT or Claude wants more compute, or needs to call a paid API, or wants to buy training data. How does it pay?

- **Credit card?** — It has no name, no address, no bank account. It can't do KYC.
- **Wire transfer?** — Same problem. Traditional finance requires a legal identity.
- **Crypto?** — Digital, permissionless, no identity needed. The AI signs a transaction and pays. Done.

Crypto is the natural payment rail for AI. But there's a catch: the most likely path is the **agent model** — every AI operates on behalf of a specific human. The human funds the wallet, sets spending limits, and is legally responsible. The AI is a delegate, not an independent actor.

This is already happening. Coinbase launched "Agentic Wallets" in 2025. Stripe, Visa, and PayPal followed. AI agents are buying compute, booking flights, paying for API access — autonomously, with crypto.

But every one of these systems uses **transparent** blockchains. Every payment is public. Which brings us to Zcash.

---

## What is x402?

When you visit a web page, your browser sends an HTTP request. The server responds with a status code:

- `200` = "Here's your page"
- `404` = "Page not found"
- `401` = "You need to log in"
- **`402`** = "Payment required"

Status code `402` has existed since 1997 but was never used — there was no internet-native money. **x402** is an open protocol (built by Coinbase, co-sponsored by Cloudflare) that makes `402` work. When a server responds with `402`, it includes payment instructions. The client pays, resends the request with proof, and gets the resource.

### A real-world example

An AI agent needs weather data from a paid API:

```
Step 1: Agent requests data
─────────────────────────────────────────────
  Agent → API:  GET /api/weather/paris

Step 2: API says "pay me first"
─────────────────────────────────────────────
  API → Agent:  402 Payment Required
                {
                  "amount": "0.001",
                  "token": "ZEC",
                  "network": "zcash:mainnet",
                  "address": "u1abc..."
                }

Step 3: Agent pays and retries
─────────────────────────────────────────────
  Agent broadcasts a shielded ZEC transaction,
  then retries with the transaction ID:

  Agent → API:  GET /api/weather/paris
                X-PAYMENT: txid=7f3a9b...

Step 4: API verifies and delivers
─────────────────────────────────────────────
  API asks its facilitator: "Did txid 7f3a9b...
  pay 0.001 ZEC to my address?"

  Facilitator: "Yes."

  API → Agent:  200 OK
                { "temperature": 18, "conditions": "partly cloudy" }
```

No account creation, no API key, no subscription. Request, pay, receive.

The server **doesn't know or care** if the client is a human or an AI. The protocol is the same. A browser and a bot send the same HTTP requests.

### Who's doing this already

- **Coinbase** launched x402 in May 2025
- **Cloudflare** co-founded the x402 Foundation
- **Google Cloud** integrated x402 into their Agent Payments Protocol
- Over **100 million payments** processed by early 2026
- Currently supports USDC on Base, Solana, Polygon, Avalanche

**Zcash is not supported yet.** That's the opportunity.

---

## What is a Facilitator?

The facilitator answers one question: **"Did this payment actually happen?"**

A resource server (API) that wants to accept payments needs to verify that clients actually paid. On transparent chains (Base, Solana), this is easy — look at the blockchain, see the transfer. On Zcash, transactions are encrypted — you need trial decryption.

Most API developers don't want to run blockchain nodes, understand viewing keys, or implement trial decryption. The facilitator abstracts all of that into one API call.

### Existing facilitators (other chains)

All on transparent chains. None for Zcash.

| Facilitator | Chains | Pricing |
|---|---|---|
| **Coinbase** | Base, Solana | Free (zero facilitator fee) |
| **OpenFacilitator** | EVM, Solana | Free tier + $5/mo for custom domain |
| **Polygon** | Polygon | Free |
| **Thirdweb** | EVM | API key required |
| **AceDataCloud** | EVM, self-hostable | Free |
| **PayAI** | EVM | Free |

All are **non-custodial** — they never touch funds. They just verify payments and tell the resource server "yes, you got paid."

---

## CipherPay's Role: The Zcash Facilitator

CipherPay's job is simple: **be the Zcash facilitator for x402.**

When a resource server (API) wants to accept ZEC payments, it registers with CipherPay and provides its **viewing key** (UFVK). When a client pays, CipherPay trial-decrypts the transaction and confirms the payment. Non-custodial — CipherPay never holds funds.

### How it works

1. Resource server registers with CipherPay — same as a merchant today. Gives us their viewing key.
2. Client (human or agent) sends shielded ZEC to the server's address.
3. Client retries the request with the transaction ID.
4. Resource server calls CipherPay:

```
POST https://api.cipherpay.app/x402/verify
Authorization: Bearer cpay_sk_...
{
  "txid": "7f3a9b...",
  "expected_amount": "0.001",
  "expected_token": "ZEC"
}
```

5. CipherPay trial-decrypts the transaction using the server's viewing key.
6. If it decrypts and the amount matches → `{ "valid": true }`
7. Resource server delivers the data.

### Whose viewing key?

The **recipient's** (resource server's). Not the sender's.

This is the same principle as CipherPay's existing merchant flow. When a customer pays a CipherPay merchant, CipherPay uses the merchant's UFVK to see incoming payments. The facilitator does exactly the same thing — uses the server's viewing key to verify that a specific transaction paid the right amount.

The sender (agent) reveals nothing. No viewing key, no identity, no balance.

### Why not check themselves?

The resource server COULD verify payments on their own — if they run Zcash infrastructure, implement trial decryption, scan the mempool, etc. Some will. But most API developers don't want to deal with that. CipherPay makes it one API call. Same reason Coinbase's facilitator exists on Base — API devs don't want to run Ethereum nodes either.

### Speed

On EVM, verification is instant (off-chain signature check). On Zcash, the transaction needs to hit the mempool first — the facilitator can't trial-decrypt it until then. This takes **5-10 seconds**.

Fine for most AI use cases (compute, data, inference). Too slow for real-time streaming micropayments — those stay on transparent chains.

### Account type

The resource server registers with CipherPay the same way a merchant does today — viewing key + API key. There's no separate "facilitator account" or "agent account." It's the same registration.

The difference is what endpoints they use:
- **Merchants** use invoices, products, POS, checkout pages, webhooks
- **x402 resource servers** use `POST /x402/verify` — a one-shot verification

Both can coexist on the same account. A store could accept CipherPay invoices for its website AND accept x402 payments for its API — same viewing key, same account.

CipherPay logs each successful verification as a payment record, so the resource server has a history of who paid what (without knowing WHO — just amounts, timestamps, txids).

---

## Why Zcash Matters

Every x402 payment on Base, Solana, or Polygon is fully public. Anyone can see which APIs an agent is using, how often, and how much it's spending. If an AI acts on behalf of a human, the human's activity is transparent.

Zcash shielded payments make all of that invisible:

| What's visible | Transparent chains | Zcash shielded |
|---|---|---|
| Payment amount | Yes | No |
| Who paid | Yes | No |
| Who received | Yes | No |
| Frequency / patterns | Yes | No |
| Link to human owner | Possible | No |

**The pitch:**

> Coinbase Agentic Wallets = AI agents that can pay, but every payment is public.
>
> Zcash agent wallets = AI agents that can pay, and nobody knows what they're paying for.

---

## Agent Wallets

The agent wallet is **not a CipherPay thing.** It's a Zcash wallet that the agent controls. CipherPay is only on the receiving/verification side.

### How agents get a wallet

The human creates the agent wallet in **Zipher** — it's just another account.

1. Open Zipher → create a new account (this is the agent's wallet)
2. Fund it — send ZEC from your main account to the agent account
3. Export the spending key
4. Give the spending key to your AI agent

The agent now has a Zcash wallet it can use autonomously. It can sign and broadcast shielded transactions. It's always online. No CipherPay involvement.

### Spending limits

With a non-custodial wallet (agent holds its own key), there are no software-enforced per-transaction limits. The spending control is **how much ZEC you put in:**

| Method | How it works |
|---|---|
| **Balance cap** | Only put 0.5 ZEC in. When it's gone, agent stops. Top up when ready. |
| **Small top-ups** | Send 0.1 ZEC per day. Don't pre-fund large amounts. |
| **Revoke the key** | Change the spending key in Zipher. Agent's old key stops working. |

It's like giving someone cash. You limit how much you hand over, not how fast they spend it.

### Monitoring

Since the agent wallet is an account in Zipher, the human sees everything:
- Balance
- Transaction history
- Memos
- Real-time updates

No CipherPay needed for monitoring. It's just another account in your wallet.

### Security

If the agent is compromised, the attacker can drain whatever ZEC is in the agent wallet. They can't touch the human's main wallet — it's a separate account with a separate key.

The damage is always capped at whatever balance the agent wallet has. Don't pre-fund more than you're comfortable losing.

---

## Multichain Payments via NEAR Intents

If a resource server accepts ZEC → the agent pays directly in shielded ZEC. CipherPay verifies.

If a resource server accepts something else (USDC on Base, SOL, ETH) → the agent swaps ZEC to whatever the server wants using **NEAR Intents**, a cross-chain swap protocol.

**This doesn't need CipherPay.** The agent calls NEAR's public API directly. CipherPay is not involved in multichain swaps.

### How it works

The 402 response tells the agent what the server wants — including the chain and token, using CAIP-2 identifiers:

```json
{
  "amount": "0.50",
  "token": "USDC",
  "network": "eip155:8453",
  "address": "0xabc..."
}
```

`eip155:8453` = Base. `solana:mainnet` = Solana. `zcash:mainnet` = Zcash.

If the server wants USDC on Base and the agent has ZEC:

```
Agent                    NEAR Intents              Server (Base)
  │                          │                          │
  │  Swap ZEC → USDC         │                          │
  │  (Zcash tx: abc123)      │                          │
  │─────────────────────────>│                          │
  │                          │                          │
  │                          │  Send USDC to server     │
  │                          │  (Base tx: def456)       │
  │                          │─────────────────────────>│
  │                          │                          │
  │  Here's Base tx: def456  │                          │
  │<─────────────────────────│                          │
  │                          │                          │
  │  GET /data               │                          │
  │  X-PAYMENT: def456       │                          │
  │────────────────────────────────────────────────────>│
  │                          │                          │
  │                          │   Server verifies USDC   │
  │                          │   via Coinbase facilitator│
  │                          │   (NOT CipherPay)        │
  │                          │                          │
  │  200 OK + data           │                          │
  │<───────────────────────────────────────────────────│
```

Two transactions on two chains. The agent uses the Zcash one to swap. The server sees the Base one to verify. The server never knows ZEC was involved.

The server verifies USDC via its own facilitator (Coinbase, OpenFacilitator, etc.). CipherPay has nothing to do with it.

### Privacy tradeoff

When paying on Zcash → fully shielded, maximum privacy.

When swapping to another chain → the swap goes through a NEAR deposit address (transparent). The last-mile payment on Base/Solana is transparent. The agent's ZEC holdings and Zcash-side activity remain private, but the specific swap-out is visible.

### Zipher already has NEAR Intents

Zipher already supports cross-chain swaps via NEAR Intents (ZEC ↔ BTC, ETH, SOL, etc.). The same infrastructure can be used by agents.

---

## The Full Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  HUMAN (you, in Zipher)                                         │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  Zipher                                                  │   │
│  │                                                          │   │
│  │  ┌────────────────┐     ┌────────────────┐               │   │
│  │  │ Main Account   │     │ Agent Account  │               │   │
│  │  │ (your funds)   │────▶│ (agent's funds)│               │   │
│  │  │                │fund │                │               │   │
│  │  │                │     │ Export spending │               │   │
│  │  │                │     │ key → give to  │               │   │
│  │  │                │     │ your AI agent  │               │   │
│  │  └────────────────┘     └───────┬────────┘               │   │
│  │                                 │                        │   │
│  │  Monitor: see agent balance,    │ spending key           │   │
│  │  tx history, memos — all in     │                        │   │
│  │  Zipher like any account.       │                        │   │
│  └─────────────────────────────────┼────────────────────────┘   │
│                                    │                            │
├────────────────────────────────────┼────────────────────────────┤
│  AGENT (autonomous, always online) │                            │
│                                    ▼                            │
│  ┌──────────┐     ┌─────────────────────────────────────┐       │
│  │ AI Agent │────▶│  Agent's own wallet                 │       │
│  │ (Claude, │     │  (has spending key from Zipher)     │       │
│  │  GPT,    │     │                                     │       │
│  │  custom) │     │  Can:                               │       │
│  │          │     │  • Sign shielded Zcash transactions │       │
│  │          │     │  • Call NEAR Intents for swaps      │       │
│  │          │     │  • Pay any x402 server directly     │       │
│  └──────────┘     └──────────────┬──────────────────────┘       │
│                                  │                              │
├──────────────────────────────────┼──────────────────────────────┤
│  RESOURCE SERVER (the API being paid)                           │
│                                  │                              │
│         ┌────────────────────────┼───────────────┐              │
│         │                        ▼               │              │
│         │  If server accepts ZEC:                │              │
│         │  ┌──────────────┐     ┌────────────┐   │              │
│         │  │  CipherPay   │◀───▶│   Zcash    │   │              │
│         │  │  Facilitator │     │ blockchain │   │              │
│         │  │  (verifies   │     │            │   │              │
│         │  │   via IVK)   │     └────────────┘   │              │
│         │  └──────────────┘                      │              │
│         │                                        │              │
│         │  If server accepts USDC/ETH/SOL:       │              │
│         │  ┌──────────────┐     ┌────────────┐   │              │
│         │  │  Coinbase /  │◀───▶│ Base, Sol, │   │              │
│         │  │  other       │     │ ETH, etc.  │   │              │
│         │  │  facilitator │     │            │   │              │
│         │  └──────────────┘     └────────────┘   │              │
│         │                                        │              │
│         └────────────────────────────────────────┘              │
│                                                                 │
│  CipherPay is only needed when the server accepts ZEC.          │
│  For other chains, the server uses its own facilitator.         │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Existing Agent Wallet Solutions (Other Chains)

For context — here's what exists today. All EVM-only, none support Zcash:

| Solution | Model | How it works | Cost |
|---|---|---|---|
| **Coinbase Agentic Wallet** | Custodial | Keys in TEE. Agent gets API key. Spending limits + KYT. | $0.005/op |
| **Privy (Stripe)** | Custodial | Shamir + AWS Nitro enclaves. Stripe holds key shares. | Free < 500 users |
| **Agentokratia** | Non-custodial | 2-of-3 threshold ECDSA. Self-hosted. | Free (open source) |
| **MetaMask Server Wallets** | Semi-custodial | Keys in TEE. Separate agent key and owner key. | Free |
| **Veridex** (ZCG grant) | Non-custodial | WebAuthn passkeys → constrained session keys. Zcash planned. | Not launched |

Our approach (agent account in Zipher, non-custodial) is closest to Agentokratia — the agent holds its own key, the human controls funding.

---

## Monetization

### How facilitators make money today

Most are free or near-free. The market is in "land grab" mode. Coinbase charges zero. OpenFacilitator has a free tier with $5/mo premium.

### CipherPay's revenue from x402

**1. Facilitator verification fees**

Resource servers that want ZEC payment verification:
- **Free tier**: 1,000 verifications/month
- **Paid tier**: $5-50/month for higher volume, analytics, custom domain
- Optional per-verification fee at scale

**2. Existing CipherPay billing**

Resource servers registered as CipherPay merchants can use both the invoice flow AND the x402 facilitator flow. The existing percentage-fee billing model applies.

**3. Developer acquisition**

A server middleware SDK (`@cipherpay/x402`) — free, open source — that makes it one line of code to accept ZEC via x402. Every install routes verifications through CipherPay. More volume = more fees.

```typescript
import { cipherPayMiddleware } from '@cipherpay/x402';

app.use('/api/premium/*', cipherPayMiddleware({
  amount: '0.001',
  currency: 'ZEC',
  facilitator: 'https://api.cipherpay.app',
}));
```

---

## What We Build

### Phase 1: x402 Facilitator Endpoint

Add `POST /x402/verify` to CipherPay. Resource servers call it with a txid, expected amount, and expected token. CipherPay trial-decrypts using the server's viewing key and returns valid/invalid.

This reuses CipherPay's existing trial decryption engine and CipherScan integration. Minimal new code.

**Same account as merchants.** Register with a viewing key, get an API key. Use invoices, x402 verification, or both.

### Phase 2: Server Middleware SDK

`@cipherpay/x402` — TypeScript package. One line to add ZEC payments to any API. Handles the 402 response format, calls the facilitator, delivers the resource.

### Phase 3: Zipher Agent Account UX

Streamline the "create another account for your agent" flow in Zipher. Export spending key, fund from main account, monitor activity. This is mostly UX polish — Zipher already supports multi-account.

### Phase 4: MCP Payment Tool

Model Context Protocol tool so AI agents (Claude, ChatGPT) can make shielded Zcash payments as a native tool call.

---

## Roadmap

| Phase | What | Effort | Impact |
|-------|------|--------|--------|
| **1** | x402 Facilitator endpoint | ~2-3 weeks | ZEC in the x402 ecosystem |
| **2** | Server middleware SDK | ~2-3 weeks | One-line ZEC integration for devs |
| **3** | Zipher agent account UX | ~1-2 weeks | Smooth agent wallet creation |
| **4** | MCP Payment Tool | ~1-2 weeks | Native AI tool for ZEC payments |

---

## Related Work

**Veridex Protocol** — ZCG grant applicant (Feb 2026) building passkey-based Zcash wallets and AI agent session keys. Their approach: derive constrained spending keys from a WebAuthn passkey. Complementary — they focus on key management, we focus on payment verification infrastructure.

---

## Open Questions

- **ZEC volatility.** x402 uses USDC (stable). For larger payments, do we need a ZEC stablecoin (future ZSA)?
- **Facilitator speed.** 5-10 seconds for mempool detection. Can we make it faster?
- **Session keys.** Could Zcash support constrained spending keys (with built-in limits) at the protocol level? Would eliminate the non-custodial spending limit problem.
- **Legal.** Is a Zcash facilitator a money transmitter? Probably not (non-custodial, verification-only), but needs legal review.
- **402 adoption.** Services need to respond with 402 for this to work. Adoption is growing fast (100M+ payments, Coinbase/Cloudflare/Google behind it), but still early.

---

## References

- [x402 Protocol](https://x402.org/) — official site and spec
- [x402 GitHub](https://github.com/coinbase/x402) — open source, Apache 2.0
- [x402 Coinbase Docs](https://docs.cdp.coinbase.com/x402/welcome) — developer documentation
- [x402 Ecosystem / Facilitators](https://www.x402.org/ecosystem?category=facilitators) — existing facilitators
- [x402 V2 Launch](https://www.x402.org/writing/x402-v2-launch) — modular architecture
- [Coinbase Agentic Wallets](https://www.coinbase.com/en-gb/developer-platform/products/agentic-wallets) — custodial agent wallets
- [Agentokratia](https://agentokratia.com/blog/wallet-comparison) — non-custodial agent wallet comparison
- [NEAR Intents](https://docs.near.org/chain-abstraction/intents/overview) — cross-chain swaps
- [NEAR 1Click API](https://docs.near-intents.org/near-intents/integration/distribution-channels/1click-api) — programmatic swap API
- [Veridex ZCG Grant](https://github.com/ZcashCommunityGrants) — passkey wallets + agent session keys

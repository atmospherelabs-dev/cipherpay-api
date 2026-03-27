#!/usr/bin/env bash
# Seeds mock events, invoices, and tickets into the local DB for UI testing.
# Usage:  ./scripts/seed_mock_events.sh          (seed)
#         ./scripts/seed_mock_events.sh --clean   (remove seeded data)
set -euo pipefail

DB="${1:-cipherpay.db}"
MERCHANT="a3567834-b9dc-4acb-b379-bd506f976060"

# Deterministic IDs so we can clean up reliably
PROD_SOLDOUT="mock-prod-soldout"
PROD_PAST="mock-prod-past"
PROD_CANCEL="mock-prod-cancel"

EVT_SOLDOUT="mock-evt-soldout"
EVT_PAST="mock-evt-past"
EVT_CANCEL="mock-evt-cancel"

PRC_SOLDOUT_GA="mock-prc-soldout-ga"
PRC_SOLDOUT_VIP="mock-prc-soldout-vip"
PRC_PAST_GA="mock-prc-past-ga"
PRC_CANCEL_GA="mock-prc-cancel-ga"
PRC_CANCEL_VIP="mock-prc-cancel-vip"

ADDR="utest1mockaddress000000000000000000000000000000000000000000000000000000000000000000000000000000"
RECV="deadbeefcafe0000000000000000000000000000000000000000000000000000000000000000000000000000"

if [ "${1:-}" = "--clean" ]; then
  echo "Cleaning mock data..."
  sqlite3 "$DB" <<'SQL'
DELETE FROM tickets  WHERE id LIKE 'mock-tkt-%';
DELETE FROM invoices WHERE id LIKE 'mock-inv-%';
DELETE FROM prices   WHERE id LIKE 'mock-prc-%';
DELETE FROM events   WHERE id LIKE 'mock-evt-%';
DELETE FROM products WHERE id LIKE 'mock-prod-%';
SQL
  echo "Done."
  exit 0
fi

echo "Seeding mock events for merchant $MERCHANT ..."

sqlite3 "$DB" <<SQL

-- ═══════════════════════════════════════════════════════════════
-- 1. SOLD-OUT EVENT  ─  "Privacy Masterclass" (capacity 3 GA + 2 VIP, all sold)
-- ═══════════════════════════════════════════════════════════════

INSERT OR REPLACE INTO products (id, merchant_id, slug, name, active)
VALUES ('$PROD_SOLDOUT', '$MERCHANT', 'privacy-masterclass', 'Privacy Masterclass', 1);

INSERT OR REPLACE INTO events (id, merchant_id, product_id, title, description, event_date, event_location, status)
VALUES ('$EVT_SOLDOUT', '$MERCHANT', '$PROD_SOLDOUT',
        'Privacy Masterclass', 'Hands-on workshop covering CoinJoin, zk proofs, and metadata resistance.',
        '2026-05-15T14:00:00Z', 'Berlin, Germany', 'active');

INSERT OR REPLACE INTO prices (id, product_id, currency, unit_amount, label, max_quantity, active)
VALUES ('$PRC_SOLDOUT_GA', '$PROD_SOLDOUT', 'EUR', 15.0, 'General Admission', 3, 1);

INSERT OR REPLACE INTO prices (id, product_id, currency, unit_amount, label, max_quantity, active)
VALUES ('$PRC_SOLDOUT_VIP', '$PROD_SOLDOUT', 'EUR', 40.0, 'VIP', 2, 1);

-- 3 GA confirmed invoices
INSERT OR REPLACE INTO invoices (id, merchant_id, memo_code, product_id, product_name, price_eur, price_zec, zec_rate_at_creation, payment_address, status, expires_at, diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis, price_id, confirmed_at)
VALUES
  ('mock-inv-so-ga1', '$MERCHANT', 'CP-MOCK-SO1', '$PROD_SOLDOUT', 'Privacy Masterclass', 15.0, 0.3, 50.0, '$ADDR', 'confirmed', '2026-04-01T00:00:00Z', 1001, '$RECV', 30000000, 30000000, '$PRC_SOLDOUT_GA', '2026-03-10T10:00:00Z'),
  ('mock-inv-so-ga2', '$MERCHANT', 'CP-MOCK-SO2', '$PROD_SOLDOUT', 'Privacy Masterclass', 15.0, 0.3, 50.0, '$ADDR', 'confirmed', '2026-04-01T00:00:00Z', 1002, '$RECV', 30000000, 30000000, '$PRC_SOLDOUT_GA', '2026-03-11T10:00:00Z'),
  ('mock-inv-so-ga3', '$MERCHANT', 'CP-MOCK-SO3', '$PROD_SOLDOUT', 'Privacy Masterclass', 15.0, 0.3, 50.0, '$ADDR', 'confirmed', '2026-04-01T00:00:00Z', 1003, '$RECV', 30000000, 30000000, '$PRC_SOLDOUT_GA', '2026-03-12T10:00:00Z');

-- 2 VIP confirmed invoices
INSERT OR REPLACE INTO invoices (id, merchant_id, memo_code, product_id, product_name, price_eur, price_zec, zec_rate_at_creation, payment_address, status, expires_at, diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis, price_id, confirmed_at)
VALUES
  ('mock-inv-so-vip1', '$MERCHANT', 'CP-MOCK-SO4', '$PROD_SOLDOUT', 'Privacy Masterclass', 40.0, 0.8, 50.0, '$ADDR', 'confirmed', '2026-04-01T00:00:00Z', 1004, '$RECV', 80000000, 80000000, '$PRC_SOLDOUT_VIP', '2026-03-13T10:00:00Z'),
  ('mock-inv-so-vip2', '$MERCHANT', 'CP-MOCK-SO5', '$PROD_SOLDOUT', 'Privacy Masterclass', 40.0, 0.8, 50.0, '$ADDR', 'confirmed', '2026-04-01T00:00:00Z', 1005, '$RECV', 80000000, 80000000, '$PRC_SOLDOUT_VIP', '2026-03-14T10:00:00Z');

-- Tickets for all 5 sold
INSERT OR REPLACE INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status)
VALUES
  ('mock-tkt-so1', 'mock-inv-so-ga1', '$PROD_SOLDOUT', '$PRC_SOLDOUT_GA', '$MERCHANT', 'tkt_mock_so_ga_001', 'valid'),
  ('mock-tkt-so2', 'mock-inv-so-ga2', '$PROD_SOLDOUT', '$PRC_SOLDOUT_GA', '$MERCHANT', 'tkt_mock_so_ga_002', 'valid'),
  ('mock-tkt-so3', 'mock-inv-so-ga3', '$PROD_SOLDOUT', '$PRC_SOLDOUT_GA', '$MERCHANT', 'tkt_mock_so_ga_003', 'valid'),
  ('mock-tkt-so4', 'mock-inv-so-vip1', '$PROD_SOLDOUT', '$PRC_SOLDOUT_VIP', '$MERCHANT', 'tkt_mock_so_vip_001', 'valid'),
  ('mock-tkt-so5', 'mock-inv-so-vip2', '$PROD_SOLDOUT', '$PRC_SOLDOUT_VIP', '$MERCHANT', 'tkt_mock_so_vip_002', 'used');


-- ═══════════════════════════════════════════════════════════════
-- 2. PAST EVENT  ─  "Zcash Dev Summit 2025" (event date in the past)
-- ═══════════════════════════════════════════════════════════════

INSERT OR REPLACE INTO products (id, merchant_id, slug, name, active)
VALUES ('$PROD_PAST', '$MERCHANT', 'zcash-dev-summit-2025', 'Zcash Dev Summit 2025', 0);

INSERT OR REPLACE INTO events (id, merchant_id, product_id, title, description, event_date, event_location, status)
VALUES ('$EVT_PAST', '$MERCHANT', '$PROD_PAST',
        'Zcash Dev Summit 2025', 'Annual developer summit. Zebra, librustzcash, wallet SDK deep dives.',
        '2025-11-20T09:00:00Z', 'Denver, CO', 'past');

INSERT OR REPLACE INTO prices (id, product_id, currency, unit_amount, label, max_quantity, active)
VALUES ('$PRC_PAST_GA', '$PROD_PAST', 'USD', 50.0, 'General Admission', 30, 0);

-- 8 confirmed invoices (historical attendance)
INSERT OR REPLACE INTO invoices (id, merchant_id, memo_code, product_id, product_name, price_eur, price_zec, zec_rate_at_creation, payment_address, status, expires_at, diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis, price_id, confirmed_at)
VALUES
  ('mock-inv-past1', '$MERCHANT', 'CP-MOCK-P1', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1010, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-15T10:00:00Z'),
  ('mock-inv-past2', '$MERCHANT', 'CP-MOCK-P2', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1011, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-16T10:00:00Z'),
  ('mock-inv-past3', '$MERCHANT', 'CP-MOCK-P3', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1012, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-17T10:00:00Z'),
  ('mock-inv-past4', '$MERCHANT', 'CP-MOCK-P4', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1013, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-18T10:00:00Z'),
  ('mock-inv-past5', '$MERCHANT', 'CP-MOCK-P5', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1014, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-19T10:00:00Z'),
  ('mock-inv-past6', '$MERCHANT', 'CP-MOCK-P6', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1015, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-20T10:00:00Z'),
  ('mock-inv-past7', '$MERCHANT', 'CP-MOCK-P7', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1016, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-21T10:00:00Z'),
  ('mock-inv-past8', '$MERCHANT', 'CP-MOCK-P8', '$PROD_PAST', 'Zcash Dev Summit 2025', 45.0, 1.0, 45.0, '$ADDR', 'confirmed', '2025-10-01T00:00:00Z', 1017, '$RECV', 100000000, 100000000, '$PRC_PAST_GA', '2025-09-22T10:00:00Z');

-- All 8 tickets used (past event, everyone checked in)
INSERT OR REPLACE INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status, used_at)
VALUES
  ('mock-tkt-past1', 'mock-inv-past1', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_001', 'used', '2025-11-20T09:15:00Z'),
  ('mock-tkt-past2', 'mock-inv-past2', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_002', 'used', '2025-11-20T09:20:00Z'),
  ('mock-tkt-past3', 'mock-inv-past3', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_003', 'used', '2025-11-20T09:25:00Z'),
  ('mock-tkt-past4', 'mock-inv-past4', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_004', 'used', '2025-11-20T09:30:00Z'),
  ('mock-tkt-past5', 'mock-inv-past5', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_005', 'used', '2025-11-20T09:35:00Z'),
  ('mock-tkt-past6', 'mock-inv-past6', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_006', 'used', '2025-11-20T09:40:00Z'),
  ('mock-tkt-past7', 'mock-inv-past7', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_007', 'used', '2025-11-20T09:45:00Z'),
  ('mock-tkt-past8', 'mock-inv-past8', '$PROD_PAST', '$PRC_PAST_GA', '$MERCHANT', 'tkt_mock_past_008', 'used', '2025-11-20T09:50:00Z');


-- ═══════════════════════════════════════════════════════════════
-- 3. CANCELLED EVENT  ─  "Crypto Privacy Workshop" (refund queue scenario)
-- ═══════════════════════════════════════════════════════════════

INSERT OR REPLACE INTO products (id, merchant_id, slug, name, active)
VALUES ('$PROD_CANCEL', '$MERCHANT', 'crypto-privacy-workshop', 'Crypto Privacy Workshop', 0);

INSERT OR REPLACE INTO events (id, merchant_id, product_id, title, description, event_date, event_location, status)
VALUES ('$EVT_CANCEL', '$MERCHANT', '$PROD_CANCEL',
        'Crypto Privacy Workshop', 'Cancelled due to venue issues.',
        '2026-06-10T18:00:00Z', 'Lisbon, Portugal', 'cancelled');

INSERT OR REPLACE INTO prices (id, product_id, currency, unit_amount, label, max_quantity, active)
VALUES ('$PRC_CANCEL_GA', '$PROD_CANCEL', 'EUR', 20.0, 'General Admission', 15, 0);

INSERT OR REPLACE INTO prices (id, product_id, currency, unit_amount, label, max_quantity, active)
VALUES ('$PRC_CANCEL_VIP', '$PROD_CANCEL', 'EUR', 55.0, 'VIP', 5, 0);

-- 4 confirmed invoices: 2 have refund_address (refund queue), 1 already refunded, 1 without refund address
INSERT OR REPLACE INTO invoices (id, merchant_id, memo_code, product_id, product_name, price_eur, price_zec, zec_rate_at_creation, payment_address, status, expires_at, diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis, price_id, confirmed_at, refund_address)
VALUES
  ('mock-inv-cx-ga1', '$MERCHANT', 'CP-MOCK-CX1', '$PROD_CANCEL', 'Crypto Privacy Workshop', 20.0, 0.4, 50.0, '$ADDR', 'confirmed', '2026-05-01T00:00:00Z', 1020, '$RECV', 40000000, 40000000, '$PRC_CANCEL_GA', '2026-04-10T10:00:00Z', 'u1refund_alice_0000000000000000000000000000000000000000000000000000000000000000000000000000000000'),
  ('mock-inv-cx-ga2', '$MERCHANT', 'CP-MOCK-CX2', '$PROD_CANCEL', 'Crypto Privacy Workshop', 20.0, 0.4, 50.0, '$ADDR', 'confirmed', '2026-05-01T00:00:00Z', 1021, '$RECV', 40000000, 40000000, '$PRC_CANCEL_GA', '2026-04-11T10:00:00Z', 'u1refund_bob_000000000000000000000000000000000000000000000000000000000000000000000000000000000000'),
  ('mock-inv-cx-vip1', '$MERCHANT', 'CP-MOCK-CX3', '$PROD_CANCEL', 'Crypto Privacy Workshop', 55.0, 1.1, 50.0, '$ADDR', 'refunded', '2026-05-01T00:00:00Z', 1022, '$RECV', 110000000, 110000000, '$PRC_CANCEL_VIP', '2026-04-12T10:00:00Z', 'u1refund_carol_00000000000000000000000000000000000000000000000000000000000000000000000000000000000'),
  ('mock-inv-cx-ga3', '$MERCHANT', 'CP-MOCK-CX4', '$PROD_CANCEL', 'Crypto Privacy Workshop', 20.0, 0.4, 50.0, '$ADDR', 'confirmed', '2026-05-01T00:00:00Z', 1023, '$RECV', 40000000, 40000000, '$PRC_CANCEL_GA', '2026-04-13T10:00:00Z', NULL);

-- All tickets voided (event was cancelled)
INSERT OR REPLACE INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status)
VALUES
  ('mock-tkt-cx1', 'mock-inv-cx-ga1', '$PROD_CANCEL', '$PRC_CANCEL_GA', '$MERCHANT', 'tkt_mock_cx_001', 'void'),
  ('mock-tkt-cx2', 'mock-inv-cx-ga2', '$PROD_CANCEL', '$PRC_CANCEL_GA', '$MERCHANT', 'tkt_mock_cx_002', 'void'),
  ('mock-tkt-cx3', 'mock-inv-cx-vip1', '$PROD_CANCEL', '$PRC_CANCEL_VIP', '$MERCHANT', 'tkt_mock_cx_003', 'void'),
  ('mock-tkt-cx4', 'mock-inv-cx-ga3', '$PROD_CANCEL', '$PRC_CANCEL_GA', '$MERCHANT', 'tkt_mock_cx_004', 'void');

SQL

echo ""
echo "Seeded:"
echo "  3 events  (sold-out, past, cancelled)"
echo "  5 products/prices"
echo "  17 invoices"
echo "  17 tickets"
echo ""
echo "To clean up:  ./scripts/seed_mock_events.sh --clean"

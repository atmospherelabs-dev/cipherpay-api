#!/usr/bin/env bash
set -euo pipefail

BASE="http://localhost:3080"
DB="cipherpay.db"
PASS=0
FAIL=0

UFVK="uviewtest17xls3f7c3zxg8cancv5v7hxztmhcxzr9lc85h7sg0fq73xunjy4j9yctfm406xn5rczqr4mq2rqkl7trgc2g8d235p6f6kvzz4xjuwzx029cz3gl75xy2r4v5k7javru7p35jda9z20xjf49h2dwdvl22cure5haf0vkj5xm87cnyszdjpg3wv6msn75570gadfmwa54yce6q5a3wq4m40dak7tfqr89dlxqt525kneufc95yy0lal5hz4h52entf9gks0f3ejaayeh500lp3q0u2ftfxzw9g265dtpm4vt5p8uqsgcnmnxtvmltwmj02hznq5v9mykdgpvg7r3u8uxpa2mpqdxphphnh83jdqfu2ryej5v2qq4d6lwjw0elax5lyvqqzw9ghavjaqz64vhz7knm7jxrfeczcafd8zwyc7dh583juwex2r6z7mc2plcvr3nujp2djvggfqfsxlf93f7z666xa8hx2qldavhpasgl3vme3dcm"

green() { printf "\033[32m%s\033[0m\n" "$*"; }
red()   { printf "\033[31m%s\033[0m\n" "$*"; }

assert_eq() {
  local label="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    green "  PASS: $label"
    PASS=$((PASS + 1))
  else
    red "  FAIL: $label (expected='$expected', got='$actual')"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_empty() {
  local label="$1" actual="$2"
  if [ -n "$actual" ] && [ "$actual" != "null" ]; then
    green "  PASS: $label"
    PASS=$((PASS + 1))
  else
    red "  FAIL: $label (was empty or null)"
    FAIL=$((FAIL + 1))
  fi
}

echo ""
echo "============================================"
echo " CipherPay Events & Tickets — API Test Suite"
echo "============================================"
echo ""

# ── 1. Health check ──────────────────────────────────────────────────────────
echo "1. Health check"
HEALTH=$(curl -s "$BASE/api/health")
assert_eq "server is up" "ok" "$(echo "$HEALTH" | jq -r '.status')"

# ── 2. Register merchant ────────────────────────────────────────────────────
echo "2. Register merchant"
REG=$(curl -s -X POST "$BASE/api/merchants" \
  -H "Content-Type: application/json" \
  -d "{\"name\":\"Test Events Merchant\",\"ufvk\":\"$UFVK\"}")

MERCHANT_ID=$(echo "$REG" | jq -r '.merchant_id')
API_KEY=$(echo "$REG" | jq -r '.api_key')
DASHBOARD_TOKEN=$(echo "$REG" | jq -r '.dashboard_token')

assert_not_empty "merchant_id" "$MERCHANT_ID"
assert_not_empty "api_key" "$API_KEY"
assert_not_empty "dashboard_token" "$DASHBOARD_TOKEN"

# ── 3. Create session ───────────────────────────────────────────────────────
echo "3. Create session"
COOKIE_JAR=$(mktemp)
SESSION_RESP=$(curl -s -X POST "$BASE/api/auth/session" \
  -H "Content-Type: application/json" \
  -d "{\"token\":\"$DASHBOARD_TOKEN\"}" \
  -c "$COOKIE_JAR")

SESSION_MID=$(echo "$SESSION_RESP" | jq -r '.merchant_id')
assert_eq "session merchant matches" "$MERCHANT_ID" "$SESSION_MID"

# ── 4. Create event with 2 tiers ────────────────────────────────────────────
echo "4. Create event (2 tiers: GA + VIP)"
EVENT_DATE=$(date -u -v+30d +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date -u -d "+30 days" +"%Y-%m-%dT%H:%M:%SZ")
EVENT_RESP=$(curl -s -X POST "$BASE/api/events" \
  -b "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -d "{
    \"title\": \"ZcashCon 2026\",
    \"description\": \"The premier Zcash conference\",
    \"event_date\": \"$EVENT_DATE\",
    \"event_location\": \"Lisbon, Portugal\",
    \"prices\": [
      {\"currency\": \"USD\", \"unit_amount\": 25.0, \"label\": \"General Admission\", \"max_quantity\": 100},
      {\"currency\": \"USD\", \"unit_amount\": 75.0, \"label\": \"VIP\", \"max_quantity\": 20}
    ]
  }")

EVENT_ID=$(echo "$EVENT_RESP" | jq -r '.id')
PRODUCT_ID=$(echo "$EVENT_RESP" | jq -r '.product_id')
EVENT_STATUS=$(echo "$EVENT_RESP" | jq -r '.status')

assert_not_empty "event_id" "$EVENT_ID"
assert_not_empty "product_id" "$PRODUCT_ID"
assert_eq "event status is active" "active" "$EVENT_STATUS"

# ── 5. List events ──────────────────────────────────────────────────────────
echo "5. List events"
EVENTS=$(curl -s "$BASE/api/events" -b "$COOKIE_JAR")
FOUND_STATUS=$(echo "$EVENTS" | jq -r ".[] | select(.id==\"$EVENT_ID\") | .status")
FOUND_SOLD=$(echo "$EVENTS" | jq -r ".[] | select(.id==\"$EVENT_ID\") | .sold_count")

assert_eq "event listed as active" "active" "$FOUND_STATUS"
assert_eq "sold_count starts at 0" "0" "$FOUND_SOLD"

# ── 6. Get prices for the product ───────────────────────────────────────────
echo "6. Get prices for event product"
PRICES=$(curl -s "$BASE/api/products/$PRODUCT_ID/prices" -b "$COOKIE_JAR")
PRICE_COUNT=$(echo "$PRICES" | jq 'length')
GA_PRICE_ID=$(echo "$PRICES" | jq -r '.[] | select(.label=="General Admission") | .id')
VIP_PRICE_ID=$(echo "$PRICES" | jq -r '.[] | select(.label=="VIP") | .id')

assert_eq "2 prices created" "2" "$PRICE_COUNT"
assert_not_empty "GA price_id" "$GA_PRICE_ID"
assert_not_empty "VIP price_id" "$VIP_PRICE_ID"

# ── 7. Checkout GA tier ─────────────────────────────────────────────────────
echo "7. Checkout GA tier"
CHECKOUT1=$(curl -s -X POST "$BASE/api/checkout" \
  -H "Content-Type: application/json" \
  -d "{\"price_id\":\"$GA_PRICE_ID\"}")

INVOICE1_ID=$(echo "$CHECKOUT1" | jq -r '.invoice_id')
CHECKOUT1_TITLE=$(echo "$CHECKOUT1" | jq -r '.event_title')
CHECKOUT1_LOC=$(echo "$CHECKOUT1" | jq -r '.event_location')
CHECKOUT1_LABEL=$(echo "$CHECKOUT1" | jq -r '.price_label')

assert_not_empty "invoice_id" "$INVOICE1_ID"
assert_eq "event_title in checkout" "ZcashCon 2026" "$CHECKOUT1_TITLE"
assert_eq "event_location in checkout" "Lisbon, Portugal" "$CHECKOUT1_LOC"
assert_eq "price_label in checkout" "General Admission" "$CHECKOUT1_LABEL"

# ── 8. Checkout VIP tier ────────────────────────────────────────────────────
echo "8. Checkout VIP tier"
CHECKOUT2=$(curl -s -X POST "$BASE/api/checkout" \
  -H "Content-Type: application/json" \
  -d "{\"price_id\":\"$VIP_PRICE_ID\"}")

INVOICE2_ID=$(echo "$CHECKOUT2" | jq -r '.invoice_id')
CHECKOUT2_LABEL=$(echo "$CHECKOUT2" | jq -r '.price_label')

assert_not_empty "invoice_id" "$INVOICE2_ID"
assert_eq "price_label in checkout" "VIP" "$CHECKOUT2_LABEL"

# ── 9. Simulate ticket creation (direct DB insert) ──────────────────────────
echo "9. Simulate ticket creation (SQLite insert)"
TICKET1_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
TICKET1_CODE="tkt_$(openssl rand -hex 16)"
TICKET2_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
TICKET2_CODE="tkt_$(openssl rand -hex 16)"

sqlite3 "$DB" "INSERT INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status)
  VALUES ('$TICKET1_ID', '$INVOICE1_ID', '$PRODUCT_ID', '$GA_PRICE_ID', '$MERCHANT_ID', '$TICKET1_CODE', 'valid');"
sqlite3 "$DB" "INSERT INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status)
  VALUES ('$TICKET2_ID', '$INVOICE2_ID', '$PRODUCT_ID', '$VIP_PRICE_ID', '$MERCHANT_ID', '$TICKET2_CODE', 'valid');"

green "  PASS: 2 tickets inserted"
PASS=$((PASS + 1))

# ── 10. Get ticket by invoice (public) ──────────────────────────────────────
echo "10. Get ticket by invoice (public endpoint)"
TKT_BY_INV=$(curl -s "$BASE/api/tickets/invoice/$INVOICE1_ID")
TKT_BY_INV_CODE=$(echo "$TKT_BY_INV" | jq -r '.code')
TKT_BY_INV_STATUS=$(echo "$TKT_BY_INV" | jq -r '.status')

assert_eq "ticket code matches" "$TICKET1_CODE" "$TKT_BY_INV_CODE"
assert_eq "ticket status is valid" "valid" "$TKT_BY_INV_STATUS"

# ── 11. Scan ticket (first time — valid) ────────────────────────────────────
echo "11. Scan ticket (first time)"
SCAN1=$(curl -s -X POST "$BASE/api/tickets/scan" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"code\":\"$TICKET1_CODE\"}")

SCAN1_VALID=$(echo "$SCAN1" | jq -r '.valid')
SCAN1_USED=$(echo "$SCAN1" | jq -r '.already_used')

assert_eq "valid on first scan" "true" "$SCAN1_VALID"
assert_eq "not already_used" "false" "$SCAN1_USED"

# ── 12. Scan ticket (second time — already used) ────────────────────────────
echo "12. Scan ticket (second time)"
SCAN2=$(curl -s -X POST "$BASE/api/tickets/scan" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"code\":\"$TICKET1_CODE\"}")

SCAN2_VALID=$(echo "$SCAN2" | jq -r '.valid')
SCAN2_USED=$(echo "$SCAN2" | jq -r '.already_used')

assert_eq "not valid on second scan" "false" "$SCAN2_VALID"
assert_eq "already_used" "true" "$SCAN2_USED"

# ── 13. Void second ticket ──────────────────────────────────────────────────
echo "13. Void ticket #2"
VOID_RESP=$(curl -s -X POST "$BASE/api/tickets/$TICKET2_ID/void" -b "$COOKIE_JAR")
VOID_STATUS=$(echo "$VOID_RESP" | jq -r '.status')

assert_eq "void response" "void" "$VOID_STATUS"

# ── 14. Scan voided ticket ──────────────────────────────────────────────────
echo "14. Scan voided ticket"
SCAN_VOID=$(curl -s -X POST "$BASE/api/tickets/scan" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"code\":\"$TICKET2_CODE\"}")

SCAN_VOID_VOIDED=$(echo "$SCAN_VOID" | jq -r '.voided')
SCAN_VOID_VALID=$(echo "$SCAN_VOID" | jq -r '.valid')

assert_eq "voided flag true" "true" "$SCAN_VOID_VOIDED"
assert_eq "valid is false" "false" "$SCAN_VOID_VALID"

# ── 15. List tickets ────────────────────────────────────────────────────────
echo "15. List tickets"
TICKETS=$(curl -s "$BASE/api/tickets" -b "$COOKIE_JAR")
TKT_COUNT=$(echo "$TICKETS" | jq 'length')
TKT1_STATUS=$(echo "$TICKETS" | jq -r ".[] | select(.id==\"$TICKET1_ID\") | .status")
TKT2_STATUS=$(echo "$TICKETS" | jq -r ".[] | select(.id==\"$TICKET2_ID\") | .status")

assert_eq "2 tickets returned" "2" "$TKT_COUNT"
assert_eq "ticket 1 is used" "used" "$TKT1_STATUS"
assert_eq "ticket 2 is void" "void" "$TKT2_STATUS"

# ── 16. List events (verify counts) ─────────────────────────────────────────
echo "16. Verify event attendance counts"
EVENTS2=$(curl -s "$BASE/api/events" -b "$COOKIE_JAR")
SOLD=$(echo "$EVENTS2" | jq -r ".[] | select(.id==\"$EVENT_ID\") | .sold_count")
USED=$(echo "$EVENTS2" | jq -r ".[] | select(.id==\"$EVENT_ID\") | .used_count")

# sold_count excludes void tickets, so should be 1 (only the used one)
assert_eq "sold_count (non-void)" "1" "$SOLD"
assert_eq "used_count" "1" "$USED"

# ── 17. Archive event ───────────────────────────────────────────────────────
echo "17. Archive event"
ARCHIVE=$(curl -s -X POST "$BASE/api/events/$EVENT_ID/archive" -b "$COOKIE_JAR")
ARCHIVE_STATUS=$(echo "$ARCHIVE" | jq -r '.status')

assert_eq "archive returns cancelled" "cancelled" "$ARCHIVE_STATUS"

# ── 18. Verify event is cancelled and product inactive ──────────────────────
echo "18. Verify event cancelled & product deactivated"
EVENTS3=$(curl -s "$BASE/api/events" -b "$COOKIE_JAR")
FINAL_STATUS=$(echo "$EVENTS3" | jq -r ".[] | select(.id==\"$EVENT_ID\") | .status")

assert_eq "event is cancelled" "cancelled" "$FINAL_STATUS"

# ── 19. Checkout should fail after archive ──────────────────────────────────
echo "19. Checkout after archive (should fail)"
CHECKOUT_FAIL=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/checkout" \
  -H "Content-Type: application/json" \
  -d "{\"price_id\":\"$GA_PRICE_ID\"}")

if [ "$CHECKOUT_FAIL" != "201" ]; then
  green "  PASS: checkout blocked (HTTP $CHECKOUT_FAIL)"
  PASS=$((PASS + 1))
else
  red "  FAIL: checkout should have been blocked after archive"
  FAIL=$((FAIL + 1))
fi

# ── 20. Cleanup ─────────────────────────────────────────────────────────────
echo "20. Cleanup — delete test merchant"
DEL=$(curl -s -X POST "$BASE/api/merchants/me/delete" -b "$COOKIE_JAR")
DEL_STATUS=$(echo "$DEL" | jq -r '.status')
assert_eq "merchant deleted" "deleted" "$DEL_STATUS"

# Cleanup temp files
rm -f "$COOKIE_JAR"

echo ""
echo "============================================"
printf " Results: \033[32m%d passed\033[0m, \033[31m%d failed\033[0m\n" "$PASS" "$FAIL"
echo "============================================"
echo ""

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi

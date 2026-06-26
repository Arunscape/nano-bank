#!/bin/bash
set -e

BASE_URL="${API_BASE_URL:-http://localhost:8081}"
EMAIL="${CUSTOMER_EMAIL:-jane.doe@example.com}"

# Colour helpers
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

step() { echo -e "\n${CYAN}=== $1 ===${NC}"; }
ok()   { echo -e "${GREEN}✓ $1${NC}"; }
info() { echo -e "${YELLOW}  $1${NC}"; }

# ── 0. Prerequisites ──────────────────────────────────────────────────────────
if ! command -v jq &>/dev/null; then
  echo "jq is required: brew install jq"
  exit 1
fi

# ── 1. Health check ───────────────────────────────────────────────────────────
step "1/6  Health check"
HEALTH=$(curl -sf "$BASE_URL/health") || {
  echo "API not reachable at $BASE_URL — is the server running?"
  exit 1
}
echo "$HEALTH" | jq
ok "API and database are healthy"

# ── 2. Create customer ────────────────────────────────────────────────────────
step "2/6  Create customer ($EMAIL)"
CUSTOMER=$(curl -sf -X POST "$BASE_URL/api/v1/customers" \
  -H "Content-Type: application/json" \
  -d "{
    \"email\": \"$EMAIL\",
    \"phone_number\": \"4161234567\",
    \"first_name\": \"Jane\",
    \"last_name\": \"Doe\",
    \"date_of_birth\": \"1990-05-15\",
    \"sin\": \"123456789\",
    \"password\": \"securepass123\"
  }") || {
  echo "Failed — is the email already taken? Try: CUSTOMER_EMAIL=other@example.com $0"
  exit 1
}
echo "$CUSTOMER" | jq
CUSTOMER_ID=$(echo "$CUSTOMER" | jq -r '.customer_id')
ok "customer_id = $CUSTOMER_ID"

# ── 3. Open credit card account ───────────────────────────────────────────────
step "3/6  Open credit card account"
ACCOUNT=$(curl -sf -X POST "$BASE_URL/api/v1/accounts" \
  -H "Content-Type: application/json" \
  -d "{\"customer_id\": \"$CUSTOMER_ID\", \"account_type\": \"credit_card\"}")
echo "$ACCOUNT" | jq
ACCOUNT_ID=$(echo "$ACCOUNT" | jq -r '.account_id')
info "Credit limit: \$$(echo "$ACCOUNT" | jq -r '.overdraft_limit')"
ok "account_id = $ACCOUNT_ID"

# ── 4. Authorize purchase ─────────────────────────────────────────────────────
step "4/6  Authorize purchase (Tim Hortons \$99.99)"
AUTH=$(curl -sf -X POST "$BASE_URL/api/v1/cards/authorize" \
  -H "Content-Type: application/json" \
  -d "{\"account_id\": \"$ACCOUNT_ID\", \"amount\": 99.99, \"merchant\": \"Tim Hortons\"}")
echo "$AUTH" | jq

AUTH_STATUS=$(echo "$AUTH" | jq -r '.status')
if [ "$AUTH_STATUS" != "approved" ]; then
  echo "Authorization declined: $(echo "$AUTH" | jq -r '.reason')"
  exit 1
fi

AUTH_ID=$(echo "$AUTH" | jq -r '.auth_id')
info "Available balance after hold: \$$(echo "$AUTH" | jq -r '.available_balance')"
ok "auth_id = $AUTH_ID"

# ── 5. Capture authorization ──────────────────────────────────────────────────
step "5/6  Capture authorization (post to ledger)"
CAPTURE=$(curl -sf -X POST "$BASE_URL/api/v1/cards/capture" \
  -H "Content-Type: application/json" \
  -d "{\"auth_id\": \"$AUTH_ID\"}")
echo "$CAPTURE" | jq
ok "Transaction posted: $(echo "$CAPTURE" | jq -r '.reference_number')"

# ── 6. Settle ─────────────────────────────────────────────────────────────────
step "6/6  Settle all captured purchases"
SETTLE=$(curl -sf -X POST "$BASE_URL/api/v1/cards/settle" \
  -H "Content-Type: application/json" \
  -d '{}')
echo "$SETTLE" | jq
ok "Done — $(echo "$SETTLE" | jq -r '.settled_transactions') transaction(s) settled for \$$(echo "$SETTLE" | jq -r '.net_amount')"

echo -e "\n${GREEN}Full flow complete.${NC}"
echo ""
echo "To inspect the ledger:"
echo "  psql -h localhost -p 5432 -U nanobank_user -d nano_bank_db"
echo "  SELECT * FROM transaction_entries ORDER BY created_at DESC LIMIT 10;"

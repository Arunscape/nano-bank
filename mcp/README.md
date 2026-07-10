# nano-bank agent MCP demo — Claude as a mandated banking agent

A real AI agent (Claude Code) acting on a customer's behalf through nano-bank's
**agent plane**: scoped, limited, expiring, **revocable** consent — demonstrated live.

The MCP server (`nano_bank_agent_mcp.py`) holds only the *agent's* credentials — it never
sees the customer's password or token, and it discovers the agent's mandates live. Its agent
JWTs are 5-minute *pointers* to one mandate each; nano-bank re-reads the mandate row on every
request, so revocation takes effect on the very next tool call.

## Prerequisites

- The nano-bank stack up (see the root `CLAUDE.md`, "Running the stack"): Kind Postgres
  (port-forward on `::1:5432`),
  the API on `:8081`, and ideally the modern core on `:8091` (only needed to *fund* the
  demo account — the agent plane itself never touches the GL).
- `uv` (both scripts carry PEP 723 inline deps — no venv or install step).
- Claude Code.

> **Prefer the UI:** the bank now serves a built-in consent app at
> **`http://localhost:8081/app`** — sign up, open an account, register an agent, grant the
> mandates with scopes/caps/payees, and copy the generated `claude mcp add` command straight
> from the agent card (one registration covers all that agent's mandates). It also shows the live activity trail (incl. denials) and has the
> big red **Revoke access** button. `setup_demo.py` below remains the one-shot CLI path.

## Run the demo

```bash
# 1. Seed: customer + funded chequing + savings + agent "Claude" holding TWO
#    differently-scoped mandates (chequing: reads+capped transfers; savings: read-only).
#    Writes mcp/.env.demo (secrets, gitignored) and prints everything below.
uv run mcp/setup_demo.py

# 2. Wire Claude Code up as the agent (command printed by the seed script).
#    Note: only the AGENT's credentials — its mandates are discovered live.
claude mcp add nano-bank-agent \
  --env NANO_BANK_URL=http://localhost:8081 \
  --env NANO_BANK_AGENT_ID=... --env NANO_BANK_AGENT_SECRET=... \
  -- uv run /abs/path/to/nano-bank/mcp/nano_bank_agent_mcp.py

# 3. In a new Claude Code session, ask (one registration, both accounts):
#    • "What access do you have to my bank?"      → both mandates, scopes, caps
#    • "What's my chequing balance? And savings?" → reads under each mandate
#    • "Move $150 from chequing into my savings (<payee id from the seed output>)."
#    • "Now move $250 more."          → denied: over the $200 per-transaction cap
#    • "Transfer $20 FROM my savings" → denied: SCOPE_MISSING (savings is read-only)
#    • "Send $50 to <another uuid>"   → denied: payee not on the allowlist
```

Tools exposed: `list_my_access`, `whoami`, `get_account_balance`, `get_recent_transactions`,
`transfer` (the read/transfer tools take an `account` selector).
The `account` selector only picks *which mandate* to act under — at the bank, every request
is still pinned to that mandate's account (the bank's agent surface has no account parameter).

**Multiple accounts — one registration.** A registration carries only the *agent's*
credentials; the server **discovers the agent's mandates live** (`POST /auth/agent-mandates`),
so one agent can hold several differently-scoped grants (e.g. read-only savings + capped
transfers on chequing). Tools take an `account` argument ("chequing", "savings-1234") that
picks which mandate to act under; `list_my_access` shows the live set. Grant or revoke in
`/app` and Claude sees the change on its next call — no re-registration ever.

### The payment demo (Phase 2)

The seeded mandate allows transfers **only** to the demo savings account, at most **$200 per
transaction** and **$500 per day** — enforced (and the spend *reserved*) under a row lock in
the bank, not in the agent. `transfer` requires an idempotency key, so a retried payment can
never double-spend: the tool's docstring instructs the model to reuse the key on retry.
Breaches come back as structured `POLICY_DENIED` results (`MAX_PER_TX_EXCEEDED`,
`DAILY_CAP_EXCEEDED`, `PAYEE_NOT_ALLOWED`) that Claude reads and explains; the two cap
overruns are audited as `step_up_required` — the exact rows Phase 3's human-approval flow
will consume.

## The revoke moment

As the account owner (customer token, printed by the seed script / in `.env.demo`):

```bash
curl -s -X DELETE http://localhost:8081/api/v1/mandates/$MANDATE_ID \
  -H "Authorization: Bearer $DEMO_CUSTOMER_TOKEN" -w "%{http_code}\n"   # → 204
```

Then ask Claude for the CHEQUING balance again (the savings mandate keeps working —
per-grant revocation). The next chequing tool call returns
`MANDATE_INACTIVE` (401) and Claude explains its access was revoked — even though its
cached token is still cryptographically valid. That's the design's core claim
(token-as-pointer, no blocklist) shown live.

## Variant: scope denial

Re-run `uv run mcp/setup_demo.py` but grant only `read:balance` (edit the `scopes` list),
re-`claude mcp add` with the new ids, and ask for transactions: `POLICY_DENIED /
SCOPE_MISSING` (403), with the denial recorded in the audit trail.

## The audit trail

Every decision — allow **and** deny, including token issuance — is append-only in
`agent_actions`:

```bash
kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db \
  -c "SELECT operation, decision, reason, created_at FROM agent_actions ORDER BY created_at DESC LIMIT 20;"
```

## Claude Desktop (untested here)

The same server works in Claude Desktop — add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "nano-bank-agent": {
      "command": "uv",
      "args": ["run", "/abs/path/to/nano-bank/mcp/nano_bank_agent_mcp.py"],
      "env": {
        "NANO_BANK_URL": "http://localhost:8081",
        "NANO_BANK_AGENT_ID": "...",
        "NANO_BANK_AGENT_SECRET": "..."
      }
    }
  }
}
```

## Cleanup

```bash
claude mcp remove nano-bank-agent
# and/or revoke the mandate (the revoke curl above) — either alone cuts access.
```

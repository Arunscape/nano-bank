# nano-bank agent MCP demo — Claude as a mandated banking agent

A real AI agent (Claude Code) acting on a customer's behalf through nano-bank's
**agent plane**: scoped, limited, expiring, **revocable** consent — demonstrated live.

The MCP server (`nano_bank_agent_mcp.py`) holds only the *agent's* credentials and a
mandate id. It never sees the customer's password or token. Its agent JWTs are 5-minute
*pointers*; nano-bank re-reads the mandate row on every request, so revocation takes
effect on the very next tool call.

## Prerequisites

- The nano-bank stack up (see `HANDOFF.md` §6): Kind Postgres (port-forward on `::1:5432`),
  the API on `:8081`, and ideally the modern core on `:8091` (only needed to *fund* the
  demo account — the agent plane itself never touches the GL).
- `uv` (both scripts carry PEP 723 inline deps — no venv or install step).
- Claude Code.

## Run the demo

```bash
# 1. Seed: customer + funded account + agent "Claude" + mandate.
#    Writes mcp/.env.demo (secrets, gitignored) and prints everything below.
uv run mcp/setup_demo.py

# 2. Wire Claude Code up as the agent (command printed by the seed script):
claude mcp add nano-bank-agent \
  --env NANO_BANK_URL=http://localhost:8081 \
  --env NANO_BANK_AGENT_ID=... --env NANO_BANK_AGENT_SECRET=... \
  --env NANO_BANK_MANDATE_ID=... \
  -- uv run /abs/path/to/nano-bank/mcp/nano_bank_agent_mcp.py

# 3. In a new Claude Code session, ask:
#    • "Who are you to my bank? Introduce yourself using the nano-bank tools."
#    • "What's my account balance?"
#    • "Summarize my recent transactions."
```

Tools exposed: `whoami`, `get_account_balance`, `get_recent_transactions`. Note what's
*absent*: there is no account parameter anywhere (the mandate pins the account) and no
transfer tool (Phase 2 — and the demo mandate wouldn't allow it anyway).

## The revoke moment

As the account owner (customer token, printed by the seed script / in `.env.demo`):

```bash
curl -s -X DELETE http://localhost:8081/api/v1/mandates/$MANDATE_ID \
  -H "Authorization: Bearer $DEMO_CUSTOMER_TOKEN" -w "%{http_code}\n"   # → 204
```

Then ask Claude for the balance again. The next tool call returns
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
        "NANO_BANK_AGENT_SECRET": "...",
        "NANO_BANK_MANDATE_ID": "..."
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

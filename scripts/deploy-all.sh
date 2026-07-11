#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== cluster B: modern core =="
( cd ../nano-bank-modern-core && ./k8s/deploy.sh )
echo "== cluster A: bank + agent =="
./k8s/deploy.sh          # postgres + bank-api + cross-cluster wiring
./agent/k8s/deploy.sh    # qdrant + mcp + api + console + secret
echo "✅ full stack up — run: ./agent/e2e_test.sh"

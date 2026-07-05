#!/bin/bash
# Stop and remove the test-harness containers (leaves nano-bank itself running).
set -uo pipefail
podman rm -f nano-bank-generator nano-bank-visa nano-bank-interac nano-bank-viewer 2>/dev/null || true
echo "🛑 Test harness stopped."

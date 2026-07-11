#!/bin/bash

echo "🛑 Stopping Nano Bank..."
echo ""

# Stop API server
echo "📍 Stopping API server..."
pkill -f "nano-bank-api" 2>/dev/null && echo "✅ API server stopped" || echo "⚠️  No API server running"
pkill -f "cargo run" 2>/dev/null || true

# Stop port-forward
echo "📍 Stopping port-forward..."
pkill -f "kubectl port-forward.*nano-bank.*postgres" 2>/dev/null && echo "✅ Port-forward stopped" || echo "⚠️  No port-forward running"

# Delete Kubernetes cluster
echo "📍 Deleting Kubernetes cluster..."
if kind get clusters 2>/dev/null | grep -q "^nano-bank$"; then
    kind delete cluster --name nano-bank
    echo "✅ Kubernetes cluster deleted"
else
    echo "⚠️  No cluster found"
fi

# Clean up log files
echo "📍 Cleaning up logs..."
rm -f /tmp/nano-bank-port-forward.log /tmp/nano-bank-api.log
echo "✅ Logs cleaned"

echo ""
echo "✅ Nano Bank stopped successfully!"

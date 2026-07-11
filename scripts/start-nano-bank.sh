#!/bin/bash
set -e

echo "🏦 Starting Nano Bank..."
echo ""

# Step 1: Create Kubernetes cluster and deploy PostgreSQL
echo "📦 Step 1/3: Creating Kubernetes cluster and deploying PostgreSQL..."
project_path=$(cd "$(dirname "$0")/.." && pwd)
cd $project_path

# Check if cluster already exists
if kind get clusters 2>/dev/null | grep -q "^nano-bank$"; then
    echo "✅ Cluster already exists"
else
    kind create cluster --config k8s/kind-cluster-config.yaml
fi

# Deploy PostgreSQL
./k8s/deploy.sh

echo ""
echo "📡 Step 2/3: Setting up port forwarding..."
# Kill any existing port-forward
pkill -f "kubectl port-forward.*nano-bank.*postgres" 2>/dev/null || true
sleep 2

# Start port-forward in background
kubectl port-forward -n nano-bank svc/postgres-service 5432:5432 > /tmp/nano-bank-port-forward.log 2>&1 &
PF_PID=$!
echo "✅ Port-forward started (PID: $PF_PID)"

# Wait for port-forward to be ready
echo "⏳ Waiting for port-forward to be ready..."
for i in {1..10}; do
    if ss -tlnp 2>/dev/null | grep -q ":5432"; then
        echo "✅ Port 5432 is now listening"
        break
    fi
    sleep 1
done

echo ""
echo "🚀 Step 3/3: Starting API server..."
cd $project_path/api

# Start the API in background
cargo run > /tmp/nano-bank-api.log 2>&1 &
API_PID=$!
echo "✅ API server starting (PID: $API_PID)"

echo ""
echo "⏳ Waiting for API to be ready..."
for i in {1..30}; do
    if curl -s http://localhost:8081/health > /dev/null 2>&1; then
        echo "✅ API is ready!"
        break
    fi
    sleep 1
done

echo ""
echo "🎉 Nano Bank is now running!"
echo ""
echo "📊 Service URLs:"
echo "  • API Server:      http://localhost:8081"
echo "  • Health Check:    http://localhost:8081/health"
echo "  • API Docs:        http://localhost:8081/docs"
echo "  • PostgreSQL:      localhost:5432"
echo ""
echo "📝 Process IDs:"
echo "  • Port-forward:    $PF_PID"
echo "  • API Server:      $API_PID"
echo ""
echo "📋 Logs:"
echo "  • Port-forward:    tail -f /tmp/nano-bank-port-forward.log"
echo "  • API Server:      tail -f /tmp/nano-bank-api.log"
echo ""
echo "🛑 To stop: scripts/stop-nano-bank.sh"

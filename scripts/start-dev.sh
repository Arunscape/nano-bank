#!/bin/bash
set -e

project_path=$(cd "$(dirname "$0")/.." && pwd)
cd "$project_path"

echo "🏦 Starting Nano Bank in Dev Mode..."
echo ""

# Step 1: Check Kubernetes and PostgreSQL
if ! kind get clusters 2>/dev/null | grep -q "^nano-bank$"; then
    echo "📦 Cluster 'nano-bank' not found. Creating and deploying..."
    kind create cluster --config k8s/kind-cluster-config.yaml
    ./k8s/deploy.sh
else
    # Cluster exists, verify postgres is deployed and ready
    echo "🐘 Checking PostgreSQL status..."
    if ! kubectl get deployment postgres -n nano-bank >/dev/null 2>&1; then
        echo "⚠️ PostgreSQL deployment missing. Deploying..."
        ./k8s/deploy.sh
    else
        # Wait up to 30 seconds for it to be ready
        if ! kubectl wait --namespace=nano-bank --for=condition=ready pod --selector=app=postgres --timeout=30s >/dev/null 2>&1; then
            echo "⚠️ PostgreSQL is not ready. Re-deploying..."
            ./k8s/deploy.sh
        else
            echo "✅ PostgreSQL is running"
        fi
    fi
fi

# Step 2: Ensure Port Forwarding is Active
if ! pgrep -f "kubectl port-forward.*nano-bank.*postgres" > /dev/null; then
    echo "📡 Setting up port forwarding..."
    
    # Kill any dangling port-forward on 5432 just in case
    if lsof -i :5432 > /dev/null 2>&1; then
        echo "⚠️ Port 5432 is already in use by another process. Attempting to kill it..."
        lsof -ti :5432 | xargs kill -9 2>/dev/null || true
        sleep 1
    fi

    kubectl port-forward -n nano-bank svc/postgres-service 5432:5432 > /tmp/nano-bank-port-forward.log 2>&1 &
    PF_PID=$!
    echo "✅ Port-forward started (PID: $PF_PID)"
    
    echo "⏳ Waiting for port-forward to be ready..."
    for i in {1..10}; do
        if nc -z localhost 5432 2>/dev/null; then
            echo "✅ Port 5432 is now listening"
            break
        fi
        sleep 1
    done
else
    echo "✅ Port-forward is already running"
fi

echo "🔍 Testing database connection and running a sample query via kubectl..."
if kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c '\dt' >/dev/null 2>&1; then
    echo "✅ Database connection successful! Tables found:"
    kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c '\dt' | head -n 10
else
    echo "❌ Database connection failed or no tables found. Is the database initialized?"
    exit 1
fi

# Step 3: Stop background API if it was started by start-nano-bank.sh
API_PIDS=$(pgrep -f "target/.*/nano-bank-api" || true)
if [ ! -z "$API_PIDS" ]; then
    echo "🛑 Stopping existing background API server..."
    kill -9 $API_PIDS 2>/dev/null || true
    sleep 1
fi

if lsof -i :8081 > /dev/null 2>&1; then
    PORT_PID=$(lsof -ti :8081)
    echo "⚠️ Port 8081 is already in use by process ID(s): $PORT_PID"
    echo "Please stop the process manually or use a different port."
    exit 1
fi

echo ""
echo "🚀 Starting API server in foreground..."
echo "💡 (Changes to the 'api' folder will be automatically hot-reloaded)"
echo "🛑 Press Ctrl+C to stop the API server"
echo ""

cd "$project_path/api"

# Use cargo-watch if available, otherwise fallback to cargo run
if cargo watch --version >/dev/null 2>&1; then
    exec cargo watch -q -c -w . -x run
else
    echo "⚠️ 'cargo watch' not found. Running standard 'cargo run' without hot-reloading."
    exec cargo run
fi

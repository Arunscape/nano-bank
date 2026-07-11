#!/bin/bash
cd "$(dirname "$0")/.."

echo "🔧 Setting up Kubernetes environment for Nano Bank..."

# Add ~/bin to PATH for this session
export PATH="$HOME/bin:$PATH"

# Check if tools are available
echo "📋 Checking prerequisites..."
if ! command -v kubectl &> /dev/null; then
    echo "❌ kubectl not found in PATH"
    echo "💡 Run: export PATH=\"\$HOME/bin:\$PATH\""
    exit 1
fi

if ! command -v kind &> /dev/null; then
    echo "❌ kind not found in PATH"
    echo "💡 Run: export PATH=\"\$HOME/bin:\$PATH\""
    exit 1
fi

if ! command -v docker &> /dev/null; then
    echo "❌ Docker not installed"
    exit 1
fi

echo "✅ All tools found"

# Check Docker permissions and start if needed
echo "🐳 Checking Docker status..."
if ! docker version &> /dev/null; then
    echo "⚠️  Docker daemon not accessible. You may need to:"
    echo "   1. Start Docker daemon: sudo systemctl start docker"
    echo "   2. Add user to docker group: sudo usermod -aG docker \$USER"
    echo "   3. Log out and back in for group changes to take effect"
    echo ""
    echo "🔧 Attempting to start Docker daemon..."
    sudo systemctl start docker

    echo "🔧 Adding user to docker group..."
    sudo usermod -aG docker $USER

    echo "⚠️  You may need to log out and back in for group changes to take effect"
    echo "📝 Alternatively, you can run: newgrp docker"

    # Try with newgrp if available
    if command -v newgrp &> /dev/null; then
        echo "🔄 Trying to refresh group membership..."
        newgrp docker << EOF
docker version
if [ \$? -eq 0 ]; then
    echo "✅ Docker is now accessible"
    echo "🚀 Ready to deploy Kubernetes cluster!"
    echo ""
    echo "📋 Next steps:"
    echo "   1. cd ~/dev/nano-bank"
    echo "   2. export PATH=\"\$HOME/bin:\$PATH\""
    echo "   3. ./k8s/deploy.sh"
else
    echo "❌ Still having Docker permission issues"
    echo "💡 Please log out and back in, then try again"
fi
EOF
    fi
else
    echo "✅ Docker is running and accessible"
    echo "🚀 Ready to deploy Kubernetes cluster!"
    echo ""
    echo "📋 To deploy the nano-bank PostgreSQL cluster:"
    echo "   1. cd ~/dev/nano-bank"
    echo "   2. export PATH=\"\$HOME/bin:\$PATH\""
    echo "   3. ./k8s/deploy.sh"
fi
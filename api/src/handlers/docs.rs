use axum::{http::StatusCode, response::Html};

pub async fn api_docs() -> Result<Html<String>, StatusCode> {
    let docs_html = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Nano Bank API Documentation</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 40px; }
        h1 { color: #2c3e50; }
        h2 { color: #34495e; border-bottom: 2px solid #eee; padding-bottom: 10px; }
        .endpoint { background: #f8f9fa; padding: 15px; margin: 10px 0; border-radius: 5px; }
        .method { padding: 4px 8px; border-radius: 3px; color: white; font-weight: bold; }
        .get { background: #28a745; }
        .post { background: #007bff; }
        .put { background: #ffc107; color: black; }
        .delete { background: #dc3545; }
        code { background: #f1f3f4; padding: 2px 4px; border-radius: 3px; }
    </style>
</head>
<body>
    <h1>🏦 Nano Bank API Documentation</h1>
    <p>Welcome to the Nano Bank Core Banking API. This is a production-grade challenger bank backend built with Rust.</p>

    <h2>🔐 Authentication Endpoints</h2>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/auth/login</code><br>
        Authenticate customer and get access token (customer plane)
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/auth/service-token</code><br>
        Mint a network-plane service token (client-credentials) — used by the card network for <code>/cards/*</code>
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/auth/refresh</code><br>
        Exchange a refresh token for a new access token (refresh token is rotated)
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/auth/logout</code><br>
        Terminate the session and revoke its refresh token
    </div>

    <h2>👤 Customer Management</h2>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/customers</code><br>
        Register new customer
    </div>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/customers/profile</code><br>
        Get customer profile (authenticated)
    </div>
    <div class="endpoint">
        <span class="method put">PUT</span> <code>/api/v1/customers/profile</code><br>
        Update customer profile
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/customers/kyc/documents</code><br>
        Upload KYC documents
    </div>

    <h2>💳 Account Management</h2>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/accounts</code><br>
        Get customer accounts
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/accounts</code><br>
        Create new account
    </div>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/accounts/{id}</code><br>
        Get account details
    </div>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/accounts/{id}/balance</code><br>
        Get account balance
    </div>

    <h2>💸 Transaction Processing</h2>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/transactions/transfer</code><br>
        Transfer money between accounts
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/transactions/deposit</code><br>
        Deposit money to account
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/transactions/withdrawal</code><br>
        Withdraw money from account
    </div>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/transactions</code><br>
        Get transaction history
    </div>

    <h2>🔒 Security</h2>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/security/sessions</code><br>
        Get active sessions
    </div>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/api/v1/security/devices</code><br>
        Get known devices
    </div>
    <div class="endpoint">
        <span class="method post">POST</span> <code>/api/v1/security/devices/trust</code><br>
        Trust a device
    </div>

    <h2>💚 System</h2>
    <div class="endpoint">
        <span class="method get">GET</span> <code>/health</code><br>
        System health check
    </div>

    <h2>🚀 Getting Started</h2>
    <ol>
        <li>Register a customer with <code>POST /api/v1/customers</code></li>
        <li>Login with <code>POST /api/v1/auth/login</code></li>
        <li>Create an account with <code>POST /api/v1/accounts</code></li>
        <li>Start banking!</li>
    </ol>

    <h2>🔧 Technical Details</h2>
    <ul>
        <li><strong>Authentication:</strong> JWT tokens with refresh mechanism</li>
        <li><strong>Database:</strong> PostgreSQL with double-entry bookkeeping</li>
        <li><strong>Currency:</strong> CAD (Canadian Dollars)</li>
        <li><strong>Precision:</strong> All monetary values use Rust Decimal for precision</li>
        <li><strong>Security:</strong> Comprehensive audit logging and fraud detection</li>
    </ul>

    <p><em>Built with ❤️ using Rust, Axum, and PostgreSQL</em></p>
</body>
</html>
"#;

    Ok(Html(docs_html.to_string()))
}

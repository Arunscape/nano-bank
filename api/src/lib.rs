pub mod config;
pub mod errors;
pub mod handlers;
pub mod middleware;
pub mod models;
pub mod repositories;
pub mod services;
pub mod utils;

use axum::{
    extract::DefaultBodyLimit,
    http::{
        header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
        HeaderValue, Method,
    },
    routing::get,
    Router,
};
use config::{database::create_connection_pool, Settings};
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize configuration
    let settings = Settings::new().unwrap_or_else(|err| {
        eprintln!("Failed to load configuration: {}", err);
        eprintln!("Using default configuration");
        Settings::default()
    });

    // Initialize logging
    init_logging(&settings).await;

    info!("🏦 Starting Nano Bank API Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    info!("Environment: {}", std::env::var("RUN_MODE").unwrap_or_else(|_| "development".into()));

    // Create database connection pool
    let pool = match create_connection_pool(&settings).await {
        Ok(pool) => {
            info!("✅ Database connection established");
            pool
        }
        Err(e) => {
            warn!("❌ Failed to connect to database: {}", e);
            warn!("💡 Make sure your PostgreSQL cluster is running:");
            warn!("   cd ~/dev/nano-bank && ./k8s/deploy.sh");
            std::process::exit(1);
        }
    };

    // Run database health check
    if let Err(e) = config::database::health_check(&pool).await {
        warn!("❌ Database health check failed: {}", e);
        std::process::exit(1);
    }

    // Verify schema is in place
    if let Err(e) = config::database::run_migrations(&pool).await {
        warn!("❌ Migration check failed: {}", e);
        std::process::exit(1);
    }

    // Ensure the internal GL accounts the card rails post against exist.
    if let Err(e) = handlers::cards::ensure_system_accounts(&pool).await {
        warn!("❌ Failed to bootstrap system GL accounts: {}", e);
        std::process::exit(1);
    }

    // Create application router
    let app = create_router(pool, &settings).await;

    // Start server
    let listener = tokio::net::TcpListener::bind(&settings.server_address()).await?;

    info!("🚀 Server running on http://{}", settings.server_address());
    info!("📖 API Documentation: http://{}/docs", settings.server_address());
    info!("💚 Health Check: http://{}/health", settings.server_address());

    axum::serve(listener, app).await?;

    Ok(())
}

pub async fn create_router(
    pool: config::database::DatabasePool,
    settings: &Settings,
) -> Router {
    // CORS configuration for web frontend
    let cors = CorsLayer::new()
        .allow_origin("http://localhost:3000".parse::<HeaderValue>().unwrap())
        .allow_origin("http://localhost:8080".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_credentials(true)
        .allow_headers([AUTHORIZATION, ACCEPT, CONTENT_TYPE]);

    // Create application state
    let app_state = handlers::AppState {
        pool: pool.clone(),
        settings: settings.clone(),
    };

    // Build the router
    Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/docs", get(handlers::docs::api_docs))
        .nest("/api/v1/auth", handlers::auth::auth_routes())
        .nest("/api/v1/customers", handlers::customers::customer_routes())
        .nest("/api/v1/accounts", handlers::accounts::account_routes())
        .nest("/api/v1/cards", handlers::cards::card_routes())
        .nest("/api/v1/transactions", handlers::transactions::transaction_routes())
        .nest("/api/v1/security", handlers::security::security_routes())
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
                .layer(cors)
        )
        .with_state(app_state)
}

pub async fn init_logging(settings: &Settings) {
    let subscriber = tracing_subscriber::registry();

    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false)
        .with_thread_ids(true)
        .with_line_number(true);

    subscriber
        .with(fmt_layer)
        .with(tracing_subscriber::EnvFilter::new(&settings.logging.level))
        .try_init().ok(); // Ignore if already initialized (for tests)
}
use secrecy::SecretString;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "Usage: sparkrouter [COMMAND]

Without a command, serve the gateway and dashboard.

Commands (stop the server first; both use ROUTER_DATABASE_URL):
  rotate-master-key         Re-encrypt stored credentials from ROUTER_MASTER_KEY
                            to ROUTER_NEW_MASTER_KEY, then start with the new key
  reset-dashboard-password  Remove the stored dashboard password; the next start
                            prints a setup code for choosing a new one
  help                      Show this message";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => serve().await,
        ["rotate-master-key"] => rotate_master_key().await,
        ["reset-dashboard-password"] => reset_dashboard_password().await,
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn rotate_master_key() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let new = std::env::var("ROUTER_NEW_MASTER_KEY").map_err(|_| {
        sparkrouter::error::AppError::Configuration(
            "ROUTER_NEW_MASTER_KEY is required (base64-encoded 32 random bytes)".into(),
        )
    })?;
    let new = Crypto::from_base64(&SecretString::from(new)).map_err(|_| {
        sparkrouter::error::AppError::Configuration(
            "ROUTER_NEW_MASTER_KEY must be base64-encoded 32 random bytes".into(),
        )
    })?;
    let pool = db::connect(&config.database_url).await?;
    let rotation = db::master_key::rotate(&pool, &config.crypto, &new).await?;
    pool.close().await;
    println!(
        "Rotated {} provider credential(s) and {} unrevealed API key(s).",
        rotation.provider_credentials, rotation.unrevealed_api_keys
    );
    println!(
        "Set ROUTER_MASTER_KEY to the new key, remove ROUTER_NEW_MASTER_KEY, and start the server. \
         The old key no longer starts it. Existing gateway API keys keep working."
    );
    Ok(())
}

async fn reset_dashboard_password() -> Result<(), Box<dyn std::error::Error>> {
    let pool = db::connect(&Config::database_url_from_env()).await?;
    let removed = db::dashboard_admin::reset(&pool).await?;
    pool.close().await;
    if removed {
        println!(
            "Stored dashboard password removed. Start the server and use the printed setup code at /setup."
        );
    } else {
        println!("No stored dashboard password exists; nothing changed.");
    }
    Ok(())
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let bind = config.bind;
    let state = AppState::from_config(config).await?;
    let pool = state.db.clone();
    let shutdown_state = state.clone();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    tracing::info!(%address, app = sparkrouter::APP_NAME, "server listening");
    if state.dashboard_auth.is_none() {
        tracing::warn!(
            "dashboard login disabled by ROUTER_DASHBOARD_AUTH=disabled; protect the dashboard with an authenticating proxy"
        );
    }
    // A one-time bootstrap code, useless once a password is set. Printing it is
    // how the operator proves control of the server to the first-run page.
    if let Some(code) = state.dashboard_setup_code() {
        tracing::warn!(
            "dashboard setup required: open http://{address}/setup and enter dashboard setup code {code}"
        );
    }
    axum::serve(
        listener,
        app::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    // Commit history from the final requests before closing the database.
    if tokio::time::timeout(
        std::time::Duration::from_secs(10),
        shutdown_state.flush_history(),
    )
    .await
    .is_err()
    {
        tracing::warn!("request history flush unavailable");
    }
    pool.close().await;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let terminate = async {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! { () = ctrl_c => {}, () = terminate => {} }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
    tracing::info!("shutting down");
}

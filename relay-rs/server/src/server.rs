//! `serve`: binds the listen address and runs the axum app until SIGTERM or Ctrl-C.

use std::net::SocketAddr;

use crate::app::{AppState, router};
use crate::config::Config;

pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// Runs the relay with `cfg` until a shutdown signal arrives. With the `lab` feature, refuses to
/// start unless every listen address is loopback.
pub async fn serve(cfg: &Config) -> Result<(), ServeError> {
    #[cfg(feature = "lab")]
    {
        crate::lab::require_loopback(&[cfg.listen]).map_err(ServeError::from)?;
        tracing::warn!(
            listen = %cfg.listen,
            "LAB BUILD: the listener is loopback-only. Local testing only."
        );
    }
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!(
        listen = %listener.local_addr()?,
        public_origin = %cfg.public_origin,
        max_connections = cfg.max_connections,
        "relay listening"
    );
    let state = AppState::new(cfg.admin_token.expose(), &cfg.assets_dir);
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("relay stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

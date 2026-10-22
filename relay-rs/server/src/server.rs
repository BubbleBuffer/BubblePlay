//! `serve`: binds the listen address and runs the axum app until SIGTERM or Ctrl-C.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::app::router;
use crate::config::Config;
use crate::runtime::{Shared, spawn_ticker};

pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// Runs the relay with `cfg` until a shutdown signal arrives.
pub async fn serve(cfg: &Config) -> Result<(), ServeError> {
    let listener = TcpListener::bind(cfg.listen).await?;
    run(cfg, listener).await
}

/// Runs the relay on an already bound listener until a shutdown signal. Tests use this with a
/// listener on 127.0.0.1:0.
pub async fn run(cfg: &Config, listener: TcpListener) -> Result<(), ServeError> {
    tracing::info!(
        listen = %listener.local_addr()?,
        public_origin = %cfg.public_origin,
        max_connections = cfg.max_connections,
        "relay listening"
    );
    let shared = Arc::new(Shared::new(cfg)?);
    let _ticker = AbortOnDrop(spawn_ticker(shared.clone()));
    let app = router(shared).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("relay stopped");
    Ok(())
}

/// Aborts its task when dropped, so the ticker never outlives the server.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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

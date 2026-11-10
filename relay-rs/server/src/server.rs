//! `serve`: binds the listen address and runs the axum app until SIGTERM or Ctrl-C.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

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
    let app = router(shared);
    let mut connections = JoinSet::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (socket, peer) = accepted?;
                let service = TowerToHyperService::new(app.clone().layer(axum::Extension(ConnectInfo::<SocketAddr>(peer))));
                connections.spawn(async move {
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
                    let _ = builder.serve_connection(TokioIo::new(socket), service).with_upgrades().await;
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
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

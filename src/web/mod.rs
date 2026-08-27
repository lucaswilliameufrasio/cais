//! Local web interface for cais.
//!
//! Exposes the same provisioning/migration/backup/restore logic as the TUI
//! through a REST API backed by an embedded HTML/JS single-page app. Secrets
//! are decrypted in memory only, after the user unlocks with the master
//! password over a bearer-token session.

pub mod api;
pub mod ops;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};

use state::WebState;

/// Runs the HTTP server until Ctrl+C (or SIGTERM), then wipes session keys.
pub fn serve(host: &str, port: u16, open_browser: bool) -> Result<()> {
    let state = Arc::new(WebState::new()?);
    let app = api::router(state.clone());

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address '{host}:{port}'"))?;
    let url = format!("http://{addr}");

    println!("cais web UI listening on {url}");
    if host == "0.0.0.0" {
        println!(
            "WARNING: binding to all interfaces. Anyone who can reach this port and knows the \
             master password can unlock the vault."
        );
    } else {
        println!("Bound to localhost only. Secrets are decrypted in memory while the server runs.");
    }

    if open_browser {
        open_in_browser(&url);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(state.clone()))
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;

    state.wipe_sessions();
    Ok(())
}

async fn shutdown_signal(state: Arc<WebState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("\nShutting down, wiping session keys...");
    state.wipe_sessions();
    // Browsers keep idle keep-alive connections open, so waiting for axum's
    // graceful drain would stall the shutdown forever (and further Ctrl+C
    // hits are swallowed by the installed handler). All secrets live in
    // memory only, so exiting the process right now is the safest shutdown.
    std::process::exit(0);
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn();
    }
}

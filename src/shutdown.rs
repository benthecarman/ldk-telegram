use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

/// Tells every part of the bot that it should stop.
#[derive(Clone)]
pub struct Shutdown {
	sender: Arc<watch::Sender<bool>>,
}

impl Shutdown {
	pub fn new() -> Self {
		Self { sender: Arc::new(watch::channel(false).0) }
	}

	pub fn trigger(&self) {
		self.sender.send_replace(true);
	}

	/// Resolves once a shutdown has been requested, immediately if it already was.
	pub async fn wait(&self) {
		let mut receiver = self.sender.subscribe();
		// The sender lives as long as `self`, so this can't fail.
		let _ = receiver.wait_for(|stopping| *stopping).await;
	}

	/// Triggers a shutdown on SIGTERM or SIGINT. A second signal exits immediately.
	pub fn listen_for_signals(&self) -> Result<()> {
		let mut sigterm =
			signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
		let mut sigint = signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
		let shutdown = self.clone();
		tokio::spawn(async move {
			tokio::select! {
				_ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
				_ = sigint.recv() => tracing::info!("received SIGINT, shutting down"),
			}
			shutdown.trigger();

			tokio::select! {
				_ = sigterm.recv() => {},
				_ = sigint.recv() => {},
			}
			tracing::warn!("received a second signal, exiting without a clean shutdown");
			std::process::exit(1);
		});
		Ok(())
	}
}

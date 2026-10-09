//! Integration with systemd's service manager. Every function is a no-op when the bot isn't
//! running under systemd.

use std::path::PathBuf;

use sd_notify::NotifyState;

/// Name of the credential (`LoadCredential=`) holding the Telegram bot token.
pub const BOT_TOKEN_CREDENTIAL: &str = "bot-token";
/// Name of the credential holding the ldk-server macaroon.
pub const MACAROON_CREDENTIAL: &str = "macaroon";
/// Name of the credential holding ldk-server's TLS certificate.
pub const TLS_CERT_CREDENTIAL: &str = "tls-cert";

fn notify(state: &[NotifyState]) {
	if let Err(e) = sd_notify::notify(state) {
		tracing::warn!("failed to notify systemd: {e}");
	}
}

/// Tells systemd the bot has started.
pub fn ready() {
	notify(&[NotifyState::Ready]);
}

/// Tells systemd the bot is shutting down.
pub fn stopping() {
	notify(&[NotifyState::Stopping]);
}

/// Sets the status line shown by `systemctl status`.
pub fn status(status: &str) {
	notify(&[NotifyState::Status(status)]);
}

/// Pings systemd's watchdog at half the configured `WatchdogSec`, if it is enabled.
pub fn spawn_watchdog() {
	let Some(timeout) = sd_notify::watchdog_enabled() else { return };
	tracing::info!("systemd watchdog enabled with a {timeout:?} timeout");
	tokio::spawn(async move {
		let mut ticker = tokio::time::interval(timeout / 2);
		loop {
			ticker.tick().await;
			notify(&[NotifyState::Watchdog]);
		}
	});
}

/// Path of a credential passed with `LoadCredential=` or `SetCredential=`, if it exists.
pub fn credential(name: &str) -> Option<PathBuf> {
	let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
	let path = PathBuf::from(dir).join(name);
	path.exists().then_some(path)
}

/// Whether log output goes to the journal, which adds its own timestamps.
pub fn logging_to_journal() -> bool {
	std::env::var_os("JOURNAL_STREAM").is_some()
}

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ldk_server_client::config as server_config;
use serde::Deserialize;

use crate::systemd;

/// Environment variable that can supply the Telegram bot token.
pub const BOT_TOKEN_ENV: &str = "LDK_TELEGRAM_BOT_TOKEN";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	pub telegram: TelegramConfig,
	#[serde(default)]
	pub ldk_server: LdkServerConfig,
	#[serde(default)]
	pub notifications: NotificationsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
	/// Bot token from @BotFather. Prefer `bot_token_file` or the environment variable so the
	/// token stays out of the config file.
	pub bot_token: Option<String>,
	/// File containing the bot token.
	pub bot_token_file: Option<PathBuf>,
	/// The only chat the bot posts to and accepts commands from. Send /connect to the bot to
	/// learn this value.
	pub chat_id: i64,
	/// Bot API server to use instead of Telegram's, such as a self-hosted telegram-bot-api.
	pub api_url: Option<url::Url>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LdkServerConfig {
	/// Path to ldk-server's own config file. Used to discover the gRPC address, TLS certificate,
	/// and admin macaroon when they aren't set explicitly below.
	pub config_path: Option<PathBuf>,
	/// gRPC address of ldk-server, e.g. `127.0.0.1:3536`.
	pub address: Option<String>,
	/// ldk-server's TLS certificate (PEM).
	pub cert_path: Option<PathBuf>,
	/// File containing the hex-encoded macaroon the bot authenticates with.
	pub macaroon_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
	pub payments_received: bool,
	pub payments_sent: bool,
	pub payments_failed: bool,
	pub payments_claimable: bool,
	pub channels: bool,
	pub forwards: bool,
	/// How often forwards are collected into a single digest message.
	pub forward_digest_interval_secs: u64,
	/// Forwards smaller than this are left out of the digest.
	pub min_forward_amount_sats: u64,
	/// Sent and received payments smaller than this are not posted.
	pub min_payment_amount_sats: u64,
	/// Post an alert once ldk-server has been unreachable for this long.
	pub offline_alert_after_secs: u64,
}

impl Default for NotificationsConfig {
	fn default() -> Self {
		Self {
			payments_received: true,
			payments_sent: true,
			payments_failed: true,
			payments_claimable: true,
			channels: true,
			forwards: true,
			forward_digest_interval_secs: 300,
			min_forward_amount_sats: 0,
			min_payment_amount_sats: 0,
			offline_alert_after_secs: 120,
		}
	}
}

impl NotificationsConfig {
	pub fn forward_digest_interval(&self) -> Duration {
		Duration::from_secs(self.forward_digest_interval_secs.max(1))
	}

	pub fn offline_alert_after(&self) -> Duration {
		Duration::from_secs(self.offline_alert_after_secs)
	}
}

/// Everything needed to build an ldk-server client.
pub struct Connection {
	pub address: String,
	pub cert_pem: Vec<u8>,
	pub macaroon: String,
}

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		let contents = std::fs::read_to_string(path)
			.with_context(|| format!("failed to read config file {}", path.display()))?;
		toml::from_str(&contents)
			.with_context(|| format!("failed to parse config file {}", path.display()))
	}
}

impl TelegramConfig {
	/// Uses, in order: `bot_token`, `bot_token_file`, the `bot-token` systemd credential, and the
	/// `LDK_TELEGRAM_BOT_TOKEN` environment variable.
	pub fn resolve_bot_token(&self) -> Result<String> {
		let token = match (&self.bot_token, &self.bot_token_file) {
			(Some(_), Some(_)) => {
				bail!("set only one of telegram.bot_token and telegram.bot_token_file")
			},
			(Some(token), None) => token.clone(),
			(None, Some(path)) => read_secret(path, "bot token")?,
			(None, None) => match systemd::credential(systemd::BOT_TOKEN_CREDENTIAL) {
				Some(path) => read_secret(&path, "bot token")?,
				None => std::env::var(BOT_TOKEN_ENV).with_context(|| {
					format!(
						"no bot token: set telegram.bot_token, telegram.bot_token_file, the \
						 {} credential, or {BOT_TOKEN_ENV}",
						systemd::BOT_TOKEN_CREDENTIAL
					)
				})?,
			},
		};
		let token = token.trim().to_string();
		if token.is_empty() {
			bail!("the Telegram bot token is empty");
		}
		Ok(token)
	}
}

impl LdkServerConfig {
	/// Explicit settings win, then systemd credentials, then what ldk-server's config implies.
	pub fn resolve(&self) -> Result<Connection> {
		let server = match &self.config_path {
			Some(path) => Some(server_config::load_config(path).map_err(anyhow::Error::msg)?),
			None => None,
		};

		let address = server_config::resolve_base_url(self.address.clone(), server.as_ref());

		let cert_override =
			self.cert_path.clone().or_else(|| systemd::credential(systemd::TLS_CERT_CREDENTIAL));
		let cert_path = server_config::resolve_cert_path(cert_override, server.as_ref())
			.context("could not find ldk-server's TLS certificate; set ldk_server.cert_path")?;
		let cert_pem =
			server_config::read_tls_certificate(&cert_path).map_err(anyhow::Error::msg)?;

		let macaroon_path = self
			.macaroon_path
			.clone()
			.or_else(|| systemd::credential(systemd::MACAROON_CREDENTIAL));
		let macaroon = match macaroon_path {
			Some(path) => read_secret(&path, "macaroon")?.trim().to_string(),
			None => server_config::resolve_macaroon(None, server.as_ref())
				.map_err(anyhow::Error::msg)?
				.context("could not find a macaroon; set ldk_server.macaroon_path")?,
		};

		Ok(Connection { address, cert_pem, macaroon })
	}
}

fn read_secret(path: &Path, what: &str) -> Result<String> {
	std::fs::read_to_string(path)
		.with_context(|| format!("failed to read {what} file {}", path.display()))
}

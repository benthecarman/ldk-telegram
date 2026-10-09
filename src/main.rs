mod commands;
mod config;
mod events;
mod format;
mod forwards;
mod node;
mod shutdown;
mod systemd;
mod telegram;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ldk_server_client::ldk_server_grpc::api::GetNodeInfoRequest;
use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;
use teloxide::update_listeners::Polling;
use teloxide::utils::command::BotCommands;
use tokio::time::{timeout, timeout_at, Instant};
use tracing_subscriber::EnvFilter;

use crate::commands::{Command, State};
use crate::config::Config;
use crate::format::{escape, short_id};
use crate::forwards::ForwardDigest;
use crate::node::Node;
use crate::shutdown::Shutdown;
use crate::telegram::{Notifier, TgBot};

/// Posts LDK Server activity to Telegram and answers commands about the node.
#[derive(Parser)]
#[command(version)]
struct Args {
	/// Path to the bot's TOML config file.
	#[arg(short, long, env = "LDK_TELEGRAM_CONFIG", default_value = "ldk-telegram.toml")]
	config: PathBuf,
}

/// How long a clean shutdown may take. Docker sends SIGKILL 10 seconds after SIGTERM by default.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
/// The share of [`SHUTDOWN_GRACE`] left for posting the final forward digest.
const FLUSH_GRACE: Duration = Duration::from_secs(2);
/// Telegram long-poll timeout. Stopping waits for the poll in flight, so keep this short.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_RPC_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();
	init_logging();

	// Listen for signals first so a stop request during startup is honored.
	let shutdown = Shutdown::new();
	shutdown.listen_for_signals()?;
	systemd::spawn_watchdog();
	systemd::status("Starting");

	let config = Config::load(&args.config)?;
	let token = config.telegram.resolve_bot_token()?;
	let node = Arc::new(Node::new(config.ldk_server.resolve()?)?);
	let chat_id = ChatId(config.telegram.chat_id);

	let mut bot = Bot::new(token);
	if let Some(api_url) = config.telegram.api_url.clone() {
		bot = bot.set_api_url(api_url);
	}
	let bot = bot.throttle(Limits::default());
	let notifier = Notifier::new(bot.clone(), chat_id);

	let listener = tokio::select! {
		listener = start(&bot, &node, &notifier) => listener?,
		() = shutdown.wait() => {
			tracing::info!("stopped during startup");
			return Ok(());
		},
	};

	let notifications = config.notifications;
	let digest = Arc::new(ForwardDigest::new(notifications.min_forward_amount_sats));
	let digest_task = notifications.forwards.then(|| {
		let interval = notifications.forward_digest_interval();
		tokio::spawn(digest.clone().run(node.clone(), notifier.clone(), interval))
	});
	let events = tokio::spawn(events::run(
		node.clone(),
		notifier.clone(),
		notifications,
		digest.clone(),
		shutdown.clone(),
	));

	let state = Arc::new(State {
		node: node.clone(),
		chat_id,
		shutdown: shutdown.clone(),
		started_at: format::now_secs(),
	});
	let handler = Update::filter_message().filter_command::<Command>().endpoint(commands::handle);
	let mut dispatcher = Dispatcher::builder(bot, handler)
		.dependencies(dptree::deps![state])
		.default_handler(|_| async {})
		.build();
	let dispatcher_token = dispatcher.shutdown_token();
	let dispatch = dispatcher.try_dispatch_with_listener(
		listener,
		LoggingErrorHandler::with_custom_text("error from the Telegram update listener"),
	);
	tokio::pin!(dispatch);

	systemd::ready();
	systemd::status("Running");
	let mut failure = None;
	tokio::select! {
		result = &mut dispatch => {
			// Exit with an error after cleaning up so systemd restarts the bot.
			failure = Some(match result {
				Ok(()) => anyhow!("the Telegram dispatcher stopped unexpectedly"),
				Err(e) => anyhow!("the Telegram dispatcher failed to start: {e}"),
			});
		},
		() = shutdown.wait() => {},
	}

	systemd::stopping();
	shutdown.trigger();
	let deadline = Instant::now() + SHUTDOWN_GRACE;
	let flush_deadline = deadline - FLUSH_GRACE;

	// Stop taking commands, let running ones finish, and tell Telegram which updates were
	// handled so they aren't delivered again after a restart. The event loop finishes the event
	// it is handling at the same time.
	let stop_dispatcher = async {
		match dispatcher_token.shutdown() {
			Ok(done) => {
				let finished = async {
					let ((), _) = tokio::join!(done, &mut dispatch);
				};
				if timeout_at(flush_deadline, finished).await.is_err() {
					tracing::warn!("commands still running at shutdown were abandoned");
				}
			},
			// The dispatcher only counts as running once its own getMe call returns. Until then
			// it hasn't taken any updates, so there is nothing to finish.
			Err(_) => tracing::info!("Telegram dispatcher hadn't started"),
		}
	};
	let stop_events = async {
		if timeout_at(flush_deadline, events).await.is_err() {
			tracing::warn!("an event still being handled at shutdown was abandoned");
		}
	};
	tokio::join!(stop_dispatcher, stop_events);

	// Post forwards collected since the last digest.
	if let Some(task) = digest_task {
		task.abort();
	}
	if timeout_at(deadline, digest.flush(&node, &notifier)).await.is_err() {
		tracing::warn!("failed to post the final forward digest in time");
	}

	match failure {
		Some(failure) => Err(failure),
		None => {
			tracing::info!("stopped");
			Ok(())
		},
	}
}

/// Connects to Telegram, registers commands, and posts the startup message.
async fn start(bot: &TgBot, node: &Node, notifier: &Notifier) -> Result<Polling<TgBot>> {
	let me = bot.get_me().await.context("failed to reach Telegram; check the bot token")?;
	tracing::info!("running as @{}", me.username());
	if let Err(e) = bot.set_my_commands(Command::bot_commands()).await {
		tracing::warn!("failed to register bot commands: {e}");
	}

	match timeout(STARTUP_RPC_TIMEOUT, node.client.get_node_info(GetNodeInfoRequest {})).await {
		Ok(Ok(info)) => {
			let alias = info.node_alias.filter(|a| !a.is_empty());
			let name = match alias {
				Some(alias) => format!("<b>{}</b> ", escape(&alias)),
				None => String::new(),
			};
			notifier
				.send(format!("🤖 Connected to {name}<code>{}</code>", short_id(&info.node_id)))
				.await;
		},
		Ok(Err(e)) => {
			tracing::warn!("ldk-server is unreachable: {e}");
			notifier
				.send(format!("🤖 Started, but ldk-server is unreachable: {}", escape(&e.message)))
				.await;
		},
		Err(_) => {
			tracing::warn!("ldk-server didn't respond");
			notifier.send("🤖 Started, but ldk-server isn't responding").await;
		},
	}

	Ok(Polling::builder(bot.clone()).timeout(POLL_TIMEOUT).delete_webhook().await.build())
}

fn init_logging() {
	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
	let builder = tracing_subscriber::fmt()
		.with_env_filter(filter)
		.with_writer(std::io::stderr)
		.with_ansi(std::io::stderr().is_terminal());
	if systemd::logging_to_journal() {
		// The journal records its own timestamps.
		builder.without_time().init();
	} else {
		builder.init();
	}
}

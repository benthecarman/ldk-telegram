use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ldk_server_client::ldk_server_grpc::api::{
	GetBalancesRequest, GetForwardedPaymentTrackingModeRequest, GetNodeInfoRequest,
	GraphGetNodeRequest, ListPeersRequest,
};
use ldk_server_client::ldk_server_grpc::types::lightning_balance::BalanceType as LightningBalanceType;
use ldk_server_client::ldk_server_grpc::types::payment_kind::Kind;
use ldk_server_client::ldk_server_grpc::types::pending_sweep_balance::BalanceType as SweepBalanceType;
use ldk_server_client::ldk_server_grpc::types::{
	Channel, ChannelShutdownState, ForwardedPaymentTrackingMode, Network, PaymentDirection,
	PaymentStatus, PendingSweepBalance,
};
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use teloxide::utils::command::BotCommands;

use crate::format::{ago, duration, escape, group, msat, now_secs, sats};
use crate::node::Node;
use crate::shutdown::Shutdown;
use crate::telegram::{send_html, TgBot};

const DAY_SECS: u64 = 24 * 60 * 60;
const WEEK_SECS: u64 = 7 * DAY_SECS;
const TOP_PEERS: usize = 5;
/// Width of the outbound/inbound bar drawn for each channel in /channels.
const BAR_WIDTH: u64 = 10;
/// Average time between blocks, for turning block counts into rough durations.
const BLOCK_SECS: u64 = 10 * 60;
/// Wallets normally sync every few seconds to a couple of minutes, depending on the backend.
const STALE_SYNC_SECS: u64 = 10 * 60;
/// Longest a command may take. Shutdown waits for running commands, so keep this bounded.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
	#[command(description = "Show funds on the node")]
	Balance,
	#[command(description = "List channels with their balances")]
	Channels,
	#[command(description = "Get the chat ID to put in the config")]
	Connect,
	#[command(description = "Show payment fees paid over the past week")]
	Costs,
	#[command(description = "Show forwarding earnings over the past week")]
	Earnings,
	#[command(description = "Show info about a node by pubkey or peer alias")]
	Graph(String),
	#[command(description = "Show this list of commands")]
	Help,
	#[command(description = "Show node info")]
	Info,
	#[command(description = "Show channel liquidity, optionally with one peer")]
	Liquidity(String),
	#[command(description = "Show pending channels, HTLCs, and sweeps")]
	Pending,
	#[command(description = "Show the list of commands")]
	Start,
	#[command(description = "Stop the bot")]
	Stop,
	#[command(description = "Show bot and ldk-server versions")]
	Version,
}

pub struct State {
	pub node: Arc<Node>,
	pub chat_id: ChatId,
	pub shutdown: Shutdown,
	/// Unix time this process started, used to ignore a /stop meant for an earlier run.
	pub started_at: u64,
}

pub async fn handle(
	bot: TgBot, msg: Message, cmd: Command, state: Arc<State>,
) -> ResponseResult<()> {
	let authorized = msg.chat.id == state.chat_id;
	if !authorized {
		// Anyone may learn their own chat ID so they can configure the bot; ignore everything
		// else from chats that aren't configured.
		if matches!(cmd, Command::Connect | Command::Start) {
			send_html(&bot, msg.chat.id, connect_text(msg.chat.id)).await;
		}
		return Ok(());
	}

	let reply = match cmd {
		Command::Help | Command::Start => Ok(help_text()),
		Command::Connect => Ok("✅ This chat is connected.".to_string()),
		Command::Stop => {
			// Telegram redelivers updates that weren't acknowledged before an unclean exit. Don't
			// let an old /stop stop this run as well.
			let sent_at = u64::try_from(msg.date.timestamp()).unwrap_or(0);
			if sent_at < state.started_at {
				tracing::info!("ignoring a /stop sent before this run started");
				return Ok(());
			}
			send_html(&bot, msg.chat.id, "🛑 Stopping the bot.").await;
			tracing::info!("stop requested from Telegram");
			state.shutdown.trigger();
			return Ok(());
		},
		cmd => {
			bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();
			let node = &state.node;
			let run = async {
				match cmd {
					Command::Balance => balance(node).await,
					Command::Channels => channels(node).await,
					Command::Costs => costs(node).await,
					Command::Earnings => earnings(node).await,
					Command::Graph(query) => graph(node, query.trim()).await,
					Command::Info => info(node).await,
					Command::Liquidity(query) => liquidity(node, query.trim()).await,
					Command::Pending => pending(node).await,
					Command::Version => version(node).await,
					Command::Help | Command::Start | Command::Connect | Command::Stop => {
						unreachable!("handled above")
					},
				}
			};
			match tokio::time::timeout(COMMAND_TIMEOUT, run).await {
				Ok(reply) => reply,
				Err(_) => Err(anyhow!("ldk-server didn't answer in time")),
			}
		},
	};

	let text = reply.unwrap_or_else(|e| {
		tracing::warn!("command failed: {e:#}");
		format!("⚠️ {}", escape(&format!("{e:#}")))
	});
	send_html(&bot, msg.chat.id, text).await;
	Ok(())
}

fn connect_text(chat_id: ChatId) -> String {
	format!(
		"🤖 This chat's ID is <code>{chat_id}</code>.\n\nSet <code>chat_id = {chat_id}</code> in the \
		 <code>[telegram]</code> section of the bot's config and restart it."
	)
}

fn help_text() -> String {
	let mut text = "🤖 <b>Commands</b>\n".to_string();
	for command in Command::bot_commands() {
		if command.command == "/start" {
			continue;
		}
		text.push_str(&format!("\n{} - {}", command.command, escape(&command.description)));
	}
	text
}

async fn info(node: &Node) -> Result<String> {
	let info = node.client.get_node_info(GetNodeInfoRequest {}).await?;
	let channels = node.list_channels().await?;
	let peers = node.client.list_peers(ListPeersRequest {}).await?.peers;

	let count = |status| channels.iter().filter(|c| ChannelStatus::of(c) == status).count();
	let mut channel_counts = format!(
		"{} active, {} inactive, {} pending",
		count(ChannelStatus::Active),
		count(ChannelStatus::Inactive),
		count(ChannelStatus::Opening)
	);
	let closing = count(ChannelStatus::Closing);
	if closing > 0 {
		channel_counts.push_str(&format!(", {closing} closing"));
	}
	let connected = peers.iter().filter(|p| p.is_connected).count();
	let network = Network::from_i32(info.network)
		.map(|n| n.as_str_name().to_lowercase())
		.unwrap_or_else(|| "unknown".to_string());
	let alias = info.node_alias.as_deref().filter(|a| !a.is_empty()).unwrap_or("(no alias)");

	let mut text = format!(
		"🤖 <b>{}</b>\n<code>{}</code>\n\n\
		 Network: {network}\n\
		 Block: {}\n\
		 Channels: {channel_counts}\n\
		 Peers: {connected} connected\n\
		 Version: {}",
		escape(alias),
		info.node_id,
		info.current_best_block.map(|b| group(b.height.into())).unwrap_or_default(),
		escape(&info.version),
	);
	let now = now_secs();
	let syncs = [
		("Lightning", info.latest_lightning_wallet_sync_timestamp),
		("On-chain", info.latest_onchain_wallet_sync_timestamp),
	];
	for (wallet, timestamp) in syncs {
		if let Some(warning) = stale_sync_warning(wallet, timestamp, now) {
			text.push_str(&format!("\n{warning}"));
		}
	}
	for uri in &info.node_uris {
		text.push_str(&format!("\n<code>{}</code>", escape(uri)));
	}
	Ok(text)
}

/// Warns about a wallet that hasn't synced for a while. Recent syncs aren't worth showing:
/// with a bitcoind backend they happen every few seconds.
fn stale_sync_warning(wallet: &str, timestamp: Option<u64>, now: u64) -> Option<String> {
	let age = now.saturating_sub(timestamp?);
	(age >= STALE_SYNC_SECS)
		.then(|| format!("⚠️ {wallet} wallet last synced {} ago", duration(age)))
}

async fn balance(node: &Node) -> Result<String> {
	let balances = node.client.get_balances(GetBalancesRequest {}).await?;
	let total = balances.total_onchain_balance_sats + balances.total_lightning_balance_sats;
	let sweeping: u64 =
		balances.pending_balances_from_channel_closures.iter().map(sweep_amount).sum();

	let mut text = format!(
		"💰 <b>Balance: {}</b>\n\n⚡ Lightning: {}\n⛓ On-chain: {} ({} spendable)",
		sats(total),
		sats(balances.total_lightning_balance_sats),
		sats(balances.total_onchain_balance_sats),
		sats(balances.spendable_onchain_balance_sats),
	);
	if balances.total_anchor_channels_reserve_sats > 0 {
		text.push_str(&format!(
			"\n⚓ Anchor reserve: {}",
			sats(balances.total_anchor_channels_reserve_sats)
		));
	}
	if sweeping > 0 {
		text.push_str(&format!("\n🧹 Sweeping from closed channels: {}", sats(sweeping)));
	}
	Ok(text)
}

/// Where a channel is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelStatus {
	Active,
	Inactive,
	Opening,
	Closing,
}

impl ChannelStatus {
	fn of(channel: &Channel) -> Self {
		if is_closing(channel) {
			Self::Closing
		} else if !channel.is_channel_ready {
			Self::Opening
		} else if !channel.is_usable {
			Self::Inactive
		} else {
			Self::Active
		}
	}
}

async fn channels(node: &Node) -> Result<String> {
	let mut channels = node.list_channels().await?;
	if channels.is_empty() {
		return Ok("No channels.".to_string());
	}
	channels.sort_by_key(|c| Reverse(c.channel_value_sats));
	let height = node.client.get_node_info(GetNodeInfoRequest {}).await?.current_best_block;
	let height = height.map(|b| b.height);

	let capacity: u64 = channels.iter().map(|c| c.channel_value_sats).sum();
	let noun = if channels.len() == 1 { "channel" } else { "channels" };
	let counts: Vec<_> = [
		(ChannelStatus::Active, "active"),
		(ChannelStatus::Inactive, "inactive"),
		(ChannelStatus::Opening, "opening"),
		(ChannelStatus::Closing, "closing"),
	]
	.into_iter()
	.filter_map(|(status, name)| {
		let count = channels.iter().filter(|c| ChannelStatus::of(c) == status).count();
		(count > 0).then(|| format!("{count} {name}"))
	})
	.collect();
	let mut text =
		format!("📊 <b>{} {noun}</b>: {}\n{}", channels.len(), sats(capacity), counts.join(", "));

	for channel in &channels {
		let status = ChannelStatus::of(channel);
		let icon = match status {
			ChannelStatus::Active => "🟢",
			ChannelStatus::Inactive => "🔴",
			ChannelStatus::Opening => "⏳",
			ChannelStatus::Closing => "🥀",
		};
		let private = if channel.is_announced { "" } else { " 🕶️" };
		let age = match (channel.short_channel_id, height) {
			(Some(scid), Some(height)) => format!(" · {} old", duration(channel_age(scid, height))),
			_ => String::new(),
		};
		text.push_str(&format!(
			"\n\n{icon} <b>{}</b>: {}{age}{private}\n",
			node.label(&channel.counterparty_node_id).await,
			sats(channel.channel_value_sats)
		));
		match status {
			ChannelStatus::Opening => {
				text.push_str("Opening");
				if let (Some(have), Some(need)) =
					(channel.confirmations, channel.confirmations_required)
				{
					text.push_str(&format!(" ({have}/{need} confirmations)"));
				}
			},
			ChannelStatus::Closing => text.push_str("Closing"),
			ChannelStatus::Active | ChannelStatus::Inactive => {
				text.push_str(&format!(
					"{} {} out, {} in",
					balance_bar(channel.outbound_capacity_msat, channel.inbound_capacity_msat),
					msat(channel.outbound_capacity_msat),
					msat(channel.inbound_capacity_msat)
				));
				let ppm = channel
					.channel_config
					.as_ref()
					.and_then(|c| c.forwarding_fee_proportional_millionths);
				if let Some(ppm) = ppm {
					text.push_str(&format!(" · {} ppm", group(ppm.into())));
				}
			},
		}
	}
	Ok(text)
}

/// A bar showing how much of a channel's spendable balance is on our side.
fn balance_bar(outbound_msat: u64, inbound_msat: u64) -> String {
	let total = u128::from(outbound_msat) + u128::from(inbound_msat);
	let filled = match total {
		0 => 0,
		// Round to the nearest cell.
		_ => ((u128::from(outbound_msat) * u128::from(BAR_WIDTH) + total / 2) / total) as u64,
	};
	"▰".repeat(filled as usize) + &"▱".repeat((BAR_WIDTH - filled) as usize)
}

/// Roughly how long ago a channel's funding confirmed, from the block height in its short
/// channel ID.
fn channel_age(short_channel_id: u64, height: u32) -> u64 {
	let funded_at = short_channel_id >> 40;
	u64::from(height).saturating_sub(funded_at) * BLOCK_SECS
}

fn is_closing(channel: &Channel) -> bool {
	matches!(
		channel.channel_shutdown_state.and_then(ChannelShutdownState::from_i32),
		Some(ChannelShutdownState::ShutdownInitiated)
			| Some(ChannelShutdownState::ResolvingHtlcs)
			| Some(ChannelShutdownState::NegotiatingClosingFee)
			| Some(ChannelShutdownState::ShutdownComplete)
	)
}

async fn liquidity(node: &Node, query: &str) -> Result<String> {
	let channels: Vec<_> =
		node.list_channels().await?.into_iter().filter(|c| c.is_channel_ready).collect();

	if query.is_empty() {
		let outbound: u64 = channels.iter().map(|c| c.outbound_capacity_msat).sum();
		let inbound: u64 = channels.iter().map(|c| c.inbound_capacity_msat).sum();
		return Ok(format!(
			"🌊 <b>Liquidity</b>\n\nOutbound: {}\nInbound: {}",
			msat(outbound),
			msat(inbound)
		));
	}

	let peers =
		find_peers(node, query, channels.iter().map(|c| c.counterparty_node_id.clone())).await;
	if peers.is_empty() {
		return Ok(format!("No channel peer matches “{}”.", escape(query)));
	}

	let mut text = "🌊 <b>Liquidity</b>\n".to_string();
	for peer in peers {
		let with_peer = channels.iter().filter(|c| c.counterparty_node_id == peer);
		let (outbound, inbound) = with_peer.fold((0, 0), |(out, inb), c| {
			(out + c.outbound_capacity_msat, inb + c.inbound_capacity_msat)
		});
		text.push_str(&format!(
			"\n{}: outbound {}, inbound {}",
			node.label(&peer).await,
			msat(outbound),
			msat(inbound)
		));
	}
	Ok(text)
}

async fn pending(node: &Node) -> Result<String> {
	let channels = node.list_channels().await?;
	let balances = node.client.get_balances(GetBalancesRequest {}).await?;
	let mut lines = Vec::new();

	let opening = channels.iter().filter(|c| ChannelStatus::of(c) == ChannelStatus::Opening);
	for channel in opening {
		let confirmations = match (channel.confirmations, channel.confirmations_required) {
			(Some(have), Some(need)) => format!(" ({have}/{need} confirmations)"),
			_ => String::new(),
		};
		lines.push(format!(
			"⏳ Opening with {}: {}{confirmations}",
			node.label(&channel.counterparty_node_id).await,
			sats(channel.channel_value_sats)
		));
	}

	for channel in channels.iter().filter(|c| is_closing(c)) {
		lines.push(format!(
			"🥀 Closing with {}: {}",
			node.label(&channel.counterparty_node_id).await,
			sats(channel.channel_value_sats)
		));
	}

	let (mut htlcs_out, mut htlcs_in) = (0, 0);
	for balance in balances.lightning_balances.iter().filter_map(|b| b.balance_type.as_ref()) {
		match balance {
			LightningBalanceType::ClaimableOnChannelClose(b) => {
				htlcs_out +=
					b.outbound_payment_htlc_rounded_msat + b.outbound_forwarded_htlc_rounded_msat;
				htlcs_in += b.inbound_claiming_htlc_rounded_msat + b.inbound_htlc_rounded_msat;
			},
			LightningBalanceType::ClaimableAwaitingConfirmations(b) => lines.push(format!(
				"⛓ {} from {} claimable at block {}",
				sats(b.amount_satoshis),
				node.label(&b.counterparty_node_id).await,
				group(b.confirmation_height.into())
			)),
			LightningBalanceType::ContentiousClaimable(b) => lines.push(format!(
				"⚔️ {} contested with {} until block {}",
				sats(b.amount_satoshis),
				node.label(&b.counterparty_node_id).await,
				group(b.timeout_height.into())
			)),
			LightningBalanceType::MaybeTimeoutClaimableHtlc(b) => lines.push(format!(
				"⌛ {} HTLC with {} claimable on timeout at block {}",
				sats(b.amount_satoshis),
				node.label(&b.counterparty_node_id).await,
				group(b.claimable_height.into())
			)),
			LightningBalanceType::MaybePreimageClaimableHtlc(b) => lines.push(format!(
				"🔑 {} HTLC with {} claimable with preimage before block {}",
				sats(b.amount_satoshis),
				node.label(&b.counterparty_node_id).await,
				group(b.expiry_height.into())
			)),
			LightningBalanceType::CounterpartyRevokedOutputClaimable(b) => lines.push(format!(
				"🚨 {} claimable from revoked state of {}",
				sats(b.amount_satoshis),
				node.label(&b.counterparty_node_id).await
			)),
		}
	}
	if htlcs_out > 0 || htlcs_in > 0 {
		lines.push(format!(
			"🔄 In-flight HTLCs: {} outgoing, {} incoming",
			msat(htlcs_out),
			msat(htlcs_in)
		));
	}

	let sweeping: u64 =
		balances.pending_balances_from_channel_closures.iter().map(sweep_amount).sum();
	if sweeping > 0 {
		lines.push(format!("🧹 Sweeping {} from closed channels", sats(sweeping)));
	}

	if lines.is_empty() {
		return Ok("✅ Nothing pending.".to_string());
	}
	Ok(format!("⏳ <b>Pending</b>\n\n{}", lines.join("\n")))
}

/// Forwarding totals for one time window.
#[derive(Default)]
struct Totals {
	count: u64,
	volume_msat: u64,
	fees_msat: u64,
}

impl Totals {
	fn add(&mut self, count: u64, volume_msat: u64, fees_msat: u64) {
		self.count += count;
		self.volume_msat += volume_msat;
		self.fees_msat += fees_msat;
	}
}

async fn earnings(node: &Node) -> Result<String> {
	let mode = node
		.client
		.get_forwarded_payment_tracking_mode(GetForwardedPaymentTrackingModeRequest {})
		.await?
		.mode;
	if ForwardedPaymentTrackingMode::from_i32(mode) != Some(ForwardedPaymentTrackingMode::Detailed)
	{
		return lifetime_earnings(node).await;
	}

	let now = now_secs();
	let (mut day, mut week) = (Totals::default(), Totals::default());
	let mut by_peer: HashMap<String, u64> = HashMap::new();
	let mut record = |timestamp: u64, count, volume, fees, out_node: Option<String>| {
		let age = now.saturating_sub(timestamp);
		if age < DAY_SECS {
			day.add(count, volume, fees);
		}
		if age < WEEK_SECS {
			week.add(count, volume, fees);
			if let Some(out_node) = out_node {
				*by_peer.entry(out_node).or_default() += fees;
			}
		}
	};

	// Older forwards are aggregated into hourly buckets; recent ones are stored individually.
	for bucket in node.list_channel_pair_forwarding_stats().await? {
		record(
			bucket.bucket_start_timestamp,
			bucket.payment_count,
			bucket.total_outbound_amount_msat,
			bucket.total_fee_earned_msat.unwrap_or(0),
			bucket.next_node_id,
		);
	}
	for forward in node.list_forwarded_payments().await? {
		record(
			forward.forwarded_at_timestamp,
			1,
			forward.outbound_amount_forwarded_msat.unwrap_or(0),
			forward.total_fee_earned_msat.unwrap_or(0),
			forward.next_node_id,
		);
	}

	let mut text = format!(
		"📈 <b>Earnings</b>\n\nPast day: {}\nPast week: {}",
		describe_totals(&day),
		describe_totals(&week)
	);

	let mut top: Vec<_> = by_peer.into_iter().filter(|(_, fees)| *fees > 0).collect();
	top.sort_by_key(|(_, fees)| std::cmp::Reverse(*fees));
	if !top.is_empty() {
		text.push_str("\n\n<b>Top outgoing peers this week</b>");
		for (peer, fees) in top.into_iter().take(TOP_PEERS) {
			text.push_str(&format!("\n{}: {}", node.label(&peer).await, msat(fees)));
		}
	}
	Ok(text)
}

/// Without detailed tracking ldk-server only keeps lifetime per-channel totals.
async fn lifetime_earnings(node: &Node) -> Result<String> {
	let mut totals = Totals::default();
	for stats in node.list_channel_forwarding_stats().await? {
		// Fees are attributed to the incoming channel, so count each forward there once.
		totals.add(
			stats.inbound_payments_forwarded,
			stats.total_inbound_amount_msat,
			stats.total_fee_earned_msat.unwrap_or(0),
		);
	}
	Ok(format!(
		"📈 <b>Earnings</b>\n\nAll time: {}\n\n<i>Weekly totals need \
		 <code>forwarded_payment_tracking_mode = \"detailed\"</code> in ldk-server's config.</i>",
		describe_totals(&totals)
	))
}

fn describe_totals(totals: &Totals) -> String {
	let noun = if totals.count == 1 { "forward" } else { "forwards" };
	format!(
		"{} earned from {} {noun} ({} routed)",
		msat(totals.fees_msat),
		group(totals.count),
		msat(totals.volume_msat)
	)
}

async fn costs(node: &Node) -> Result<String> {
	let now = now_secs();
	let (mut day, mut week) = ([0u64; 2], [0u64; 2]);
	for payment in node.list_payments().await? {
		let outbound =
			PaymentDirection::from_i32(payment.direction) == Some(PaymentDirection::Outbound);
		let succeeded = PaymentStatus::from_i32(payment.status) == Some(PaymentStatus::Succeeded);
		let Some(fee) = payment.fee_paid_msat.filter(|_| outbound && succeeded) else { continue };
		let onchain = matches!(payment.kind.and_then(|k| k.kind), Some(Kind::Onchain(_)));
		let slot = usize::from(onchain);
		let age = now.saturating_sub(payment.latest_update_timestamp);
		if age < DAY_SECS {
			day[slot] += fee;
		}
		if age < WEEK_SECS {
			week[slot] += fee;
		}
	}

	let describe = |[lightning, onchain]: [u64; 2]| {
		format!(
			"{} ({} lightning, {} on-chain)",
			msat(lightning + onchain),
			msat(lightning),
			msat(onchain)
		)
	};
	Ok(format!("📉 <b>Fees paid</b>\n\nPast day: {}\nPast week: {}", describe(day), describe(week)))
}

async fn graph(node: &Node, query: &str) -> Result<String> {
	if query.is_empty() {
		return Ok("Usage: /graph &lt;pubkey or peer alias&gt;".to_string());
	}

	let channels = node.list_channels().await?;
	let peers = node.client.list_peers(ListPeersRequest {}).await?.peers;

	let node_id = if is_pubkey(query) {
		query.to_lowercase()
	} else {
		let candidates = channels
			.iter()
			.map(|c| c.counterparty_node_id.clone())
			.chain(peers.iter().map(|p| p.node_id.clone()));
		let mut matches = find_peers(node, query, candidates).await;
		match matches.len() {
			0 => return Ok(format!("No peer matches “{}”.", escape(query))),
			1 => matches.remove(0),
			_ => {
				let mut text = format!("Multiple peers match “{}”:", escape(query));
				for peer in &matches {
					text.push_str(&format!("\n{} <code>{peer}</code>", node.label(peer).await));
				}
				return Ok(text);
			},
		}
	};

	let graph_node = node
		.client
		.graph_get_node(GraphGetNodeRequest { node_id: node_id.clone() })
		.await
		.ok()
		.and_then(|r| r.node);

	let mut text = match &graph_node {
		Some(graph_node) => {
			let announcement = graph_node.announcement_info.as_ref();
			let alias = announcement.map(|a| a.alias.trim()).filter(|a| !a.is_empty());
			let mut text = format!(
				"🌐 <b>{}</b>\n<code>{node_id}</code>\n\nPublic channels: {}",
				escape(alias.unwrap_or("(no alias)")),
				graph_node.channels.len()
			);
			if let Some(announcement) = announcement {
				text.push_str(&format!("\nLast update: {}", ago(announcement.last_update.into())));
				for address in &announcement.addresses {
					text.push_str(&format!("\n<code>{}</code>", escape(address)));
				}
			}
			text
		},
		None => format!("🌐 <code>{node_id}</code>\n\nNot found in the network graph."),
	};

	let ours: Vec<_> = channels.iter().filter(|c| c.counterparty_node_id == node_id).collect();
	if !ours.is_empty() {
		let capacity: u64 = ours.iter().map(|c| c.channel_value_sats).sum();
		let outbound: u64 = ours.iter().map(|c| c.outbound_capacity_msat).sum();
		let inbound: u64 = ours.iter().map(|c| c.inbound_capacity_msat).sum();
		text.push_str(&format!(
			"\n\nOur channels: {} ({} total, outbound {}, inbound {})",
			ours.len(),
			sats(capacity),
			msat(outbound),
			msat(inbound)
		));
	}
	let status = match peers.iter().find(|p| p.node_id == node_id) {
		Some(peer) if peer.is_connected => "connected",
		Some(_) => "disconnected",
		None => "not a peer",
	};
	text.push_str(&format!("\nPeer: {status}"));
	Ok(text)
}

async fn version(node: &Node) -> Result<String> {
	let info = node.client.get_node_info(GetNodeInfoRequest {}).await.context("ldk-server")?;
	Ok(format!(
		"🤖 ldk-telegram {}\nldk-server {}",
		env!("CARGO_PKG_VERSION"),
		escape(&info.version)
	))
}

fn sweep_amount(balance: &PendingSweepBalance) -> u64 {
	match &balance.balance_type {
		Some(SweepBalanceType::PendingBroadcast(b)) => b.amount_satoshis,
		Some(SweepBalanceType::BroadcastAwaitingConfirmation(b)) => b.amount_satoshis,
		Some(SweepBalanceType::AwaitingThresholdConfirmations(b)) => b.amount_satoshis,
		None => 0,
	}
}

fn is_pubkey(s: &str) -> bool {
	s.len() == 66 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Finds node IDs whose public key starts with, or alias contains, `query`.
async fn find_peers(
	node: &Node, query: &str, candidates: impl Iterator<Item = String>,
) -> Vec<String> {
	let query = query.to_lowercase();
	let mut matches = Vec::new();
	for node_id in candidates.collect::<BTreeSet<_>>() {
		let alias_matches = match node.alias(&node_id).await {
			Some(alias) => alias.to_lowercase().contains(&query),
			None => false,
		};
		if node_id.starts_with(&query) || alias_matches {
			matches.push(node_id);
		}
	}
	matches
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn draws_balance_bars() {
		assert_eq!(balance_bar(0, 0), "▱▱▱▱▱▱▱▱▱▱");
		assert_eq!(balance_bar(0, 1_000), "▱▱▱▱▱▱▱▱▱▱");
		assert_eq!(balance_bar(1_000, 0), "▰▰▰▰▰▰▰▰▰▰");
		assert_eq!(balance_bar(1_000, 3_000), "▰▰▰▱▱▱▱▱▱▱");
		assert_eq!(balance_bar(u64::MAX, u64::MAX), "▰▰▰▰▰▱▱▱▱▱");
	}

	#[test]
	fn ages_channels_by_funding_height() {
		// Block 800,000, transaction 1,234, output 1.
		let scid = (800_000 << 40) | (1_234 << 16) | 1;
		assert_eq!(channel_age(scid, 800_000), 0);
		assert_eq!(channel_age(scid, 800_144), DAY_SECS);
		assert_eq!(channel_age(scid, 799_000), 0);
	}

	#[test]
	fn only_warns_about_stale_syncs() {
		let now = 1_000_000;
		assert_eq!(stale_sync_warning("Lightning", None, now), None);
		assert_eq!(stale_sync_warning("Lightning", Some(now), now), None);
		assert_eq!(stale_sync_warning("Lightning", Some(now - 599), now), None);
		assert_eq!(
			stale_sync_warning("On-chain", Some(now - 2 * 3_600), now),
			Some("⚠️ On-chain wallet last synced 2h ago".to_string())
		);
	}
}

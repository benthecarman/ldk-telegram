use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::LdkServerError;
use ldk_server_client::ldk_server_grpc::api::{
	GraphGetNodeRequest, ListChannelForwardingStatsRequest, ListChannelPairForwardingStatsRequest,
	ListChannelsRequest, ListForwardedPaymentsRequest, ListPaymentsRequest,
};
use ldk_server_client::ldk_server_grpc::types::{
	Channel, ChannelForwardingStats, ChannelPairForwardingStats, ForwardedPayment, Payment,
};

use crate::config::Connection;
use crate::format::{escape, short_id};

const ALIAS_TTL: Duration = Duration::from_secs(60 * 60);
/// ldk-server's client has no request timeout, so bound every call here.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Aliases only make messages prettier, so don't wait long for them.
const ALIAS_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs an ldk-server request, failing if it takes longer than `limit`.
async fn rpc<T>(
	limit: Duration, request: impl Future<Output = Result<T, LdkServerError>>,
) -> Result<T> {
	match tokio::time::timeout(limit, request).await {
		Ok(response) => Ok(response?),
		Err(_) => bail!("ldk-server didn't respond within {} seconds", limit.as_secs()),
	}
}

/// What we remember about a channel so we can describe it after it has closed.
#[derive(Clone, Debug)]
pub struct ChannelInfo {
	pub counterparty_node_id: String,
	pub capacity_sats: u64,
	pub is_outbound: bool,
}

/// An ldk-server client plus caches used to make messages readable.
pub struct Node {
	pub client: LdkServerClient,
	aliases: Mutex<HashMap<String, (Option<String>, Instant)>>,
	channels: Mutex<HashMap<String, ChannelInfo>>,
	/// IDs of channels known to have been funded, from channel lists and open events.
	funded: Mutex<HashSet<String>>,
	/// Whether channels have been listed at least once, so `funded` covers channels that were
	/// opened before the bot started.
	channels_loaded: AtomicBool,
}

/// Fetches every page of a paginated list RPC.
macro_rules! paginate {
	($client:expr, $method:ident, $request:ident, $field:ident) => {{
		let mut items = Vec::new();
		let mut page_token = None;
		loop {
			let response = rpc(RPC_TIMEOUT, $client.$method($request { page_token })).await?;
			items.extend(response.$field);
			match response.next_page_token {
				Some(token) => page_token = Some(token),
				None => break,
			}
		}
		items
	}};
}

impl Node {
	pub fn new(connection: Connection) -> Result<Self> {
		let client =
			LdkServerClient::new(connection.address, connection.macaroon, &connection.cert_pem)
				.map_err(anyhow::Error::msg)?;
		Ok(Self {
			client,
			aliases: Mutex::default(),
			channels: Mutex::default(),
			funded: Mutex::default(),
			channels_loaded: AtomicBool::new(false),
		})
	}

	/// Looks up a node's alias in the network graph, caching the result.
	pub async fn alias(&self, node_id: &str) -> Option<String> {
		if let Some((alias, fetched)) = self.aliases.lock().unwrap().get(node_id) {
			if fetched.elapsed() < ALIAS_TTL {
				return alias.clone();
			}
		}

		let request = GraphGetNodeRequest { node_id: node_id.to_string() };
		let alias = match rpc(ALIAS_TIMEOUT, self.client.graph_get_node(request)).await {
			Ok(response) => response
				.node
				.and_then(|node| node.announcement_info)
				.map(|info| info.alias)
				.filter(|alias| !alias.trim().is_empty()),
			Err(e) => {
				tracing::debug!("alias lookup for {node_id} failed: {e}");
				None
			},
		};
		self.aliases.lock().unwrap().insert(node_id.to_string(), (alias.clone(), Instant::now()));
		alias
	}

	/// A short HTML label for a node: its alias, or the start of its public key.
	pub async fn label(&self, node_id: &str) -> String {
		match self.alias(node_id).await {
			Some(alias) => escape(&alias),
			None => format!("<code>{}</code>", short_id(node_id)),
		}
	}

	/// Like [`Node::label`], but for an optional node ID.
	pub async fn label_opt(&self, node_id: Option<&str>) -> String {
		match node_id {
			Some(node_id) => self.label(node_id).await,
			None => "unknown".to_string(),
		}
	}

	/// Lists open channels and remembers their details for later events.
	pub async fn list_channels(&self) -> Result<Vec<Channel>> {
		let channels =
			rpc(RPC_TIMEOUT, self.client.list_channels(ListChannelsRequest {})).await?.channels;
		let mut funded = self.funded.lock().unwrap();
		funded.extend(
			channels.iter().filter(|c| c.funding_txo.is_some()).map(|c| c.channel_id.clone()),
		);
		let mut cache = self.channels.lock().unwrap();
		for channel in &channels {
			cache.insert(
				channel.channel_id.clone(),
				ChannelInfo {
					counterparty_node_id: channel.counterparty_node_id.clone(),
					capacity_sats: channel.channel_value_sats,
					is_outbound: channel.is_outbound,
				},
			);
		}
		self.channels_loaded.store(true, Ordering::Relaxed);
		Ok(channels)
	}

	/// Whether channels have been listed at least once since the bot started.
	pub fn channels_loaded(&self) -> bool {
		self.channels_loaded.load(Ordering::Relaxed)
	}

	pub fn mark_funded(&self, channel_id: &str) {
		self.funded.lock().unwrap().insert(channel_id.to_string());
	}

	pub fn is_funded(&self, channel_id: &str) -> bool {
		self.funded.lock().unwrap().contains(channel_id)
	}

	pub fn cached_channel(&self, channel_id: &str) -> Option<ChannelInfo> {
		self.channels.lock().unwrap().get(channel_id).cloned()
	}

	pub async fn list_payments(&self) -> Result<Vec<Payment>> {
		Ok(paginate!(self.client, list_payments, ListPaymentsRequest, payments))
	}

	pub async fn list_forwarded_payments(&self) -> Result<Vec<ForwardedPayment>> {
		Ok(paginate!(
			self.client,
			list_forwarded_payments,
			ListForwardedPaymentsRequest,
			forwarded_payments
		))
	}

	pub async fn list_channel_forwarding_stats(&self) -> Result<Vec<ChannelForwardingStats>> {
		Ok(paginate!(
			self.client,
			list_channel_forwarding_stats,
			ListChannelForwardingStatsRequest,
			stats
		))
	}

	pub async fn list_channel_pair_forwarding_stats(
		&self,
	) -> Result<Vec<ChannelPairForwardingStats>> {
		Ok(paginate!(
			self.client,
			list_channel_pair_forwarding_stats,
			ListChannelPairForwardingStatsRequest,
			stats
		))
	}
}

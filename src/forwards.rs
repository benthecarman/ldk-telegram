use std::sync::{Arc, Mutex};
use std::time::Duration;

use ldk_server_client::ldk_server_grpc::events::PaymentForwarded;
use ldk_server_client::ldk_server_grpc::types::HtlcLocator;

use crate::format::msat;
use crate::node::Node;
use crate::telegram::Notifier;

/// The most forwards listed individually in one digest message.
const MAX_LINES: usize = 20;

struct Forward {
	in_node: Option<String>,
	out_node: Option<String>,
	amount_msat: u64,
	fee_msat: Option<u64>,
}

/// Collects forwards and periodically posts them as one message.
pub struct ForwardDigest {
	pending: Mutex<Vec<Forward>>,
	min_amount_msat: u64,
}

impl ForwardDigest {
	pub fn new(min_amount_sats: u64) -> Self {
		Self { pending: Mutex::default(), min_amount_msat: min_amount_sats.saturating_mul(1000) }
	}

	pub fn push(&self, node: &Node, event: PaymentForwarded) {
		if event.outbound_amount_forwarded_msat < self.min_amount_msat {
			return;
		}
		let node_of = |htlcs: &[HtlcLocator]| {
			let htlc = htlcs.first()?;
			htlc.node_id
				.clone()
				.or_else(|| node.cached_channel(&htlc.channel_id).map(|c| c.counterparty_node_id))
		};
		let forward = Forward {
			in_node: node_of(&event.prev_htlcs),
			out_node: node_of(&event.next_htlcs),
			amount_msat: event.outbound_amount_forwarded_msat,
			fee_msat: event.total_fee_earned_msat,
		};
		self.pending.lock().unwrap().push(forward);
	}

	/// Posts a digest every `interval` if any forwards were collected.
	pub async fn run(self: Arc<Self>, node: Arc<Node>, notifier: Notifier, interval: Duration) {
		let mut ticker = tokio::time::interval(interval);
		ticker.tick().await;
		loop {
			ticker.tick().await;
			self.flush(&node, &notifier).await;
		}
	}

	/// Posts any collected forwards now.
	pub async fn flush(&self, node: &Node, notifier: &Notifier) {
		let forwards = std::mem::take(&mut *self.pending.lock().unwrap());
		if !forwards.is_empty() {
			notifier.send(format_digest(node, &forwards).await).await;
		}
	}
}

async fn format_digest(node: &Node, forwards: &[Forward]) -> String {
	let total_amount: u64 = forwards.iter().map(|f| f.amount_msat).sum();
	let total_fees: u64 = forwards.iter().filter_map(|f| f.fee_msat).sum();
	let count = forwards.len();
	let noun = if count == 1 { "forward" } else { "forwards" };

	let mut text = format!(
		"💸 <b>{count} {noun}</b>: {} routed, earned {}\n",
		msat(total_amount),
		msat(total_fees)
	);
	for forward in forwards.iter().take(MAX_LINES) {
		let fee = match forward.fee_msat {
			Some(fee) => msat(fee),
			None => "unknown fee".to_string(),
		};
		text.push_str(&format!(
			"\n{} → {}: {} ({fee})",
			node.label_opt(forward.in_node.as_deref()).await,
			node.label_opt(forward.out_node.as_deref()).await,
			msat(forward.amount_msat),
		));
	}
	if count > MAX_LINES {
		text.push_str(&format!("\n…and {} more", count - MAX_LINES));
	}
	text
}

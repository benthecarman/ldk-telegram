use std::sync::{Arc, Mutex};
use std::time::Duration;

use ldk_server_client::ldk_server_grpc::events::PaymentForwarded;
use ldk_server_client::ldk_server_grpc::types::HtlcLocator;

use crate::format::msat;
use crate::node::Node;
use crate::telegram::Notifier;

/// The most routes listed individually in one digest message.
const MAX_LINES: usize = 20;

struct Forward {
	in_node: Option<String>,
	out_node: Option<String>,
	amount_msat: u64,
	fee_msat: Option<u64>,
}

/// Forwards that took the same route, from one peer to another.
#[derive(Debug, PartialEq)]
struct Route<'a> {
	in_node: Option<&'a str>,
	out_node: Option<&'a str>,
	count: usize,
	amount_msat: u64,
	/// Sum of the known fees, or `None` if no fee on this route is known.
	fee_msat: Option<u64>,
}

/// Groups forwards by route, in the order each route was first seen.
fn group_routes(forwards: &[Forward]) -> Vec<Route<'_>> {
	let mut routes: Vec<Route> = Vec::new();
	for forward in forwards {
		let in_node = forward.in_node.as_deref();
		let out_node = forward.out_node.as_deref();
		let route = match routes.iter().position(|r| r.in_node == in_node && r.out_node == out_node)
		{
			Some(i) => &mut routes[i],
			None => {
				routes.push(Route { in_node, out_node, count: 0, amount_msat: 0, fee_msat: None });
				routes.last_mut().unwrap()
			},
		};
		route.count += 1;
		route.amount_msat += forward.amount_msat;
		if let Some(fee) = forward.fee_msat {
			route.fee_msat = Some(route.fee_msat.unwrap_or(0) + fee);
		}
	}
	routes
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
	let routes = group_routes(forwards);
	for route in routes.iter().take(MAX_LINES) {
		let fee = match route.fee_msat {
			Some(fee) => msat(fee),
			None => "unknown fee".to_string(),
		};
		let times = if route.count > 1 { format!(" ×{}", route.count) } else { String::new() };
		text.push_str(&format!(
			"\n{} → {}{times}: {} ({fee})",
			node.label_opt(route.in_node).await,
			node.label_opt(route.out_node).await,
			msat(route.amount_msat),
		));
	}
	if routes.len() > MAX_LINES {
		let more = routes.len() - MAX_LINES;
		let noun = if more == 1 { "route" } else { "routes" };
		text.push_str(&format!("\n…and {more} more {noun}"));
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;

	fn forward(in_node: &str, out_node: &str, amount_msat: u64, fee_msat: Option<u64>) -> Forward {
		Forward {
			in_node: Some(in_node.to_string()),
			out_node: Some(out_node.to_string()),
			amount_msat,
			fee_msat,
		}
	}

	#[test]
	fn groups_forwards_by_route() {
		let forwards = [
			forward("a", "b", 1_000, Some(10)),
			forward("b", "a", 2_000, Some(20)),
			forward("a", "b", 3_000, Some(30)),
			Forward {
				in_node: None,
				out_node: Some("b".to_string()),
				amount_msat: 4_000,
				fee_msat: None,
			},
		];
		let route = |in_node, out_node, count, amount_msat, fee_msat| Route {
			in_node,
			out_node,
			count,
			amount_msat,
			fee_msat,
		};
		assert_eq!(
			group_routes(&forwards),
			[
				route(Some("a"), Some("b"), 2, 4_000, Some(40)),
				route(Some("b"), Some("a"), 1, 2_000, Some(20)),
				route(None, Some("b"), 1, 4_000, None),
			]
		);
	}

	#[test]
	fn sums_known_fees_on_a_route() {
		let forwards = [forward("a", "b", 1_000, None), forward("a", "b", 1_000, Some(5))];
		assert_eq!(group_routes(&forwards)[0].fee_msat, Some(5));
	}
}

use std::sync::Arc;
use std::time::{Duration, Instant};

use ldk_server_client::ldk_server_grpc::events::event_envelope::Event;
use ldk_server_client::ldk_server_grpc::events::{
	ChannelClosureInitiator, ChannelState, ChannelStateChanged, EventEnvelope, PaymentFailureReason,
};
use ldk_server_client::ldk_server_grpc::types::payment_kind::Kind;
use ldk_server_client::ldk_server_grpc::types::Payment;

use crate::config::NotificationsConfig;
use crate::format::{escape, msat, sats};
use crate::forwards::ForwardDigest;
use crate::node::Node;
use crate::shutdown::Shutdown;
use crate::systemd;
use crate::telegram::Notifier;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A stream that stays up this long counts as healthy and resets the reconnect backoff.
const HEALTHY_STREAM: Duration = Duration::from_secs(30);
/// Longest time spent handling one event, so a stuck lookup can't stall the stream.
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Subscribes to ldk-server's event stream and posts notifications, reconnecting until shutdown.
///
/// Shutdown is only checked between events, so an event that has been received is always
/// handled.
pub async fn run(
	node: Arc<Node>, notifier: Notifier, config: NotificationsConfig, digest: Arc<ForwardDigest>,
	shutdown: Shutdown,
) {
	let mut backoff = MIN_BACKOFF;
	let mut offline_since: Option<Instant> = None;
	let mut alerted = false;

	loop {
		let subscribed = tokio::select! {
			subscribed = node.client.subscribe_events() => subscribed,
			() = shutdown.wait() => return,
		};
		match subscribed {
			Ok(mut stream) => {
				tracing::info!("subscribed to ldk-server events");
				systemd::status("Watching ldk-server events");
				if alerted {
					notifier
						.send("✅ Reconnected to ldk-server. Events from while it was unreachable were not delivered.")
						.await;
				}
				offline_since = None;
				alerted = false;

				// Remember current channels so close notifications can describe them.
				if let Err(e) = node.list_channels().await {
					tracing::warn!("failed to list channels: {e}");
				}

				let connected_at = Instant::now();
				loop {
					let message = tokio::select! {
						message = stream.next_message() => message,
						() = shutdown.wait() => return,
					};
					let Some(message) = message else { break };
					match message {
						Ok(envelope) => {
							let handled =
								handle_event(&node, &notifier, &config, &digest, envelope);
							if tokio::time::timeout(EVENT_TIMEOUT, handled).await.is_err() {
								tracing::warn!("gave up handling an event after {EVENT_TIMEOUT:?}");
							}
						},
						Err(e) => {
							tracing::warn!("event stream error: {e}");
							break;
						},
					}
				}
				tracing::warn!("event stream closed");
				systemd::status("Reconnecting to ldk-server");
				if connected_at.elapsed() >= HEALTHY_STREAM {
					backoff = MIN_BACKOFF;
				}
			},
			Err(e) => {
				tracing::warn!("failed to subscribe to ldk-server events: {e}");
				systemd::status("Waiting for ldk-server");
			},
		}

		let since = *offline_since.get_or_insert_with(Instant::now);
		if !alerted && since.elapsed() >= config.offline_alert_after() {
			notifier.send("⚠️ Lost connection to ldk-server. Retrying…").await;
			alerted = true;
		}

		tokio::select! {
			() = tokio::time::sleep(backoff) => {},
			() = shutdown.wait() => return,
		}
		backoff = (backoff * 2).min(MAX_BACKOFF);
	}
}

async fn handle_event(
	node: &Node, notifier: &Notifier, config: &NotificationsConfig, digest: &ForwardDigest,
	envelope: EventEnvelope,
) {
	let Some(event) = envelope.event else { return };
	let min_payment_msat = config.min_payment_amount_sats.saturating_mul(1000);

	let text = match event {
		Event::PaymentReceived(event) if config.payments_received => event
			.payment
			.filter(|p| p.amount_msat.unwrap_or(0) >= min_payment_msat)
			.map(|p| format!("💰 Received <b>{}</b>{}", amount(&p), describe_kind(&p))),
		Event::PaymentSuccessful(event) if config.payments_sent => {
			event.payment.filter(|p| p.amount_msat.unwrap_or(0) >= min_payment_msat).map(|p| {
				let fee =
					p.fee_paid_msat.map(|fee| format!(", fee {}", msat(fee))).unwrap_or_default();
				format!("⚡ Sent <b>{}</b>{}{fee}", amount(&p), describe_kind(&p))
			})
		},
		Event::PaymentFailed(event) if config.payments_failed => {
			let reason = event
				.reason
				.and_then(PaymentFailureReason::from_i32)
				.map(failure_reason)
				.unwrap_or("unknown reason");
			let what = match event.payment.as_ref().and_then(|p| p.amount_msat) {
				Some(amount) => format!("Payment of {}", msat(amount)),
				None => "Payment".to_string(),
			};
			Some(format!("❌ {what} failed: {reason}"))
		},
		Event::PaymentClaimable(event) if config.payments_claimable => {
			let deadline = event
				.claim_deadline
				.map(|height| format!(" before block {height}"))
				.unwrap_or_default();
			Some(format!(
				"⏳ Payment of <b>{}</b> is waiting to be claimed or failed{deadline}",
				msat(event.claimable_amount_msat)
			))
		},
		Event::PaymentForwarded(event) if config.forwards => {
			digest.push(node, event);
			None
		},
		Event::ChannelStateChanged(event) if config.channels => channel_message(node, event).await,
		Event::SpliceNegotiated(event) if config.channels => {
			if let Err(e) = node.list_channels().await {
				tracing::warn!("failed to list channels: {e}");
			}
			Some(format!(
				"🔀 Splice negotiated with {}, waiting for confirmation",
				node.label(&event.counterparty_node_id).await
			))
		},
		Event::SpliceNegotiationFailed(event) if config.channels => Some(format!(
			"⚠️ Splice negotiation with {} failed",
			node.label(&event.counterparty_node_id).await
		)),
		_ => None,
	};

	if let Some(text) = text {
		notifier.send(text).await;
	}
}

async fn channel_message(node: &Node, event: ChannelStateChanged) -> Option<String> {
	let state = ChannelState::from_i32(event.state)?;

	// Closed channels are no longer listed, so rely on what was cached while they were open.
	let info = match state {
		ChannelState::Closed => node.cached_channel(&event.channel_id),
		_ => {
			if let Err(e) = node.list_channels().await {
				tracing::warn!("failed to list channels: {e}");
			}
			node.cached_channel(&event.channel_id)
		},
	};

	let peer_id = event
		.counterparty_node_id
		.clone()
		.or_else(|| info.as_ref().map(|i| i.counterparty_node_id.clone()));
	let peer = node.label_opt(peer_id.as_deref()).await;
	let capacity =
		info.as_ref().map(|i| format!(": {}", sats(i.capacity_sats))).unwrap_or_default();
	let reason = event
		.reason
		.as_ref()
		.map(|r| r.message.trim())
		.filter(|m| !m.is_empty())
		.map(|m| format!("\n{}", escape(m)))
		.unwrap_or_default();

	match state {
		ChannelState::Pending => {
			let opener = match info.map(|i| i.is_outbound) {
				Some(true) => " (opened by us)",
				Some(false) => " (opened by peer)",
				None => "",
			};
			Some(format!("⏳ Opening channel with {peer}{capacity}{opener}"))
		},
		ChannelState::Ready => Some(format!("🌹 Channel opened with {peer}{capacity}")),
		ChannelState::OpenFailed => Some(format!("⚠️ Channel open with {peer} failed{reason}")),
		ChannelState::Closed => {
			let closer = match ChannelClosureInitiator::from_i32(event.closure_initiator) {
				Some(ChannelClosureInitiator::Local) => " (closed by us)",
				Some(ChannelClosureInitiator::Remote) => " (closed by peer)",
				_ => "",
			};
			Some(format!("🥀 Channel closed with {peer}{capacity}{closer}{reason}"))
		},
		ChannelState::Unspecified => None,
	}
}

fn amount(payment: &Payment) -> String {
	match payment.amount_msat {
		Some(amount) => msat(amount),
		None => "unknown amount".to_string(),
	}
}

/// Describes how a payment was made, e.g. ` via keysend`.
fn describe_kind(payment: &Payment) -> String {
	let Some(kind) = payment.kind.as_ref().and_then(|k| k.kind.as_ref()) else {
		return String::new();
	};
	let (method, note) = match kind {
		Kind::Onchain(_) => ("on-chain", None),
		Kind::Bolt11(_) => ("invoice", None),
		Kind::Bolt12Offer(offer) => ("offer", offer.payer_note.as_deref()),
		Kind::Bolt12Refund(refund) => ("refund", refund.payer_note.as_deref()),
		Kind::Spontaneous(_) => ("keysend", None),
	};
	match note.map(str::trim).filter(|n| !n.is_empty()) {
		Some(note) => format!(" via {method}\n📝 {}", escape(note)),
		None => format!(" via {method}"),
	}
}

fn failure_reason(reason: PaymentFailureReason) -> &'static str {
	match reason {
		PaymentFailureReason::Unspecified => "unknown reason",
		PaymentFailureReason::RecipientRejected => "recipient rejected the payment",
		PaymentFailureReason::UserAbandoned => "payment abandoned",
		PaymentFailureReason::RetriesExhausted => "retries exhausted",
		PaymentFailureReason::PaymentExpired => "payment expired",
		PaymentFailureReason::RouteNotFound => "no route found",
		PaymentFailureReason::UnexpectedError => "unexpected error",
		PaymentFailureReason::UnknownRequiredFeatures => "invoice requires unknown features",
		PaymentFailureReason::InvoiceRequestExpired => "invoice request expired",
		PaymentFailureReason::InvoiceRequestRejected => "invoice request rejected",
		PaymentFailureReason::BlindedPathCreationFailed => "failed to create blinded path",
	}
}

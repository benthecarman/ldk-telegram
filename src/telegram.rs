use teloxide::adaptors::Throttle;
use teloxide::prelude::*;
use teloxide::types::{LinkPreviewOptions, ParseMode};

/// The bot, rate limited to stay under Telegram's per-chat message limits.
pub type TgBot = Throttle<Bot>;

const NO_PREVIEW: LinkPreviewOptions = LinkPreviewOptions {
	is_disabled: true,
	url: None,
	prefer_small_media: false,
	prefer_large_media: false,
	show_above_text: false,
};

/// Telegram's limit on a message's length, in UTF-16 code units.
const MAX_MESSAGE_LEN: usize = 4096;

/// Sends an HTML message to a chat, logging rather than returning failures. Long messages are
/// split between lines, so tags must not span lines.
pub async fn send_html(bot: &TgBot, chat_id: ChatId, text: impl Into<String>) {
	for part in split_message(&text.into()) {
		let result = bot
			.send_message(chat_id, part)
			.parse_mode(ParseMode::Html)
			.link_preview_options(NO_PREVIEW)
			.await;
		if let Err(e) = result {
			tracing::error!("failed to send Telegram message: {e}");
			return;
		}
	}
}

/// Splits text into parts that fit in a Telegram message, breaking between lines. The limit
/// applies after HTML is parsed, so measuring the raw HTML errs on the safe side.
fn split_message(text: &str) -> Vec<String> {
	let len = |s: &str| s.encode_utf16().count();
	let mut parts = Vec::new();
	let mut part = String::new();
	for line in text.split('\n') {
		if !part.is_empty() && len(&part) + 1 + len(line) > MAX_MESSAGE_LEN {
			parts.push(std::mem::take(&mut part));
		}
		if !part.is_empty() {
			part.push('\n');
		}
		part.push_str(line);
	}
	parts.push(part);
	// A split can land next to blank lines. Drop them, and any part left empty, which Telegram
	// would reject.
	parts
		.into_iter()
		.map(|p| p.trim_matches('\n').to_string())
		.filter(|p| !p.trim().is_empty())
		.collect()
}

/// Posts notifications to the configured chat.
#[derive(Clone)]
pub struct Notifier {
	bot: TgBot,
	chat_id: ChatId,
}

impl Notifier {
	pub fn new(bot: TgBot, chat_id: ChatId) -> Self {
		Self { bot, chat_id }
	}

	pub async fn send(&self, text: impl Into<String>) {
		send_html(&self.bot, self.chat_id, text).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keeps_short_messages_whole() {
		assert_eq!(split_message("a\n\nb"), vec!["a\n\nb"]);
	}

	#[test]
	fn splits_long_messages_between_lines() {
		let line = "x".repeat(1_000);
		let text = [line.as_str(); 5].join("\n");
		let parts = split_message(&text);
		assert_eq!(parts, vec![[line.as_str(); 4].join("\n"), line.clone()]);
	}

	#[test]
	fn drops_blank_parts() {
		let line = "x".repeat(4_000);
		let text = format!("{line}\n\n{line}");
		assert_eq!(split_message(&text), vec![line.clone(), line]);
	}
}

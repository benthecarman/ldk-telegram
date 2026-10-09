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

/// Sends an HTML message to a chat, logging rather than returning failures.
pub async fn send_html(bot: &TgBot, chat_id: ChatId, text: impl Into<String>) {
	let result = bot
		.send_message(chat_id, text)
		.parse_mode(ParseMode::Html)
		.link_preview_options(NO_PREVIEW)
		.await;
	if let Err(e) = result {
		tracing::error!("failed to send Telegram message: {e}");
	}
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

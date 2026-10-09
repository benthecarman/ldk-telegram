use std::time::{SystemTime, UNIX_EPOCH};

/// Formats a number with thousands separators, e.g. `1,234,567`.
pub fn group(n: u64) -> String {
	let digits = n.to_string();
	let mut out = String::with_capacity(digits.len() + digits.len() / 3);
	for (i, c) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

pub fn sats(n: u64) -> String {
	if n == 1 {
		"1 sat".to_string()
	} else {
		format!("{} sats", group(n))
	}
}

/// Formats a millisatoshi amount as sats, keeping msat precision only below one sat.
pub fn msat(n: u64) -> String {
	if n > 0 && n < 1000 {
		format!("{} msat", n)
	} else {
		sats(n / 1000)
	}
}

/// Escapes text for Telegram's HTML parse mode.
pub fn escape(s: &str) -> String {
	s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The first eight characters of a public key or other hex identifier.
pub fn short_id(id: &str) -> &str {
	id.get(..8).unwrap_or(id)
}

pub fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Describes how long ago a unix timestamp was, e.g. `5m ago`.
pub fn ago(timestamp: u64) -> String {
	format!("{} ago", duration(now_secs().saturating_sub(timestamp)))
}

/// A short human duration, e.g. `5m` or `3d`.
pub fn duration(secs: u64) -> String {
	let (n, unit) = match secs {
		0..=59 => (secs, "s"),
		60..=3_599 => (secs / 60, "m"),
		3_600..=86_399 => (secs / 3_600, "h"),
		_ => (secs / 86_400, "d"),
	};
	format!("{n}{unit}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn groups_thousands() {
		assert_eq!(group(0), "0");
		assert_eq!(group(999), "999");
		assert_eq!(group(1_000), "1,000");
		assert_eq!(group(1_234_567), "1,234,567");
		assert_eq!(group(100_000_000), "100,000,000");
	}

	#[test]
	fn formats_msat() {
		assert_eq!(msat(0), "0 sats");
		assert_eq!(msat(1), "1 msat");
		assert_eq!(msat(999), "999 msat");
		assert_eq!(msat(1_000), "1 sat");
		assert_eq!(msat(1_500), "1 sat");
		assert_eq!(msat(2_000_000), "2,000 sats");
	}

	#[test]
	fn formats_durations() {
		assert_eq!(duration(0), "0s");
		assert_eq!(duration(59), "59s");
		assert_eq!(duration(60), "1m");
		assert_eq!(duration(3_600), "1h");
		assert_eq!(duration(2 * 86_400), "2d");
	}

	#[test]
	fn escapes_html() {
		assert_eq!(escape("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
	}

	#[test]
	fn shortens_ids() {
		assert_eq!(short_id("0123456789abcdef"), "01234567");
		assert_eq!(short_id("abc"), "abc");
	}
}

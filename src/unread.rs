/// WhatsApp Web reports unread messages in the page title, for example
/// `(3) WhatsApp` or `(12) WhatsApp Web`. This parser intentionally only
/// trusts the leading parenthesised number; everything else is treated as zero
/// so a page title change cannot produce a bogus badge.
pub fn parse_unread(title: &str) -> u32 {
    let title = title.trim_start();
    let Some(rest) = title.strip_prefix('(') else {
        return 0;
    };
    let Some(end) = rest.find(')') else {
        return 0;
    };
    let number = rest[..end]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if number.is_empty() {
        return 0;
    }
    number.parse::<u64>().unwrap_or(0).min(9_999) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_titles() {
        assert_eq!(parse_unread("(3) WhatsApp"), 3);
        assert_eq!(parse_unread("(12) WhatsApp Web"), 12);
        assert_eq!(parse_unread("  (1) WhatsApp"), 1);
        assert_eq!(parse_unread("(999) WhatsApp"), 999);
    }

    #[test]
    fn returns_zero_without_a_count() {
        assert_eq!(parse_unread("WhatsApp"), 0);
        assert_eq!(parse_unread(""), 0);
        assert_eq!(parse_unread("(abc) WhatsApp"), 0);
        assert_eq!(parse_unread("(3 WhatsApp"), 0);
        assert_eq!(parse_unread("3) WhatsApp"), 0);
    }

    #[test]
    fn caps_absurd_counts() {
        assert_eq!(parse_unread("(999999999999) WhatsApp"), 9_999);
    }
}

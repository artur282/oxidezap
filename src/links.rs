use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkAction {
    /// Stay inside the WebKit view.
    Allow,
    /// Hand the URL to the desktop's default handler.
    External(String),
    /// A `whatsapp://` URL that must be translated and loaded in the app.
    WhatsApp(String),
}

pub fn classify(raw: &str) -> LinkAction {
    let raw = raw.trim();
    if raw.is_empty() {
        return LinkAction::Allow;
    }
    let Ok(url) = Url::parse(raw) else {
        return LinkAction::Allow;
    };

    match url.scheme() {
        "http" | "https" => {
            if is_whatsapp_host(url.host_str().unwrap_or_default()) {
                LinkAction::Allow
            } else {
                LinkAction::External(raw.to_string())
            }
        }
        "mailto" | "tel" => LinkAction::External(raw.to_string()),
        "whatsapp" => match whatsapp_to_web(raw) {
            Some(web) => LinkAction::WhatsApp(web),
            None => LinkAction::External(raw.to_string()),
        },
        "blob" | "data" | "about" => LinkAction::Allow,
        _ => LinkAction::External(raw.to_string()),
    }
}

/// WhatsApp background/utility pages live on `whatsapp.net` (for example
/// `flows.whatsapp.net/flows/cache_management/`). They must not open a system
/// browser tab or replace the chat view; new-window requests for them are
/// ignored.
pub fn is_internal_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "whatsapp.net" || host.ends_with(".whatsapp.net")
}

pub fn is_whatsapp_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "whatsapp.com"
        || host.ends_with(".whatsapp.com")
        || host == "wa.me"
        || host.ends_with(".wa.me")
}

pub fn whatsapp_to_web(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "whatsapp" {
        return None;
    }

    match url.host_str()? {
        "send" => {
            let mut phone = None;
            let mut text = None;
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "phone" => phone = Some(value.into_owned()),
                    "text" => text = Some(value.into_owned()),
                    _ => {}
                }
            }
            let phone = phone?;
            let mut target = Url::parse("https://web.whatsapp.com/send").ok()?;
            target.query_pairs_mut().append_pair("phone", &phone);
            if let Some(text) = text {
                target.query_pairs_mut().append_pair("text", &text);
            }
            Some(target.to_string())
        }
        "chat" => {
            let code = url
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.into_owned())?;
            Some(format!("https://chat.whatsapp.com/{code}"))
        }
        _ => None,
    }
}

pub fn open_external(url: &str) {
    if let Err(error) = open::that_detached(url) {
        tracing::warn!(%url, %error, "could not open the external URL");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whatsapp_domains_stay_in_the_app() {
        assert_eq!(classify("https://web.whatsapp.com/"), LinkAction::Allow);
        assert_eq!(classify("https://wa.me/5491100000000"), LinkAction::Allow);
        assert_eq!(classify("https://chat.whatsapp.com/abc"), LinkAction::Allow);
    }

    #[test]
    fn unrelated_links_open_externally() {
        assert_eq!(
            classify("https://example.com/article"),
            LinkAction::External("https://example.com/article".to_string())
        );
        assert_eq!(
            classify("mailto:hi@example.com"),
            LinkAction::External("mailto:hi@example.com".to_string())
        );
        assert_eq!(
            classify("tel:+123456"),
            LinkAction::External("tel:+123456".to_string())
        );
    }

    #[test]
    fn whatsapp_protocol_urls_are_translated() {
        assert_eq!(
            classify("whatsapp://send?phone=5491100000000&text=hello"),
            LinkAction::WhatsApp(
                "https://web.whatsapp.com/send?phone=5491100000000&text=hello".to_string()
            )
        );
        assert_eq!(
            classify("whatsapp://chat?code=abc123"),
            LinkAction::WhatsApp("https://chat.whatsapp.com/abc123".to_string())
        );
    }

    #[test]
    fn whatsapp_net_utility_pages_are_internal() {
        assert!(is_internal_url(
            "https://flows.whatsapp.net/flows/cache_management/"
        ));
        assert!(is_internal_url("https://mmg.whatsapp.net/file"));
        assert!(!is_internal_url("https://web.whatsapp.com/"));
        assert!(!is_internal_url("https://wa.me/5491100000000"));
        assert!(!is_internal_url("https://evilwhatsapp.net/"));
        assert!(!is_internal_url("https://whatsapp.net.evil.test/"));
    }

    #[test]
    fn unknown_schemes_are_external() {
        assert_eq!(
            classify("ftp://example.com/file"),
            LinkAction::External("ftp://example.com/file".to_string())
        );
    }

    #[test]
    fn malformed_urls_do_not_crash() {
        assert_eq!(classify(""), LinkAction::Allow);
        assert_eq!(classify("not a url"), LinkAction::Allow);
    }
}

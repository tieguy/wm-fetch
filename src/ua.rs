//! User-Agent construction per the WMF User-Agent policy.
//!
//! Format: `<client name>/<version> (<contact information>) <library>/<version>`
//! https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_User-Agent_Policy

/// Default client name contains "bot" — the policy asks bots to carry that
/// substring so WMF can classify the traffic.
pub const DEFAULT_CLIENT_NAME: &str = "wm-fetch-bot";

pub fn build(client_name: &str, contact_page: Option<&str>, contact_email: Option<&str>) -> String {
    let mut contact = String::new();
    if let Some(p) = contact_page {
        contact.push_str(p);
    }
    if let Some(e) = contact_email {
        if !contact.is_empty() {
            contact.push(' ');
        }
        contact.push_str(e);
    }
    let lib = concat!("reqwest/", env!("REQWEST_VERSION"));
    format!(
        "{}/{} ({}) {}",
        client_name,
        env!("CARGO_PKG_VERSION"),
        contact,
        lib
    )
}

/// The policy wants contact info in a parenthesized group. A custom
/// `--user-agent` without one draws a stderr warning (still honored — the
/// obligation to run a compliant agent belongs to the caller).
pub fn has_contact_group(ua: &str) -> bool {
    match (ua.find('('), ua.find(')')) {
        (Some(o), Some(c)) => c > o,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_construction() {
        let ua = build(
            DEFAULT_CLIENT_NAME,
            Some("https://en.wikipedia.org/wiki/User:LuisVilla"),
            Some("luis@lu.is"),
        );
        assert!(ua.starts_with("wm-fetch-bot/2.0.0 ("), "{ua}");
        assert!(
            ua.contains("https://en.wikipedia.org/wiki/User:LuisVilla luis@lu.is"),
            "{ua}"
        );
        assert!(
            ua.ends_with(&format!(" reqwest/{}", env!("REQWEST_VERSION"))),
            "{ua}"
        );
        assert!(ua.contains("bot")); // UA policy classification substring
    }

    #[test]
    fn ua_email_only() {
        let ua = build("mybot", None, Some("a@b.c"));
        assert!(ua.starts_with("mybot/2.0.0 (a@b.c) reqwest/"), "{ua}");
    }

    #[test]
    fn ua_page_only() {
        let ua = build("mybot", Some("https://example.org/bot"), None);
        assert!(
            ua.starts_with("mybot/2.0.0 (https://example.org/bot) reqwest/"),
            "{ua}"
        );
    }

    #[test]
    fn contact_group_detection() {
        assert!(has_contact_group("mybot/1.0 (me@example.com)"));
        assert!(!has_contact_group("mybot/1.0"));
        assert!(!has_contact_group("mybot/1.0 )x("));
    }
}

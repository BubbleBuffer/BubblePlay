//! Pairing links: `bubbleplay://pair?relay=<url>&pc=<id>&code=<code>` (the tray's QR code) and
//! `https://<relay>/pair#pc=<id>&code=<code>` (the browser's form).

use url::Url;

/// What a pairing link carries. `relay` is empty when the link names none.
#[derive(Debug, PartialEq, Eq)]
pub struct PairLink {
    pub relay: String,
    pub pc_id: String,
    pub code: String,
}

pub fn parse(link: &str) -> Result<PairLink, String> {
    let bad = || "This is not a BubblePlay pairing link.".to_owned();
    let url = Url::parse(link.trim()).map_err(|_| bad())?;
    let (relay, query) = match url.scheme() {
        "bubbleplay" if url.host_str() == Some("pair") => {
            (None, url.query().unwrap_or_default().to_owned())
        }
        "https" | "http" if url.path() == "/pair" => {
            let mut origin = url.clone();
            origin.set_path("");
            origin.set_fragment(None);
            origin.set_query(None);
            (
                Some(origin.as_str().trim_end_matches('/').to_owned()),
                url.fragment().unwrap_or_default().to_owned(),
            )
        }
        _ => return Err(bad()),
    };
    let mut link_relay = relay.unwrap_or_default();
    let (mut pc_id, mut code) = (String::new(), String::new());
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "relay" => link_relay = value.into_owned(),
            "pc" => pc_id = value.into_owned(),
            "code" => code = value.into_owned(),
            _ => {}
        }
    }
    if pc_id.is_empty() || code.is_empty() {
        return Err("The pairing link has no PC ID or no code.".to_owned());
    }
    Ok(PairLink {
        relay: link_relay,
        pc_id,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_app_link() {
        let link = parse("bubbleplay://pair?relay=https%3A%2F%2Frelay.example&pc=77VH-7M56-8GJK-Y12J&code=70Q1K-9MXR1").unwrap();
        assert_eq!(link.relay, "https://relay.example");
        assert_eq!(link.pc_id, "77VH-7M56-8GJK-Y12J");
        assert_eq!(link.code, "70Q1K-9MXR1");
    }

    #[test]
    fn reads_the_browser_link() {
        let link =
            parse("https://relay.example/pair#pc=77VH-7M56-8GJK-Y12J&code=70Q1K-9MXR1").unwrap();
        assert_eq!(link.relay, "https://relay.example");
        assert_eq!(link.pc_id, "77VH-7M56-8GJK-Y12J");
    }

    #[test]
    fn refuses_other_links() {
        assert!(parse("https://relay.example/other#pc=a&code=b").is_err());
        assert!(parse("bubbleplay://pair?relay=x").is_err());
        assert!(parse("not a link").is_err());
    }
}

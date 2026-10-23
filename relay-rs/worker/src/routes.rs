//! Request helpers the native server also has: the seven WebSocket routes, the `pc` and `epoch`
//! query values, and the client address. On Cloudflare the client address is the
//! CF-Connecting-IP header, which the platform sets (there are no trusted proxies to check).

use std::net::IpAddr;

use bp_relay_core::relay::Route;
use worker::{Request, Url};

/// The route for a WebSocket path. None for any other path.
pub fn route_for_path(path: &str) -> Option<Route> {
    match path {
        "/host-ws" => Some(Route::HostWs),
        "/ws" => Some(Route::Ws),
        "/host-version-ws" => Some(Route::HostVersionWs),
        "/host-media-ws" => Some(Route::HostMediaWs),
        "/media-ws" => Some(Route::MediaWs),
        "/host-file-ws" => Some(Route::HostFileWs),
        "/file-ws" => Some(Route::FileWs),
        _ => None,
    }
}

/// The first `pc` and the first `epoch` in the query, as the native server reads them.
pub fn query_values(url: &Url) -> (Option<String>, Option<String>) {
    let mut pc = None;
    let mut epoch = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "pc" if pc.is_none() => pc = Some(value.into_owned()),
            "epoch" if epoch.is_none() => epoch = Some(value.into_owned()),
            _ => {}
        }
    }
    (pc, epoch)
}

/// The client address from CF-Connecting-IP. None when it is missing or not an IP address.
pub fn client_ip(req: &Request) -> Option<IpAddr> {
    let value = req.headers().get("CF-Connecting-IP").ok()??;
    value.trim().parse().ok()
}

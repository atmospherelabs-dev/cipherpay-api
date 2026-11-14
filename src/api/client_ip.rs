//! Client IP resolution behind the local reverse proxy.
//!
//! Production runs the API on loopback behind Caddy, so every TCP peer is
//! 127.0.0.1. Rate limits keyed on the peer address were therefore shared by
//! all callers. Caddy (v2.5+) discards `X-Forwarded-For` from untrusted
//! clients and sets it to the real remote address, so when the peer is a
//! loopback proxy we take the right-most `X-Forwarded-For` entry (the one the
//! proxy appended). For any non-loopback peer the header is ignored, because
//! a direct caller can write whatever it likes there.

use actix_governor::{KeyExtractor, SimpleKeyExtractionError};
use actix_web::dev::ServiceRequest;
use actix_web::http::header::HeaderMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

fn resolve(peer: Option<SocketAddr>, headers: &HeaderMap) -> Option<IpAddr> {
    let peer_ip = peer?.ip();
    if !peer_ip.is_loopback() {
        return Some(peer_ip);
    }
    let forwarded = headers
        .get_all("x-forwarded-for")
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .last();
    Some(forwarded.unwrap_or(peer_ip))
}

/// Best-effort client IP for logging and rate limiting.
pub fn client_ip(req: &actix_web::HttpRequest) -> Option<IpAddr> {
    resolve(req.peer_addr(), req.headers())
}

/// IPv6 clients usually control a whole /64, so bucket them by prefix.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

/// Governor key extractor that rate-limits per real client, not per proxy.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientIpKeyExtractor;

impl KeyExtractor for ClientIpKeyExtractor {
    type Key = IpAddr;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        resolve(req.peer_addr(), req.headers())
            .map(bucket)
            .ok_or_else(|| SimpleKeyExtractionError::new("Could not determine client address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::{HeaderName, HeaderValue};

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in xff {
            h.append(
                HeaderName::from_static("x-forwarded-for"),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn loopback_peer_uses_rightmost_forwarded_entry() {
        let peer = "127.0.0.1:5000".parse().ok();
        let ip = resolve(peer, &headers(&["6.6.6.6, 203.0.113.9"])).unwrap();
        assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        let ip = resolve(peer, &headers(&["6.6.6.6", "198.51.100.4"])).unwrap();
        assert_eq!(ip, "198.51.100.4".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn direct_peer_ignores_spoofed_header() {
        let peer = "198.51.100.7:443".parse().ok();
        let ip = resolve(peer, &headers(&["127.0.0.1"])).unwrap();
        assert_eq!(ip, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn loopback_without_header_or_garbage_falls_back_to_peer() {
        let peer = "[::1]:5000".parse().ok();
        assert_eq!(
            resolve(peer, &headers(&[])).unwrap(),
            "::1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            resolve(peer, &headers(&["not-an-ip"])).unwrap(),
            "::1".parse::<IpAddr>().unwrap()
        );
        assert!(resolve(None, &headers(&[])).is_none());
    }

    #[test]
    fn ipv6_is_bucketed_by_64() {
        let a = bucket("2001:db8:1:2:aaaa::1".parse().unwrap());
        let b = bucket("2001:db8:1:2:bbbb::9".parse().unwrap());
        assert_eq!(a, b);
        assert_eq!(
            bucket("::ffff:203.0.113.1".parse().unwrap()),
            "203.0.113.1".parse::<IpAddr>().unwrap()
        );
    }

    #[actix_web::test]
    async fn governor_buckets_clients_behind_proxy_separately() {
        use actix_governor::{Governor, GovernorConfigBuilder};
        use actix_web::{test, web, App, HttpResponse};

        let conf = GovernorConfigBuilder::default()
            .key_extractor(ClientIpKeyExtractor)
            .seconds_per_request(60)
            .burst_size(1)
            .finish()
            .unwrap();
        let app = test::init_service(
            App::new()
                .wrap(Governor::new(&conf))
                .route("/", web::post().to(HttpResponse::Ok)),
        )
        .await;
        let call = |xff: &'static str| {
            test::TestRequest::post()
                .uri("/")
                .peer_addr("127.0.0.1:40000".parse().unwrap())
                .insert_header(("x-forwarded-for", xff))
                .to_request()
        };
        assert_eq!(
            test::call_service(&app, call("203.0.113.1")).await.status(),
            200
        );
        assert_eq!(
            test::call_service(&app, call("203.0.113.1")).await.status(),
            429
        );
        // A different client through the same proxy is not blocked.
        assert_eq!(
            test::call_service(&app, call("203.0.113.2")).await.status(),
            200
        );
    }
}

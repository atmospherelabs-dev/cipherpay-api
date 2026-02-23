use std::net::{IpAddr, ToSocketAddrs};
use zcash_address::ZcashAddress;

pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    pub fn too_long(field: &str, max: usize) -> Self {
        Self {
            field: field.to_string(),
            message: format!("{} must be at most {} characters", field, max),
        }
    }

    pub fn invalid(field: &str, reason: &str) -> Self {
        Self {
            field: field.to_string(),
            message: format!("{}: {}", field, reason),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.message,
            "field": self.field,
        })
    }
}

pub fn validate_length(field: &str, value: &str, max: usize) -> Result<(), ValidationError> {
    if value.len() > max {
        return Err(ValidationError::too_long(field, max));
    }
    Ok(())
}

pub fn validate_optional_length(
    field: &str,
    value: &Option<String>,
    max: usize,
) -> Result<(), ValidationError> {
    if let Some(v) = value {
        validate_length(field, v, max)?;
    }
    Ok(())
}

pub fn validate_email_format(field: &str, email: &str) -> Result<(), ValidationError> {
    validate_length(field, email, 254)?;

    let parts: Vec<&str> = email.splitn(2, '@').collect();
    if parts.len() != 2 {
        return Err(ValidationError::invalid(field, "must contain @"));
    }
    let (local, domain) = (parts[0], parts[1]);

    if local.is_empty() || local.len() > 64 {
        return Err(ValidationError::invalid(field, "invalid local part"));
    }
    if domain.is_empty() || !domain.contains('.') {
        return Err(ValidationError::invalid(field, "invalid domain"));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(ValidationError::invalid(field, "invalid domain"));
    }

    Ok(())
}

pub fn validate_webhook_url(
    field: &str,
    url_str: &str,
    is_testnet: bool,
) -> Result<(), ValidationError> {
    validate_length(field, url_str, 2000)?;

    if is_testnet {
        if !url_str.starts_with("https://") && !url_str.starts_with("http://") {
            return Err(ValidationError::invalid(field, "must start with http:// or https://"));
        }
    } else if !url_str.starts_with("https://") {
        return Err(ValidationError::invalid(field, "must start with https:// in production"));
    }

    let parsed = url::Url::parse(url_str)
        .map_err(|_| ValidationError::invalid(field, "invalid URL"))?;

    let host = match parsed.host_str() {
        Some(h) => h.to_string(),
        None => return Err(ValidationError::invalid(field, "missing hostname")),
    };

    if parsed.username() != "" || parsed.password().is_some() {
        return Err(ValidationError::invalid(field, "URL must not contain credentials"));
    }

    if is_private_host(&host) {
        return Err(ValidationError::invalid(field, "internal/private addresses are not allowed"));
    }

    Ok(())
}

pub fn validate_zcash_address(field: &str, addr: &str) -> Result<(), ValidationError> {
    validate_length(field, addr, 500)?;

    ZcashAddress::try_from_encoded(addr).map_err(|_| {
        ValidationError::invalid(
            field,
            "must be a valid Zcash address (failed checksum/encoding validation)",
        )
    })?;

    Ok(())
}

fn is_private_host(host: &str) -> bool {
    let lower = host.to_lowercase();
    if lower == "localhost" || lower.ends_with(".local") || lower.ends_with(".internal") {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(&ip);
    }

    false
}

pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.octets()[0] == 0xfd        // unique-local (fd00::/8)
                || v6.octets()[0] == 0xfc        // unique-local (fc00::/8)
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80) // link-local (fe80::/10)
        }
    }
}

/// DNS-level SSRF check: resolve hostname and verify none of the IPs are private.
/// Used at webhook dispatch time (not at URL save time) to catch DNS rebinding.
/// Fails closed: if DNS resolution fails, the request is blocked.
pub fn resolve_and_check_host(url: &str) -> Result<(), String> {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Err("invalid URL".to_string()),
    };

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Err("missing hostname".to_string()),
    };

    if parsed.username() != "" || parsed.password().is_some() {
        return Err("URL must not contain credentials".to_string());
    }

    let port = parsed.port().unwrap_or(443);
    let with_port = format!("{}:{}", host, port);

    match with_port.to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            if addrs.is_empty() {
                return Err("DNS resolved to no addresses".to_string());
            }
            for addr in &addrs {
                if is_private_ip(&addr.ip()) {
                    return Err(format!("webhook URL resolves to private IP: {}", addr.ip()));
                }
            }
            Ok(())
        }
        Err(e) => Err(format!("DNS resolution failed: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_length() {
        assert!(validate_length("name", "hello", 100).is_ok());
        assert!(validate_length("name", &"x".repeat(101), 100).is_err());
    }

    #[test]
    fn test_validate_email() {
        assert!(validate_email_format("email", "user@example.com").is_ok());
        assert!(validate_email_format("email", "user@sub.domain.com").is_ok());
        assert!(validate_email_format("email", "noatsign").is_err());
        assert!(validate_email_format("email", "@domain.com").is_err());
        assert!(validate_email_format("email", "user@").is_err());
        assert!(validate_email_format("email", "user@domain").is_err());
        assert!(validate_email_format("email", "user@.domain.com").is_err());
    }

    #[test]
    fn test_validate_webhook_url() {
        assert!(validate_webhook_url("url", "https://example.com/hook", false).is_ok());
        assert!(validate_webhook_url("url", "http://example.com/hook", false).is_err());
        assert!(validate_webhook_url("url", "http://example.com/hook", true).is_ok());
        assert!(validate_webhook_url("url", "https://localhost/hook", false).is_err());
        assert!(validate_webhook_url("url", "https://127.0.0.1/hook", false).is_err());
        assert!(validate_webhook_url("url", "https://192.168.1.1/hook", false).is_err());
        // userinfo bypass attempt
        assert!(validate_webhook_url("url", "https://evil@localhost/hook", false).is_err());
        assert!(validate_webhook_url("url", "https://user:pass@example.com/hook", false).is_err());
    }

    #[test]
    fn test_validate_zcash_address() {
        // Valid addresses should pass, invalid ones should fail
        assert!(validate_zcash_address("addr", "invalid123").is_err());
        assert!(validate_zcash_address("addr", "bc1qxyz").is_err());
        assert!(validate_zcash_address("addr", "u1abc123").is_err()); // invalid checksum
        assert!(validate_zcash_address("addr", "").is_err());
        // A properly encoded t-address would pass; we verify the crate rejects garbage
        assert!(validate_zcash_address("addr", "t1000000000000000000000000000000000").is_err());
    }

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.1.1".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
        // IPv6 unique-local and link-local
        assert!(is_private_ip(&"fd00::1".parse().unwrap()));
        assert!(is_private_ip(&"fe80::1".parse().unwrap()));
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"2607:f8b0:4004:800::200e".parse().unwrap())); // Google public IPv6
    }
}

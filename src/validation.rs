use std::net::{IpAddr, ToSocketAddrs};

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
    url: &str,
    is_testnet: bool,
) -> Result<(), ValidationError> {
    validate_length(field, url, 2000)?;

    if is_testnet {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ValidationError::invalid(field, "must start with http:// or https://"));
        }
    } else if !url.starts_with("https://") {
        return Err(ValidationError::invalid(field, "must start with https:// in production"));
    }

    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    if host.is_empty() {
        return Err(ValidationError::invalid(field, "missing hostname"));
    }

    if is_private_host(host) {
        return Err(ValidationError::invalid(field, "internal/private addresses are not allowed"));
    }

    Ok(())
}

pub fn validate_zcash_address(field: &str, addr: &str) -> Result<(), ValidationError> {
    validate_length(field, addr, 500)?;

    let valid_prefixes = ["u1", "utest1", "zs1", "ztestsapling", "t1", "t3"];
    if !valid_prefixes.iter().any(|p| addr.starts_with(p)) {
        return Err(ValidationError::invalid(
            field,
            "must be a valid Zcash address (u1, utest1, zs1, t1, or t3 prefix)",
        ));
    }

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
                || v4.octets()[0] == 169 && v4.octets()[1] == 254
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// DNS-level SSRF check: resolve hostname and verify none of the IPs are private.
/// Used at webhook dispatch time (not at URL save time) to catch DNS rebinding.
pub fn resolve_and_check_host(url: &str) -> Result<(), String> {
    let host_port = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");

    let with_port = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{}:443", host_port)
    };

    match with_port.to_socket_addrs() {
        Ok(addrs) => {
            for addr in addrs {
                if is_private_ip(&addr.ip()) {
                    return Err(format!("webhook URL resolves to private IP: {}", addr.ip()));
                }
            }
            Ok(())
        }
        Err(_) => Ok(()),
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
    }

    #[test]
    fn test_validate_zcash_address() {
        assert!(validate_zcash_address("addr", "u1abc123").is_ok());
        assert!(validate_zcash_address("addr", "utest1abc").is_ok());
        assert!(validate_zcash_address("addr", "t1abc").is_ok());
        assert!(validate_zcash_address("addr", "zs1abc").is_ok());
        assert!(validate_zcash_address("addr", "invalid123").is_err());
        assert!(validate_zcash_address("addr", "bc1qxyz").is_err());
    }

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.1.1".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
    }
}

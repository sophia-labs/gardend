use axum::http::header;
use std::{net::IpAddr, time::Duration};

const LOCAL_WEB_FETCH_TIMEOUT_SECS: u64 = 30;
const LOCAL_WEB_FETCH_MAX_REDIRECTS: usize = 10;

pub fn validate_http_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| "Invalid URL".to_string())?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        _ => Err("Only HTTP and HTTPS URLs are supported".to_string()),
    }
}

fn local_import_ip_block_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            if ip.is_loopback() {
                Some("loopback")
            } else if ip.is_private() {
                Some("private")
            } else if ip.is_link_local() {
                Some("link-local")
            } else if ip.is_unspecified() {
                Some("unspecified")
            } else if ip.is_broadcast() {
                Some("broadcast")
            } else if ip.is_multicast() {
                Some("multicast")
            } else if octets[0] == 0 {
                Some("this-network")
            } else if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                Some("carrier-grade-nat")
            } else if octets[0] == 169 && octets[1] == 254 {
                Some("link-local-metadata")
            } else if octets[0] == 198 && (18..=19).contains(&octets[1]) {
                Some("benchmark-private")
            } else if octets[0] >= 240 {
                Some("reserved")
            } else {
                None
            }
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return local_import_ip_block_reason(IpAddr::V4(mapped));
            }
            if ip.is_loopback() {
                Some("loopback")
            } else if ip.is_unspecified() {
                Some("unspecified")
            } else if ip.is_multicast() {
                Some("multicast")
            } else if (segments[0] & 0xfe00) == 0xfc00 {
                Some("unique-local")
            } else if (segments[0] & 0xffc0) == 0xfe80 {
                Some("link-local")
            } else {
                None
            }
        }
    }
}

async fn validate_local_import_fetch_target(url: &reqwest::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL host is required".to_string())?
        .trim()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Err("Local/private network URLs are not allowed for imports".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Some(reason) = local_import_ip_block_reason(ip) {
            return Err(format!(
                "Local/private network URLs are not allowed for imports ({reason})"
            ));
        }
        return Ok(());
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL port could not be inferred".to_string())?;
    let addresses = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|error| format!("Failed to resolve URL host: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("URL host did not resolve".to_string());
    }
    for address in addresses {
        if let Some(reason) = local_import_ip_block_reason(address.ip()) {
            return Err(format!(
                "Local/private network URLs are not allowed for imports ({reason})"
            ));
        }
    }
    Ok(())
}

pub fn local_web_fetch_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(LOCAL_WEB_FETCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Mozilla/5.0 (compatible; Mnemosyne/1.0; +https://mnemosyne.me)")
        .build()
        .map_err(|error| format!("create local web fetch client: {error}"))
}

pub async fn fetch_limited_url_bytes(
    client: &reqwest::Client,
    url: reqwest::Url,
    max_bytes: usize,
) -> Result<(Vec<u8>, String), String> {
    let mut url = url;
    for redirect_count in 0..=LOCAL_WEB_FETCH_MAX_REDIRECTS {
        validate_local_import_fetch_target(&url).await?;
        let response = client.get(url.clone()).send().await.map_err(|error| {
            if error.is_timeout() {
                format!("Request timed out after {LOCAL_WEB_FETCH_TIMEOUT_SECS}s")
            } else {
                format!("Request failed: {error}")
            }
        })?;
        let status = response.status();
        if status.is_redirection() {
            if redirect_count >= LOCAL_WEB_FETCH_MAX_REDIRECTS {
                return Err("Too many redirects".to_string());
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "Redirect response omitted Location header".to_string())?;
            let redirect_url = url
                .join(location)
                .map_err(|_| "Invalid redirect URL".to_string())?;
            url = validate_http_url(redirect_url.as_str())?;
            continue;
        }
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or("HTTP error");
            return Err(format!("HTTP {}: {reason}", status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length as usize > max_bytes)
        {
            return Err(format!("Response too large (>{max_bytes} bytes)"));
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("Failed to read response body: {error}"))?;
        if bytes.len() > max_bytes {
            return Err(format!("Response too large ({} bytes)", bytes.len()));
        }
        return Ok((bytes.to_vec(), content_type));
    }
    Err("Too many redirects".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn validate_http_url_rejects_non_http_schemes() {
        assert!(validate_http_url("https://example.com").is_ok());
        assert!(validate_http_url("http://example.com").is_ok());
        assert_eq!(
            validate_http_url("file:///tmp/example").expect_err("file URLs must be rejected"),
            "Only HTTP and HTTPS URLs are supported"
        );
    }

    #[test]
    fn local_import_ip_block_reason_covers_private_ranges() {
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            Some("loopback")
        );
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))),
            Some("private")
        );
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
            Some("link-local")
        );
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            Some("loopback")
        );
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V6("fc00::1".parse().unwrap())),
            Some("unique-local")
        );
        assert_eq!(
            local_import_ip_block_reason(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))),
            None
        );
    }
}

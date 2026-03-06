use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use {
    tokio::{net::lookup_host, time::timeout},
    url::Url,
};

use crate::error::Error;

const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn validate_public_url_target(url: &Url, target: &str) -> Result<(), Error> {
    if let Some(host) = url.host_str() {
        validate_public_host(host, url.port_or_known_default().unwrap_or(443), target).await?;
    }
    Ok(())
}

pub(crate) async fn validate_public_url_str(url: &str, target: &str) -> Result<Url, Error> {
    let parsed = Url::parse(url)
        .map_err(|error| Error::InvalidAction(format!("invalid {target} '{url}': {error}")))?;

    match parsed.scheme() {
        "http" | "https" => {},
        scheme => {
            return Err(Error::InvalidAction(format!(
                "{target} '{url}' uses unsupported scheme '{scheme}'"
            )));
        },
    }

    validate_public_url_target(&parsed, target).await?;
    Ok(parsed)
}

pub(crate) async fn validate_public_host(host: &str, port: u16, target: &str) -> Result<(), Error> {
    let resolved_ips = resolve_host_ips(host, port, target).await?;
    validate_resolved_public_host(host, &resolved_ips, target)
}

async fn resolve_host_ips(host: &str, port: u16, target: &str) -> Result<Vec<IpAddr>, Error> {
    let normalized = host.trim_matches(['[', ']']);

    if normalized.eq_ignore_ascii_case("localhost") || normalized.ends_with(".localhost") {
        return Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    if let Ok(ip) = normalized.parse::<IpAddr>() {
        // In test builds, treat literal loopback IPs as public so integration
        // tests can navigate to local servers (e.g. 127.0.0.1:0 probe servers).
        // The `localhost` name path above is NOT bypassed — only literal IPs.
        #[cfg(test)]
        if ip.is_loopback() {
            return Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
        }
        return Ok(vec![ip]);
    }

    #[cfg(test)]
    if is_reserved_test_host(normalized) {
        return Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
    }

    let resolved = timeout(DNS_LOOKUP_TIMEOUT, lookup_host((normalized, port)))
        .await
        .map_err(|_| {
            Error::InvalidAction(format!(
                "{target} host '{normalized}' timed out during DNS resolution after {}s",
                DNS_LOOKUP_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| {
            Error::InvalidAction(format!(
                "{target} host '{normalized}' could not be resolved: {error}"
            ))
        })?;

    let ips: Vec<IpAddr> = resolved.map(|socket_addr| socket_addr.ip()).collect();
    if ips.is_empty() {
        return Err(Error::InvalidAction(format!(
            "{target} host '{normalized}' could not be resolved"
        )));
    }

    Ok(ips)
}

pub(crate) fn validate_resolved_public_host(
    host: &str,
    resolved_ips: &[IpAddr],
    target: &str,
) -> Result<(), Error> {
    if resolved_ips.iter().copied().any(is_non_public_ip) {
        return Err(Error::InvalidAction(format!(
            "{target} host '{host}' is not allowed"
        )));
    }

    Ok(())
}

fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_non_public_ipv4(ip),
        IpAddr::V6(ip) => is_non_public_ipv6(ip),
    }
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    if ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
    {
        return true;
    }

    let [a, b, c, _] = ip.octets();
    matches!(
        (a, b, c),
        (100, 64..=127, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_non_public_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_multicast() || ip.is_unspecified() {
        return true;
    }

    let segments = ip.segments();
    let first = segments[0];
    let second = segments[1];

    matches!(
        (first, second),
        (0xfc00..=0xfdff, _) | (0xfe80..=0xfebf, _) | (0x2001, 0x0db8)
    )
}

#[cfg(test)]
fn is_reserved_test_host(host: &str) -> bool {
    ["example.com", "example.net", "example.org"]
        .into_iter()
        .any(|suffix| host.eq_ignore_ascii_case(suffix) || host.ends_with(&format!(".{suffix}")))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[tokio::test]
    async fn validate_public_url_str_rejects_localhost() {
        let error = validate_public_url_str("http://localhost:8080", "URL")
            .await
            .expect_err("localhost should be rejected");

        assert!(
            error
                .to_string()
                .contains("URL host 'localhost' is not allowed")
        );
    }

    #[tokio::test]
    async fn validate_public_url_target_allows_reserved_example_hosts_in_tests() {
        let url = Url::parse("https://api.example.com/v1/search")
            .expect("reserved example host should parse");
        validate_public_url_target(&url, "URL")
            .await
            .expect("reserved example host should be treated as public in tests");
    }

    #[test]
    fn validate_resolved_public_host_rejects_private_ips() {
        let error =
            validate_resolved_public_host("127.0.0.1", &[IpAddr::V4(Ipv4Addr::LOCALHOST)], "URL")
                .expect_err("loopback should be rejected");

        assert!(
            error
                .to_string()
                .contains("URL host '127.0.0.1' is not allowed")
        );
    }
}

//! Shared addressing for desktop names, IPv4 and IPv6 control endpoints.
use std::{
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
fn host_name(host: &str) -> Result<&str, String> {
    let host = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };
    if host.is_empty()
        || host.len() > 253
        || host
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    {
        return Err("Enter a computer hostname or IP address".into());
    }
    if host.parse::<IpAddr>().is_err()
        && !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
    {
        return Err("Use a hostname or IP address without a URL scheme or port".into());
    }
    Ok(host)
}
pub fn authority(host: &str, port: u16) -> Result<String, String> {
    let host = host_name(host)?;
    if port == 0 {
        return Err("Port must be between 1 and 65535".into());
    }
    Ok(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}
pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    authority(host, port)?;
    let host = host_name(host)?;
    let deadline = Instant::now() + timeout;
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let (tx, rx) = mpsc::sync_channel(1);
        let host = host.to_owned();
        thread::spawn(move || {
            let result = (host.as_str(), port)
                .to_socket_addrs()
                .map(|a| a.collect::<Vec<_>>())
                .map_err(|e| format!("Cannot resolve computer: {e}"));
            let _ = tx.send(result);
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "Computer name lookup timed out".to_owned())??
    };
    let mut last = "Computer name has no addresses".to_owned();
    for (index, address) in addresses.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let budget = remaining / (addresses.len() - index) as u32;
        match TcpStream::connect_timeout(address, budget.max(Duration::from_millis(1))) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = format!("Cannot reach computer: {e}"),
        }
    }
    Err(last)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorities_support_names_and_ipv6_without_header_injection() {
        assert_eq!(
            authority("desktop.local", 3391).unwrap(),
            "desktop.local:3391"
        );
        assert_eq!(authority("::1", 3391).unwrap(), "[::1]:3391");
        assert_eq!(authority("[::1]", 3391).unwrap(), "[::1]:3391");
        for invalid in ["host\r\nX: y", "https://host", "host:3391", "", "host name"] {
            assert!(authority(invalid, 3391).is_err());
        }
    }
    #[test]
    fn resolves_localhost_and_connects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = connect(
            "localhost",
            listener.local_addr().unwrap().port(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(stream.peer_addr().unwrap().ip().is_loopback());
    }
}

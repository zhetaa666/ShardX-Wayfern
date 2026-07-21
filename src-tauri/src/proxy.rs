use crate::{settings, store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Socks5,
    Http,
    Https,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEntry {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// "PL", "US", …
    #[serde(default)]
    pub country: String,
    /// Free-form note.
    #[serde(default)]
    pub notes: String,
}

impl ProxyEntry {
    fn connection_signature(&self) -> String {
        let mut hasher = Sha256::new();
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        hasher.update(scheme.as_bytes());
        hasher.update([0]);
        hasher.update(self.host.as_bytes());
        hasher.update([0]);
        hasher.update(self.port.to_be_bytes());
        hasher.update([0]);
        hasher.update(self.username.as_bytes());
        hasher.update([0]);
        hasher.update(self.password.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn same_connection(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.password == other.password
    }

    /// Build `--proxy-server=<scheme>://[user:pass@]host:port` for ShardX.
    pub fn to_proxy_server_arg(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        let host_port = format!("{}:{}", self.host, self.port);
        if self.username.is_empty() && self.password.is_empty() {
            format!("{scheme}://{host_port}")
        } else {
            let user = url::form_urlencoded::byte_serialize(self.username.as_bytes())
                .collect::<String>();
            let pass = url::form_urlencoded::byte_serialize(self.password.as_bytes())
                .collect::<String>();
            format!("{scheme}://{user}:{pass}@{host_port}")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyStore {
    #[serde(default)]
    pub proxies: Vec<ProxyEntry>,
}

pub fn load() -> Result<ProxyStore> {
    let path = store::proxies_path()?;
    if !path.exists() {
        return Ok(ProxyStore::default());
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save(s: &ProxyStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    fs::write(store::proxies_path()?, body)?;
    Ok(())
}

pub fn list() -> Result<Vec<ProxyEntry>> {
    Ok(load()?.proxies)
}

pub fn upsert(mut entry: ProxyEntry) -> Result<ProxyEntry> {
    if entry.id.is_empty() {
        entry.id = uuid::Uuid::new_v4().to_string();
    }
    let mut s = load()?;
    let connection_changed = if let Some(slot) = s.proxies.iter_mut().find(|p| p.id == entry.id) {
        let changed = !slot.same_connection(&entry);
        *slot = entry.clone();
        changed
    } else {
        s.proxies.push(entry.clone());
        true
    };
    save(&s)?;
    if connection_changed {
        clear_cache_key(&entry.id)?;
    }
    Ok(entry)
}

/// Upsert that reuses an entry with the same effective connection.
pub fn upsert_dedup(mut entry: ProxyEntry) -> Result<ProxyEntry> {
    let mut s = load()?;
    if let Some(existing) = s.proxies.iter().find(|p| p.same_connection(&entry)) {
        return Ok(existing.clone());
    }
    if entry.id.is_empty() {
        entry.id = uuid::Uuid::new_v4().to_string();
    }
    s.proxies.push(entry.clone());
    save(&s)?;
    Ok(entry)
}

pub fn delete(id: &str) -> Result<()> {
    let mut s = load()?;
    s.proxies.retain(|p| p.id != id);
    save(&s)?;
    // Also wipe persisted test history and cache identity.
    let mut hs = load_history()?;
    if hs.by_proxy.remove(id).is_some() {
        save_history(&hs)?;
    }
    clear_cache_key(id)?;
    Ok(())
}

pub fn get(id: &str) -> Result<Option<ProxyEntry>> {
    Ok(load()?.proxies.into_iter().find(|p| p.id == id))
}

/// SOCKS5/HTTP CONNECT probe; returns RTT in ms on success.
pub async fn probe(entry: &ProxyEntry) -> Result<u128> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration, Instant};

    let started = Instant::now();
    let addr = format!("{}:{}", entry.host, entry.port);
    let mut stream = timeout(Duration::from_secs(8), TcpStream::connect(&addr))
        .await
        .context("connect timeout")??;

    match entry.kind {
        ProxyKind::Socks5 => {
            // RFC 1928 §3 greeting
            let auth_method: u8 = if entry.username.is_empty() { 0x00 } else { 0x02 };
            stream.write_all(&[0x05, 0x01, auth_method]).await?;
            let mut resp = [0u8; 2];
            stream.read_exact(&mut resp).await?;
            if resp[0] != 0x05 {
                anyhow::bail!("not SOCKS5");
            }
            if resp[1] == 0xFF {
                anyhow::bail!("no acceptable auth method");
            }
            if auth_method == 0x02 {
                // RFC 1929 user/pass sub-negotiation
                let mut buf = vec![0x01u8];
                buf.push(entry.username.len() as u8);
                buf.extend_from_slice(entry.username.as_bytes());
                buf.push(entry.password.len() as u8);
                buf.extend_from_slice(entry.password.as_bytes());
                stream.write_all(&buf).await?;
                let mut auth_resp = [0u8; 2];
                stream.read_exact(&mut auth_resp).await?;
                if auth_resp[1] != 0x00 {
                    anyhow::bail!("auth failed");
                }
            }
        }
        ProxyKind::Http | ProxyKind::Https => {
            // CONNECT with Basic auth; read until CRLFCRLF to avoid clipping headers.
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let mut req = String::from(
                "CONNECT example.com:443 HTTP/1.1\r\n\
                 Host: example.com:443\r\n",
            );
            if !entry.username.is_empty() || !entry.password.is_empty() {
                let creds = format!("{}:{}", entry.username, entry.password);
                let encoded = STANDARD.encode(creds.as_bytes());
                req.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
            }
            req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
            stream.write_all(req.as_bytes()).await?;

            // Read until CRLFCRLF or 4 KB cap.
            let mut buf = Vec::with_capacity(512);
            let mut tmp = [0u8; 256];
            let head: String = loop {
                let n = timeout(Duration::from_secs(8), stream.read(&mut tmp))
                    .await
                    .context("read timeout")??;
                if n == 0 { break String::from_utf8_lossy(&buf).to_string(); }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 4096 {
                    break String::from_utf8_lossy(&buf).to_string();
                }
            };
            let first_line = head.lines().next().unwrap_or("");
            if !first_line.starts_with("HTTP/1.1 200") && !first_line.starts_with("HTTP/1.0 200") {
                anyhow::bail!("CONNECT failed: {first_line}");
            }
        }
    }
    Ok(started.elapsed().as_millis())
}

// ---- Bulk import ----
//
// Accepted: socks5://user:pass@host:port, user:pass@host:port, host:port:user:pass,
//           host:port@user:pass, host:port. `#` lines and trailing `# country=X note=Y`
//           supported. SOCKS5 default kind when scheme missing.

/// Parse a single proxy line for inline (unsaved) use by the API.
pub fn parse_single(line: &str) -> Option<ProxyEntry> {
    parse_one(line.trim(), &ProxyKind::Socks5)
}

pub fn parse_bulk(text: &str, default_kind: ProxyKind) -> Vec<ProxyEntry> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(p) = parse_one(line, &default_kind) {
            out.push(p);
        }
    }
    out
}

fn parse_one(line: &str, default_kind: &ProxyKind) -> Option<ProxyEntry> {
    // Optional trailing `# country=US note=foo`.
    let (main, comment) = match line.find('#') {
        Some(i) => (line[..i].trim(), Some(line[i + 1..].trim())),
        None => (line, None),
    };
    let (kind, rest) = if let Some(r) = main.strip_prefix("socks5://") {
        (ProxyKind::Socks5, r)
    } else if let Some(r) = main.strip_prefix("https://") {
        (ProxyKind::Https, r)
    } else if let Some(r) = main.strip_prefix("http://") {
        (ProxyKind::Http, r)
    } else {
        (default_kind.clone(), main)
    };

    let (host_part, user, pass) = if let Some((u, hp)) = rest.split_once('@') {
        let (un, pw) = u.split_once(':').unwrap_or((u, ""));
        (hp.to_string(), un.to_string(), pw.to_string())
    } else {
        // host:port or host:port:user:pass
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.len() {
            2 => (rest.to_string(), String::new(), String::new()),
            4 => (
                format!("{}:{}", parts[0], parts[1]),
                parts[2].to_string(),
                parts[3].to_string(),
            ),
            _ => return None,
        }
    };

    let (host, port_s) = host_part.rsplit_once(':')?;
    let port: u16 = port_s.parse().ok()?;
    let mut country = String::new();
    let mut notes = String::new();
    if let Some(c) = comment {
        for kv in c.split_whitespace() {
            if let Some(v) = kv.strip_prefix("country=") {
                country = v.to_string();
            } else if let Some(v) = kv.strip_prefix("note=") {
                notes = v.to_string();
            }
        }
    }
    Some(ProxyEntry {
        // ID assigned now so pre-save test snapshots key under the kept uuid.
        id: uuid::Uuid::new_v4().to_string(),
        name: format!("{host}:{port}"),
        kind,
        host: host.to_string(),
        port,
        username: user,
        password: pass,
        country,
        notes,
    })
}

/// Save many entries; returns count actually persisted (deduped on host:port:user).
pub fn bulk_save(entries: Vec<ProxyEntry>) -> Result<usize> {
    let mut store_data = load()?;
    let mut added = 0usize;
    for mut e in entries {
        let dup = store_data
            .proxies
            .iter()
            .any(|x| x.host == e.host && x.port == e.port && x.username == e.username);
        if dup {
            continue;
        }
        if e.id.is_empty() {
            e.id = uuid::Uuid::new_v4().to_string();
        }
        store_data.proxies.push(e);
        added += 1;
    }
    save(&store_data)?;
    Ok(added)
}

// ---- UDP probe (SOCKS5 UDP_ASSOCIATE; RFC 1928 §7) ----

/// Resolve a public STUN server to IPv4 (probe target for the UDP relay).
async fn resolve_stun_ipv4() -> Result<(std::net::Ipv4Addr, u16)> {
    const HOSTS: &[&str] = &[
        "stun.l.google.com:19302",
        "stun1.l.google.com:19302",
        "stun.cloudflare.com:3478",
    ];
    for h in HOSTS {
        if let Ok(addrs) = tokio::net::lookup_host(*h).await {
            for a in addrs {
                if let std::net::IpAddr::V4(v4) = a.ip() {
                    return Ok((v4, a.port()));
                }
            }
        }
    }
    anyhow::bail!("no STUN server resolved to IPv4")
}

pub async fn probe_udp(entry: &ProxyEntry) -> Result<u128> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::time::{timeout, Duration, Instant};

    if !matches!(entry.kind, ProxyKind::Socks5) {
        anyhow::bail!("UDP probe only supported for SOCKS5");
    }
    let started = Instant::now();
    let mut tcp = timeout(
        Duration::from_secs(8),
        TcpStream::connect(format!("{}:{}", entry.host, entry.port)),
    )
    .await
    .context("connect timeout")??;

    let auth_method: u8 = if entry.username.is_empty() { 0x00 } else { 0x02 };
    tcp.write_all(&[0x05, 0x01, auth_method]).await?;
    let mut greet = [0u8; 2];
    tcp.read_exact(&mut greet).await?;
    if greet[1] == 0xFF {
        anyhow::bail!("no acceptable auth method");
    }
    if auth_method == 0x02 {
        let mut buf = vec![0x01u8];
        buf.push(entry.username.len() as u8);
        buf.extend_from_slice(entry.username.as_bytes());
        buf.push(entry.password.len() as u8);
        buf.extend_from_slice(entry.password.as_bytes());
        tcp.write_all(&buf).await?;
        let mut ar = [0u8; 2];
        tcp.read_exact(&mut ar).await?;
        if ar[1] != 0x00 {
            anyhow::bail!("auth failed");
        }
    }
    // UDP_ASSOCIATE: cmd=0x03, ATYP=IPv4, addr=0.0.0.0, port=0
    tcp.write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    let mut hdr = [0u8; 4];
    tcp.read_exact(&mut hdr).await?;
    if hdr[1] != 0x00 {
        anyhow::bail!("UDP_ASSOCIATE refused (rep={:#x})", hdr[1]);
    }
    let bind_addr: SocketAddr = match hdr[3] {
        0x01 => {
            // IPv4
            let mut ip = [0u8; 4];
            tcp.read_exact(&mut ip).await?;
            let mut p = [0u8; 2];
            tcp.read_exact(&mut p).await?;
            let port = u16::from_be_bytes(p);
            let v4 = std::net::Ipv4Addr::from(ip);
            // 0.0.0.0 → fall back to TCP peer (where the relay lives).
            if v4.is_unspecified() {
                let peer = tcp.peer_addr()?;
                SocketAddr::new(peer.ip(), port)
            } else {
                SocketAddr::new(std::net::IpAddr::V4(v4), port)
            }
        }
        0x04 => {
            let mut ip = [0u8; 16];
            tcp.read_exact(&mut ip).await?;
            let mut p = [0u8; 2];
            tcp.read_exact(&mut p).await?;
            SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::from(ip)), u16::from_be_bytes(p))
        }
        _ => anyhow::bail!("unsupported ATYP in UDP reply"),
    };

    // Probe with STUN binding request (DNS-port-53 often blocked, STUN passes).
    let (stun_ip, stun_port) = resolve_stun_ipv4()
        .await
        .context("could not resolve a STUN server to probe UDP with")?;

    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(bind_addr).await?;
    let mut pkt: Vec<u8> = Vec::with_capacity(32);
    // SOCKS5 UDP header: RSV(2)=0, FRAG=0, ATYP=IPv4, DST=<stun>, PORT.
    pkt.extend_from_slice(&[0, 0, 0, 0x01]);
    pkt.extend_from_slice(&stun_ip.octets());
    pkt.extend_from_slice(&stun_port.to_be_bytes());
    // STUN Binding Request (RFC 5389): type=0x0001, magic 0x2112A442, 12B txid.
    let mut stun = vec![0x00u8, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
    stun.extend_from_slice(&uuid::Uuid::new_v4().as_bytes()[..12]);
    pkt.extend_from_slice(&stun);
    udp.send(&pkt).await?;

    let mut buf = vec![0u8; 1500];
    let n = timeout(Duration::from_secs(6), udp.recv(&mut buf))
        .await
        .context("UDP reply timeout — proxy doesn't relay UDP")??;
    if n < 20 {
        anyhow::bail!("UDP reply too short");
    }
    // RFC 1928: dropping TCP control tears down the relay; keep it alive.
    drop(tcp);
    Ok(started.elapsed().as_millis())
}

// ---- Geo lookup ----

#[derive(Debug, Clone, Serialize)]
pub struct GeoInfo {
    pub ip: String,
    pub country: String,
    /// ISO 3166-1 alpha-2.
    pub country_code: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    pub timezone: String,
    pub latitude: f64,
    pub longitude: f64,
    pub provider: String,
}

fn string_value(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .unwrap_or("")
        .to_string()
}

fn float_value(value: &serde_json::Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(|field| {
            field
                .as_f64()
                .or_else(|| field.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostPublicIps {
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub updated_at: String,
}

fn normalize_ip_list<I>(values: I) -> Vec<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut addresses = Vec::new();
    for value in values {
        for candidate in value.as_ref().split(',') {
            let candidate = candidate.trim();
            if candidate.parse::<std::net::IpAddr>().is_ok()
                && !addresses.iter().any(|existing| existing == candidate)
            {
                addresses.push(candidate.to_string());
            }
        }
    }
    addresses
}

fn load_host_public_ips() -> Option<HostPublicIps> {
    let path = store::host_public_ips_path().ok()?;
    let body = fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

pub fn cached_host_public_ips() -> Vec<String> {
    load_host_public_ips()
        .map(|cache| normalize_ip_list(cache.addresses))
        .unwrap_or_default()
}

fn host_public_ips_cache_is_fresh(cache: &HostPublicIps) -> bool {
    const MAX_AGE_SECS: u64 = 300;
    let Some(updated_at) = cache.updated_at.strip_prefix('@') else {
        return false;
    };
    let Ok(updated_at) = updated_at.parse::<u64>() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    now.saturating_sub(updated_at) <= MAX_AGE_SECS
}

async fn refresh_host_public_ips() -> Result<Vec<String>> {
    let geo = geo_check_via(None, None).await?;
    let addresses = normalize_ip_list([geo.ip]);
    if addresses.is_empty() {
        anyhow::bail!("direct GeoIP check did not return a valid public IP address");
    }
    let cache = HostPublicIps {
        addresses: addresses.clone(),
        updated_at: unix_now(),
    };
    fs::write(
        store::host_public_ips_path()?,
        serde_json::to_string_pretty(&cache)?,
    )?;
    Ok(addresses)
}

pub async fn ensure_host_public_ips() -> Result<Vec<String>> {
    if let Some(cache) = load_host_public_ips() {
        let addresses = normalize_ip_list(&cache.addresses);
        if !addresses.is_empty() && host_public_ips_cache_is_fresh(&cache) {
            return Ok(addresses);
        }
    }
    refresh_host_public_ips().await
}

fn parse_ixbrowser_geo(body: &serde_json::Value, provider: String) -> Result<GeoInfo> {
    if string_value(body, "status") != "success" {
        anyhow::bail!("ixbrowser.com: {}", string_value(body, "message"));
    }
    let region_name = string_value(body, "regionName");
    Ok(GeoInfo {
        ip: string_value(body, "query"),
        country: string_value(body, "country"),
        country_code: string_value(body, "countryCode"),
        region: if region_name.is_empty() {
            string_value(body, "region")
        } else {
            region_name
        },
        city: string_value(body, "city"),
        isp: string_value(body, "isp"),
        timezone: string_value(body, "timezone"),
        latitude: float_value(body, "lat"),
        longitude: float_value(body, "lon"),
        provider,
    })
}

/// Probe IP/country the world sees when traffic exits the proxy.
pub async fn geo_check(entry: &ProxyEntry, provider_override: Option<String>) -> Result<GeoInfo> {
    geo_check_via(Some(entry), provider_override).await
}

/// Probe geo through `entry` if Some, else direct; provider default ixBrowser.
pub async fn geo_check_via(entry: Option<&ProxyEntry>, provider_override: Option<String>) -> Result<GeoInfo> {
    let configured = provider_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            settings::load()
                .ok()
                .and_then(|s| s.geo_checker)
                .unwrap_or_else(|| "ixbrowser.com".into())
        });
    let provider = match configured.as_str() {
        "ixbrowser.com" | "ip-api.com" | "ipapi.co" | "ipwho.is" => configured,
        _ => "ixbrowser.com".into(),
    };

    let url = match provider.as_str() {
        "ixbrowser.com" => "https://www.ixbrowser.com/api/ip-api?proxy_mode=2",
        "ip-api.com" => "http://ip-api.com/json/?fields=status,message,query,country,countryCode,regionName,city,isp,timezone,lat,lon",
        "ipapi.co" => "https://ipapi.co/json/",
        "ipwho.is" => "https://ipwho.is/",
        _ => "https://www.ixbrowser.com/api/ip-api?proxy_mode=2",
    };

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8));
    if let Some(entry) = entry {
        let scheme = match entry.kind {
            ProxyKind::Socks5 => "socks5h", // DNS via proxy
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        let proxy_url = if entry.username.is_empty() && entry.password.is_empty() {
            format!("{scheme}://{}:{}", entry.host, entry.port)
        } else {
            let user = url::form_urlencoded::byte_serialize(entry.username.as_bytes()).collect::<String>();
            let pass = url::form_urlencoded::byte_serialize(entry.password.as_bytes()).collect::<String>();
            format!("{scheme}://{user}:{pass}@{}:{}", entry.host, entry.port)
        };
        let proxy = reqwest::Proxy::all(&proxy_url).context("bad proxy URL")?;
        builder = builder.proxy(proxy);
    } else {
        // Direct check: bypass any system proxy.
        builder = builder.no_proxy();
    }
    let client = builder.build()?;

    let body: serde_json::Value = client.get(url).send().await?.json().await?;

    let s = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let f = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0.0)
    };
    let info = match provider.as_str() {
        "ixbrowser.com" => parse_ixbrowser_geo(&body, provider)?,
        "ip-api.com" => {
            if s(&body, "status") == "fail" {
                anyhow::bail!("ip-api.com: {}", s(&body, "message"));
            }
            GeoInfo {
                ip: s(&body, "query"),
                country: s(&body, "country"),
                country_code: s(&body, "countryCode"),
                region: s(&body, "regionName"),
                city: s(&body, "city"),
                isp: s(&body, "isp"),
                timezone: s(&body, "timezone"),
                latitude: f(&body, "lat"),
                longitude: f(&body, "lon"),
                provider,
            }
        }
        "ipapi.co" => GeoInfo {
            ip: s(&body, "ip"),
            country: s(&body, "country_name"),
            country_code: s(&body, "country_code"),
            region: s(&body, "region"),
            city: s(&body, "city"),
            isp: s(&body, "org"),
            timezone: s(&body, "timezone"),
            latitude: f(&body, "latitude"),
            longitude: f(&body, "longitude"),
            provider,
        },
        "ipwho.is" => GeoInfo {
            ip: s(&body, "ip"),
            country: s(&body, "country"),
            country_code: s(&body, "country_code"),
            region: s(&body, "region"),
            city: s(&body, "city"),
            isp: body.get("connection").and_then(|c| c.get("isp")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            timezone: body.get("timezone").and_then(|t| t.get("id")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            latitude: f(&body, "latitude"),
            longitude: f(&body, "longitude"),
            provider,
        },
        _ => GeoInfo {
            ip: s(&body, "query"),
            country: s(&body, "country"),
            country_code: s(&body, "countryCode"),
            region: String::new(),
            city: String::new(),
            isp: String::new(),
            timezone: String::new(),
            latitude: 0.0,
            longitude: 0.0,
            provider,
        },
    };
    Ok(info)
}

/// Map ISO-3166 alpha-2 to BCP-47 locale (coarse).
pub fn country_to_locale(cc: &str) -> &'static str {
    match cc.to_ascii_uppercase().as_str() {
        "US" => "en-US",
        "GB" | "UK" => "en-GB",
        "CA" => "en-CA",
        "AU" => "en-AU",
        "NZ" => "en-NZ",
        "IE" => "en-IE",
        "ZA" => "en-ZA",
        "IN" => "en-IN",
        "DE" => "de-DE",
        "AT" => "de-AT",
        "CH" => "de-CH",
        "FR" => "fr-FR",
        "BE" => "fr-BE",
        "ES" => "es-ES",
        "MX" => "es-MX",
        "AR" => "es-AR",
        "CO" => "es-CO",
        "CL" => "es-CL",
        "IT" => "it-IT",
        "NL" => "nl-NL",
        "PL" => "pl-PL",
        "BR" => "pt-BR",
        "PT" => "pt-PT",
        "RO" => "ro-RO",
        "RU" => "ru-RU",
        "BY" => "be-BY",
        "UA" => "uk-UA",
        "TR" => "tr-TR",
        "GR" => "el-GR",
        "CZ" => "cs-CZ",
        "SK" => "sk-SK",
        "HU" => "hu-HU",
        "SE" => "sv-SE",
        "FI" => "fi-FI",
        "NO" => "nb-NO",
        "DK" => "da-DK",
        "BG" => "bg-BG",
        "HR" => "hr-HR",
        "SI" => "sl-SI",
        "RS" => "sr-RS",
        "IL" => "he-IL",
        "SA" | "AE" | "EG" => "ar-SA",
        "ID" => "id-ID",
        "MY" => "ms-MY",
        "PH" => "fil-PH",
        "VN" => "vi-VN",
        "TH" => "th-TH",
        "CN" => "zh-CN",
        "HK" => "zh-HK",
        "TW" => "zh-TW",
        "JP" => "ja-JP",
        "KR" => "ko-KR",
        _ => "en-US",
    }
}

// ---- Test history ----

/// One observation of a proxy's exit state; same-IP consecutive entries collapse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSnapshot {
    pub first_seen: String,
    pub last_seen: String,
    pub ip: String,
    pub country_code: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    pub timezone: String,
    pub latitude: f64,
    pub longitude: f64,
    pub tcp_ms: Option<u128>,
    pub udp_ms: Option<u128>,
    pub udp_error: Option<String>,
    pub provider: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryStore {
    #[serde(default)]
    by_proxy: HashMap<String, Vec<TestSnapshot>>,
}

fn history_path() -> Result<PathBuf> {
    Ok(store::config_root()?.join("proxies-history.json"))
}

fn load_history() -> Result<HistoryStore> {
    let path = history_path()?;
    if !path.exists() {
        return Ok(HistoryStore::default());
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save_history(s: &HistoryStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    fs::write(history_path()?, body)?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProxyCacheKeys {
    #[serde(default)]
    by_proxy: HashMap<String, String>,
}

fn load_cache_keys() -> Result<ProxyCacheKeys> {
    let path = store::proxy_cache_keys_path()?;
    if !path.exists() {
        return Ok(ProxyCacheKeys::default());
    }
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save_cache_keys(keys: &ProxyCacheKeys) -> Result<()> {
    let body = serde_json::to_string_pretty(keys)?;
    fs::write(store::proxy_cache_keys_path()?, body)?;
    Ok(())
}

fn record_cache_key(entry: &ProxyEntry) -> Result<()> {
    if entry.id.is_empty() {
        return Ok(());
    }
    let mut keys = load_cache_keys()?;
    keys.by_proxy
        .insert(entry.id.clone(), entry.connection_signature());
    save_cache_keys(&keys)
}

fn clear_cache_key(proxy_id: &str) -> Result<()> {
    let mut keys = load_cache_keys()?;
    if keys.by_proxy.remove(proxy_id).is_some() {
        save_cache_keys(&keys)?;
    }
    Ok(())
}

fn cache_key_matches(entry: &ProxyEntry) -> bool {
    load_cache_keys()
        .ok()
        .and_then(|keys| keys.by_proxy.get(&entry.id).cloned())
        .map(|signature| signature == entry.connection_signature())
        .unwrap_or(false)
}

/// Persist a test result; same-IP consecutive entries collapse, capped at 50 per proxy.
fn record_test(proxy_id: &str, mut snap: TestSnapshot) -> Result<TestSnapshot> {
    if proxy_id.is_empty() {
        if snap.first_seen.is_empty() {
            snap.first_seen = snap.last_seen.clone();
        }
        return Ok(snap);
    }
    let mut hs = load_history()?;
    let entries = hs.by_proxy.entry(proxy_id.into()).or_default();
    if let Some(last) = entries.last_mut() {
        if !snap.ip.is_empty()
            && last.ip == snap.ip
        {
            last.last_seen = snap.last_seen.clone();
            last.tcp_ms = snap.tcp_ms;
            last.udp_ms = snap.udp_ms;
            last.udp_error = snap.udp_error.clone();
            let out = last.clone();
            save_history(&hs)?;
            return Ok(out);
        }
    }
    if snap.first_seen.is_empty() {
        snap.first_seen = snap.last_seen.clone();
    }
    entries.push(snap.clone());
    if entries.len() > 50 {
        let drop = entries.len() - 50;
        entries.drain(..drop);
    }
    save_history(&hs)?;
    Ok(snap)
}

pub fn history(proxy_id: &str) -> Result<Vec<TestSnapshot>> {
    let hs = load_history()?;
    Ok(hs.by_proxy.get(proxy_id).cloned().unwrap_or_default())
}

pub fn latest_test(proxy_id: &str) -> Option<TestSnapshot> {
    load_history()
        .ok()
        .and_then(|hs| hs.by_proxy.get(proxy_id).and_then(|v| v.last().cloned()))
}

pub fn snapshot_has_geo(snap: &TestSnapshot) -> bool {
    !snap.ip.is_empty()
        && !snap.country_code.is_empty()
        && !snap.timezone.is_empty()
        && snap.latitude != 0.0
        && snap.longitude != 0.0
}

pub fn latest_matching_test(entry: &ProxyEntry) -> Option<TestSnapshot> {
    if !cache_key_matches(entry) {
        return None;
    }
    latest_test(&entry.id).filter(snapshot_has_geo)
}

pub async fn ensure_cached_geo(entry: &ProxyEntry) -> Result<TestSnapshot> {
    if let Some(snapshot) = latest_matching_test(entry) {
        return Ok(snapshot);
    }
    let snapshot = full_test(entry).await?;
    if !snapshot_has_geo(&snapshot) {
        anyhow::bail!(
            "proxy test did not return complete GeoIP data; verify the proxy and geo checker"
        );
    }
    Ok(snapshot)
}

pub async fn ensure_ixbrowser_webrtc(entry: &ProxyEntry) -> Result<TestSnapshot> {
    let snapshot = ensure_cached_geo(entry).await?;
    ensure_host_public_ips().await?;
    Ok(snapshot)
}

fn unix_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{s}")
}

/// Run TCP + UDP + geo, persist into history, auto-fill country tag.
pub async fn full_test(entry: &ProxyEntry) -> Result<TestSnapshot> {
    let now = unix_now();

    let _ = ensure_host_public_ips().await;
    let tcp_res = probe(entry).await;
    let udp_res = if matches!(entry.kind, ProxyKind::Socks5) {
        Some(probe_udp(entry).await)
    } else {
        None
    };
    let geo_res = geo_check(entry, None).await;

    // TCP failure → zero geo so snapshot reads "Failed, no IP".
    let tcp_failed = tcp_res.is_err();
    let (ip, country_code, country, region, city, isp, tz, lat, lng, provider) =
        match (&geo_res, tcp_failed) {
            (Ok(g), false) => (
                g.ip.clone(), g.country_code.clone(), g.country.clone(),
                g.region.clone(), g.city.clone(), g.isp.clone(),
                g.timezone.clone(), g.latitude, g.longitude, g.provider.clone(),
            ),
            _ => (String::new(), String::new(), String::new(),
                  String::new(), String::new(), String::new(),
                  String::new(), 0.0, 0.0, String::new()),
        };

    let snap = TestSnapshot {
        first_seen: String::new(),
        last_seen: now,
        ip,
        country_code,
        country,
        region,
        city,
        isp,
        timezone: tz,
        latitude: lat,
        longitude: lng,
        tcp_ms: tcp_res.ok(),
        udp_ms: udp_res
            .as_ref()
            .and_then(|r| r.as_ref().ok().copied()),
        udp_error: udp_res
            .as_ref()
            .and_then(|r| r.as_ref().err().map(|e| e.to_string())),
        provider,
    };

    let recorded = record_test(&entry.id, snap)?;
    if snapshot_has_geo(&recorded) {
        record_cache_key(entry)?;
    } else {
        clear_cache_key(&entry.id)?;
    }

    // Backfill empty country tag on the stored entry.
    if !recorded.country_code.is_empty() {
        let mut store_data = load()?;
        if let Some(p) = store_data.proxies.iter_mut().find(|p| p.id == entry.id) {
            if p.country.is_empty() || p.country == "—" {
                p.country = recorded.country_code.clone();
                save(&store_data)?;
            }
        }
    }

    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> ProxyEntry {
        ProxyEntry {
            id: "proxy-1".into(),
            name: "Detroit".into(),
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1080,
            username: "user".into(),
            password: "secret".into(),
            country: "US".into(),
            notes: "note".into(),
        }
    }

    #[test]
    fn connection_signature_ignores_display_metadata() {
        let original = entry();
        let mut renamed = original.clone();
        renamed.name = "Other label".into();
        renamed.notes = "Other note".into();
        renamed.country = "CA".into();

        assert_eq!(
            original.connection_signature(),
            renamed.connection_signature()
        );
    }

    #[test]
    fn connection_signature_changes_with_effective_connection() {
        let original = entry();
        let mut changed = original.clone();
        changed.host = "127.0.0.2".into();
        assert_ne!(
            original.connection_signature(),
            changed.connection_signature()
        );

        changed = original.clone();
        changed.password = "new-secret".into();
        assert_ne!(
            original.connection_signature(),
            changed.connection_signature()
        );
    }

    #[test]
    fn ixbrowser_geo_parses_string_coordinates() {
        let body = serde_json::json!({
            "status": "success",
            "country": "ID",
            "countryCode": "ID",
            "region": "Jakarta",
            "regionName": "Jakarta",
            "city": "Jakarta",
            "lat": "-6.2146",
            "lon": "106.8451",
            "timezone": "Asia/Jakarta",
            "query": "93.185.162.133"
        });

        let geo = parse_ixbrowser_geo(&body, "ixbrowser.com".into()).unwrap();
        assert_eq!(geo.ip, "93.185.162.133");
        assert_eq!(geo.country_code, "ID");
        assert_eq!(geo.region, "Jakarta");
        assert_eq!(geo.city, "Jakarta");
        assert_eq!(geo.timezone, "Asia/Jakarta");
        assert_eq!(geo.latitude, -6.2146);
        assert_eq!(geo.longitude, 106.8451);
    }

    #[test]
    fn ixbrowser_geo_accepts_numeric_coordinates() {
        let body = serde_json::json!({
            "status": "success",
            "lat": -6.2146,
            "lon": 106.8451
        });

        let geo = parse_ixbrowser_geo(&body, "ixbrowser.com".into()).unwrap();
        assert_eq!(geo.latitude, -6.2146);
        assert_eq!(geo.longitude, 106.8451);
    }
}

/// Fallback country → IANA timezone for providers that omit timezone.
pub fn country_to_timezone(cc: &str) -> &'static str {
    match cc.to_ascii_uppercase().as_str() {
        "US" => "America/New_York",
        "CA" => "America/Toronto",
        "GB" | "UK" => "Europe/London",
        "DE" => "Europe/Berlin",
        "FR" => "Europe/Paris",
        "ES" => "Europe/Madrid",
        "IT" => "Europe/Rome",
        "NL" => "Europe/Amsterdam",
        "PL" => "Europe/Warsaw",
        "PT" => "Europe/Lisbon",
        "RO" => "Europe/Bucharest",
        "RU" => "Europe/Moscow",
        "UA" => "Europe/Kyiv",
        "TR" => "Europe/Istanbul",
        "GR" => "Europe/Athens",
        "CZ" => "Europe/Prague",
        "HU" => "Europe/Budapest",
        "SE" => "Europe/Stockholm",
        "FI" => "Europe/Helsinki",
        "NO" => "Europe/Oslo",
        "DK" => "Europe/Copenhagen",
        "CH" => "Europe/Zurich",
        "AT" => "Europe/Vienna",
        "BR" => "America/Sao_Paulo",
        "AR" => "America/Argentina/Buenos_Aires",
        "MX" => "America/Mexico_City",
        "AU" => "Australia/Sydney",
        "NZ" => "Pacific/Auckland",
        "IN" => "Asia/Kolkata",
        "ID" => "Asia/Jakarta",
        "MY" => "Asia/Kuala_Lumpur",
        "SG" => "Asia/Singapore",
        "TH" => "Asia/Bangkok",
        "VN" => "Asia/Ho_Chi_Minh",
        "CN" => "Asia/Shanghai",
        "HK" => "Asia/Hong_Kong",
        "TW" => "Asia/Taipei",
        "JP" => "Asia/Tokyo",
        "KR" => "Asia/Seoul",
        "IL" => "Asia/Jerusalem",
        "SA" => "Asia/Riyadh",
        "AE" => "Asia/Dubai",
        _ => "UTC",
    }
}

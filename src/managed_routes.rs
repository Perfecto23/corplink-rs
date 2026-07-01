use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use reqwest::header;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::config::{Config, ManagedRouteSource, ManagedRoutesConfig};

const DEFAULT_CACHE_FILE: &str = ".run/managed-routes-cache.json";
const DEFAULT_STALE_TTL_SECS: i64 = 86_400;
const DEFAULT_GITHUB_META_URL: &str = "https://api.github.com/meta";
const DEFAULT_GITHUB_KEYS: &[&str] = &["web", "api", "git"];
const DEFAULT_DOH_URL: &str = "https://cloudflare-dns.com/dns-query";
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ManagedRouteCache {
    version: u8,
    sources: Vec<SourceCacheEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SourceCacheEntry {
    name: String,
    source_type: String,
    source_fingerprint: Option<String>,
    routes: Vec<String>,
    resolved_at: i64,
    error: Option<String>,
}

pub async fn resolve_managed_routes(conf: &Config) -> Result<Vec<String>> {
    let Some(managed_routes) = conf.managed_routes.as_ref() else {
        return Ok(Vec::new());
    };
    if !managed_routes.enabled.unwrap_or(true) {
        return Ok(Vec::new());
    }

    let sources = managed_routes
        .sources
        .as_deref()
        .context("managed_routes.enabled is true but sources is missing")?;
    if sources.is_empty() {
        bail!("managed_routes.enabled is true but sources is empty");
    }

    if managed_routes.refresh_interval_secs.is_some() {
        log::info!(
            "managed_routes refresh_interval_secs is reserved; routes are resolved on startup"
        );
    }

    let include_ipv6 = managed_routes.include_ipv6.unwrap_or(false);
    let stale_ttl_secs = managed_routes
        .stale_ttl_secs
        .unwrap_or(DEFAULT_STALE_TTL_SECS);
    let cache_path = resolve_cache_path(conf, managed_routes);
    let mut cache = ManagedRouteCache::load(&cache_path).await;
    let now = unix_now_secs();
    let mut routes = Vec::new();

    for source in sources {
        validate_source_name(source.name())?;
        let source_type = source.source_type();
        let source_fingerprint = source_fingerprint(source, include_ipv6)?;
        match resolve_source(source, include_ipv6).await {
            Ok(source_routes) => {
                let source_routes = normalize_source_routes(source.name(), &source_routes)?;
                log::info!(
                    "managed_routes source {} ({}) resolved {} routes",
                    source.name(),
                    source_type,
                    source_routes.len()
                );
                cache.upsert(SourceCacheEntry {
                    name: source.name().to_string(),
                    source_type: source_type.to_string(),
                    source_fingerprint: Some(source_fingerprint.clone()),
                    routes: source_routes.clone(),
                    resolved_at: now,
                    error: None,
                });
                routes.extend(source_routes);
            }
            Err(err) => match cache.fresh_entry(source, &source_fingerprint, now, stale_ttl_secs) {
                Some(entry) => {
                    let age = now.saturating_sub(entry.resolved_at);
                    log::warn!(
                        "managed_routes source {} ({}) failed: {:#}; using {} cached routes (age {}s)",
                        source.name(),
                        source_type,
                        err,
                        entry.routes.len(),
                        age
                    );
                    routes.extend(entry.routes.clone());
                }
                None => {
                    bail!(
                        "managed_routes source {} ({}) failed and no fresh cache is available: {:#}",
                        source.name(),
                        source_type,
                        err
                    );
                }
            },
        }
    }

    if let Err(err) = cache.save(&cache_path).await {
        log::warn!(
            "failed to save managed_routes cache {}: {:#}",
            cache_path.display(),
            err
        );
    }

    dedupe_routes(&mut routes);
    log::info!("managed_routes resolved {} total routes", routes.len());
    Ok(routes)
}

async fn resolve_source(source: &ManagedRouteSource, include_ipv6: bool) -> Result<Vec<String>> {
    match source {
        ManagedRouteSource::GithubMeta { keys, meta_url, .. } => {
            let keys = keys
                .clone()
                .unwrap_or_else(|| DEFAULT_GITHUB_KEYS.iter().map(|s| s.to_string()).collect());
            resolve_github_meta(
                meta_url.as_deref().unwrap_or(DEFAULT_GITHUB_META_URL),
                &keys,
                include_ipv6,
            )
            .await
        }
        ManagedRouteSource::DnsHosts { hosts, .. } => resolve_dns_hosts(hosts, include_ipv6).await,
    }
}

async fn resolve_github_meta(
    meta_url: &str,
    keys: &[String],
    include_ipv6: bool,
) -> Result<Vec<String>> {
    if keys.is_empty() {
        bail!("github_meta keys is empty");
    }

    let client = managed_routes_http_client()?;
    let meta = client
        .get(meta_url)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header(header::USER_AGENT, "corplink-rs-managed-routes")
        .send()
        .await
        .with_context(|| format!("failed to fetch GitHub Meta API {meta_url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub Meta API returned error for {meta_url}"))?
        .json::<Value>()
        .await
        .context("failed to parse GitHub Meta API response")?;

    collect_github_meta_routes(&meta, keys, include_ipv6)
}

fn collect_github_meta_routes(
    meta: &Value,
    keys: &[String],
    include_ipv6: bool,
) -> Result<Vec<String>> {
    let mut routes = Vec::new();
    for key in keys {
        let values = meta
            .get(key)
            .and_then(Value::as_array)
            .with_context(|| format!("GitHub Meta API response missing list field {key:?}"))?;
        for value in values {
            let route = value.as_str().with_context(|| {
                format!("GitHub Meta API field {key:?} contains non-string route")
            })?;
            if !include_ipv6 && route.contains(':') {
                continue;
            }
            routes.push(route.to_string());
        }
    }
    Ok(routes)
}

async fn resolve_dns_hosts(hosts: &[String], include_ipv6: bool) -> Result<Vec<String>> {
    if hosts.is_empty() {
        bail!("dns_hosts hosts is empty");
    }

    let client = managed_routes_http_client()?;
    let mut routes = Vec::new();
    for host in hosts {
        let host = host.trim();
        if host.is_empty() {
            bail!("dns_hosts contains empty host");
        }

        let host_routes = resolve_dns_host_doh(&client, host, include_ipv6).await?;
        if host_routes.is_empty() {
            bail!("DNS host {host:?} resolved no usable addresses");
        }
        log::info!(
            "managed_routes DNS host {} resolved {} routes",
            host,
            host_routes.len()
        );
        routes.extend(host_routes);
    }
    Ok(routes)
}

fn managed_routes_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to build managed_routes http client")
}

async fn resolve_dns_host_doh(
    client: &reqwest::Client,
    host: &str,
    include_ipv6: bool,
) -> Result<Vec<String>> {
    let mut ips = Vec::new();
    ips.extend(resolve_doh_record(client, host, "A").await?);
    if include_ipv6 {
        ips.extend(resolve_doh_record(client, host, "AAAA").await?);
    }
    Ok(routes_from_ips(ips, include_ipv6))
}

async fn resolve_doh_record(
    client: &reqwest::Client,
    host: &str,
    record_type: &str,
) -> Result<Vec<IpAddr>> {
    let response = client
        .get(DEFAULT_DOH_URL)
        .query(&[("name", host), ("type", record_type)])
        .header(header::ACCEPT, "application/dns-json")
        .header(header::USER_AGENT, "corplink-rs-managed-routes")
        .send()
        .await
        .with_context(|| format!("failed to query DoH for {host:?} {record_type}"))?
        .error_for_status()
        .with_context(|| format!("DoH returned error for {host:?} {record_type}"))?
        .json::<Value>()
        .await
        .with_context(|| format!("failed to parse DoH response for {host:?} {record_type}"))?;

    collect_doh_ips(&response, record_type)
}

fn collect_doh_ips(response: &Value, record_type: &str) -> Result<Vec<IpAddr>> {
    let expected_type = match record_type {
        "A" => 1,
        "AAAA" => 28,
        _ => bail!("unsupported DNS record type {record_type:?}"),
    };
    let mut ips = Vec::new();
    let Some(answers) = response.get("Answer").and_then(Value::as_array) else {
        return Ok(ips);
    };
    for answer in answers {
        if answer.get("type").and_then(Value::as_i64) != Some(expected_type) {
            continue;
        }
        let Some(data) = answer.get("data").and_then(Value::as_str) else {
            continue;
        };
        let ip: IpAddr = data
            .parse()
            .with_context(|| format!("DoH {record_type} answer contains invalid IP {data:?}"))?;
        ips.push(ip);
    }
    Ok(ips)
}

fn routes_from_ips<I>(ips: I, include_ipv6: bool) -> Vec<String>
where
    I: IntoIterator<Item = IpAddr>,
{
    let mut routes = Vec::new();
    for ip in ips {
        if is_fake_ip(ip) {
            log::warn!("skip fake DNS IP {} from managed_routes dns_hosts", ip);
            continue;
        }
        match ip {
            IpAddr::V4(ip) => routes.push(format!("{ip}/32")),
            IpAddr::V6(ip) if include_ipv6 => routes.push(format!("{ip}/128")),
            IpAddr::V6(_) => {}
        }
    }
    dedupe_routes(&mut routes);
    routes
}

fn is_fake_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let raw = u32::from(ip);
            let start = u32::from(std::net::Ipv4Addr::new(198, 18, 0, 0));
            let end = u32::from(std::net::Ipv4Addr::new(198, 19, 255, 255));
            (start..=end).contains(&raw)
        }
        IpAddr::V6(_) => false,
    }
}

fn normalize_source_routes(source_name: &str, routes: &[String]) -> Result<Vec<String>> {
    if routes.is_empty() {
        bail!("managed_routes source {source_name:?} returned no routes");
    }

    let mut normalized = Vec::with_capacity(routes.len());
    for route in routes {
        normalized.push(crate::utils::normalize_route(route).with_context(|| {
            format!("managed_routes source {source_name:?} has invalid route {route:?}")
        })?);
    }
    dedupe_routes(&mut normalized);
    Ok(normalized)
}

fn dedupe_routes(routes: &mut Vec<String>) {
    let mut seen = HashSet::with_capacity(routes.len());
    routes.retain(|route| seen.insert(route.clone()));
}

fn validate_source_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("managed_routes source name is empty");
    }
    Ok(())
}

fn source_fingerprint(source: &ManagedRouteSource, include_ipv6: bool) -> Result<String> {
    let material = match source {
        ManagedRouteSource::GithubMeta {
            name,
            keys,
            meta_url,
        } => {
            let keys = keys
                .clone()
                .unwrap_or_else(|| DEFAULT_GITHUB_KEYS.iter().map(|s| s.to_string()).collect());
            json!([
                "v1",
                "github_meta",
                name,
                meta_url.as_deref().unwrap_or(DEFAULT_GITHUB_META_URL),
                keys,
                include_ipv6
            ])
        }
        ManagedRouteSource::DnsHosts {
            name, hosts, port, ..
        } => json!(["v1", "dns_hosts", name, hosts, port, include_ipv6]),
    };
    let material =
        serde_json::to_vec(&material).context("failed to serialize managed_routes fingerprint")?;
    Ok(format!("{:x}", Sha256::digest(material)))
}

fn resolve_cache_path(conf: &Config, managed_routes: &ManagedRoutesConfig) -> PathBuf {
    let cache_file = managed_routes
        .cache_file
        .as_deref()
        .unwrap_or(DEFAULT_CACHE_FILE);
    let path = PathBuf::from(cache_file);
    if path.is_absolute() {
        return path;
    }

    match conf
        .conf_file
        .as_deref()
        .and_then(|file| Path::new(file).parent())
    {
        Some(parent) => parent.join(path),
        None => path,
    }
}

impl ManagedRouteCache {
    async fn load(path: &Path) -> ManagedRouteCache {
        match fs::read_to_string(path).await {
            Ok(data) => match serde_json::from_str::<ManagedRouteCache>(&data) {
                Ok(mut cache) => {
                    if cache.version == 0 {
                        cache.version = 1;
                    }
                    cache
                }
                Err(err) => {
                    log::warn!(
                        "failed to parse managed_routes cache {}: {}",
                        path.display(),
                        err
                    );
                    ManagedRouteCache {
                        version: 1,
                        sources: Vec::new(),
                    }
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ManagedRouteCache {
                version: 1,
                sources: Vec::new(),
            },
            Err(err) => {
                log::warn!(
                    "failed to read managed_routes cache {}: {}",
                    path.display(),
                    err
                );
                ManagedRouteCache {
                    version: 1,
                    sources: Vec::new(),
                }
            }
        }
    }

    async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
        }

        let file_name = path
            .file_name()
            .context("managed_routes cache path missing filename")?
            .to_string_lossy();
        let tmp_path = path.with_file_name(format!("{file_name}.tmp"));
        let data = serde_json::to_string_pretty(self).context("failed to serialize cache")? + "\n";
        fs::write(&tmp_path, data)
            .await
            .with_context(|| format!("failed to write cache temp file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .await
            .with_context(|| format!("failed to replace cache file {}", path.display()))?;
        Ok(())
    }

    fn upsert(&mut self, entry: SourceCacheEntry) {
        match self
            .sources
            .iter_mut()
            .find(|cached| cached.name == entry.name && cached.source_type == entry.source_type)
        {
            Some(cached) => *cached = entry,
            None => self.sources.push(entry),
        }
    }

    fn fresh_entry(
        &self,
        source: &ManagedRouteSource,
        source_fingerprint: &str,
        now: i64,
        stale_ttl_secs: i64,
    ) -> Option<&SourceCacheEntry> {
        self.sources.iter().find(|entry| {
            entry.name == source.name()
                && entry.source_type == source.source_type()
                && entry.source_fingerprint.as_deref() == Some(source_fingerprint)
                && !entry.routes.is_empty()
                && now.saturating_sub(entry.resolved_at) <= stale_ttl_secs
        })
    }
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().try_into().unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn github_meta_routes_filter_ipv6_by_default() {
        let meta = json!({
            "web": ["140.82.112.0/20", "2606:50c0::/32"],
            "api": ["20.205.243.166/32"],
        });
        let keys = vec!["web".to_string(), "api".to_string()];

        let routes = collect_github_meta_routes(&meta, &keys, false).unwrap();

        assert_eq!(routes, vec!["140.82.112.0/20", "20.205.243.166/32"]);
    }

    #[test]
    fn dns_routes_filter_ipv6_by_default() {
        let ips = vec![
            "20.205.243.166".parse::<IpAddr>().unwrap(),
            "2001:db8::1".parse::<IpAddr>().unwrap(),
        ];

        let routes = routes_from_ips(ips, false);

        assert_eq!(routes, vec!["20.205.243.166/32"]);
    }

    #[test]
    fn doh_collector_keeps_only_requested_record_type() {
        let response = json!({
            "Answer": [
                { "type": 5, "data": "example.redshift.amazonaws.com" },
                { "type": 1, "data": "54.240.1.10" },
                { "type": 28, "data": "2001:db8::1" }
            ]
        });

        let ips = collect_doh_ips(&response, "A").unwrap();

        assert_eq!(ips, vec!["54.240.1.10".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn dns_routes_skip_fake_ip_range() {
        let ips = vec![
            "198.18.38.107".parse::<IpAddr>().unwrap(),
            "54.240.1.10".parse::<IpAddr>().unwrap(),
        ];

        let routes = routes_from_ips(ips, false);

        assert_eq!(routes, vec!["54.240.1.10/32"]);
    }

    #[test]
    fn cache_entry_expires_after_stale_ttl() {
        let source = ManagedRouteSource::DnsHosts {
            name: "redshift-prod".to_string(),
            hosts: vec!["example.redshift.amazonaws.com".to_string()],
            port: Some(5439),
        };
        let fingerprint = source_fingerprint(&source, false).unwrap();
        let cache = ManagedRouteCache {
            version: 1,
            sources: vec![SourceCacheEntry {
                name: "redshift-prod".to_string(),
                source_type: "dns_hosts".to_string(),
                source_fingerprint: Some(fingerprint.clone()),
                routes: vec!["20.205.243.166/32".to_string()],
                resolved_at: 100,
                error: None,
            }],
        };

        assert!(cache.fresh_entry(&source, &fingerprint, 200, 101).is_some());
        assert!(cache.fresh_entry(&source, &fingerprint, 202, 101).is_none());
    }

    #[test]
    fn cache_entry_rejects_changed_source_inputs() {
        let old_source = ManagedRouteSource::DnsHosts {
            name: "redshift-prod".to_string(),
            hosts: vec!["old.example.redshift.amazonaws.com".to_string()],
            port: Some(5439),
        };
        let new_source = ManagedRouteSource::DnsHosts {
            name: "redshift-prod".to_string(),
            hosts: vec!["new.example.redshift.amazonaws.com".to_string()],
            port: Some(5439),
        };
        let cache = ManagedRouteCache {
            version: 1,
            sources: vec![SourceCacheEntry {
                name: "redshift-prod".to_string(),
                source_type: "dns_hosts".to_string(),
                source_fingerprint: Some(source_fingerprint(&old_source, false).unwrap()),
                routes: vec!["20.205.243.166/32".to_string()],
                resolved_at: 100,
                error: None,
            }],
        };
        let new_fingerprint = source_fingerprint(&new_source, false).unwrap();

        assert!(cache
            .fresh_entry(&new_source, &new_fingerprint, 110, 101)
            .is_none());
    }

    #[test]
    fn cache_entry_rejects_changed_ipv6_setting() {
        let source = ManagedRouteSource::GithubMeta {
            name: "github".to_string(),
            keys: Some(vec!["web".to_string()]),
            meta_url: None,
        };
        let cache = ManagedRouteCache {
            version: 1,
            sources: vec![SourceCacheEntry {
                name: "github".to_string(),
                source_type: "github_meta".to_string(),
                source_fingerprint: Some(source_fingerprint(&source, false).unwrap()),
                routes: vec!["140.82.112.0/20".to_string()],
                resolved_at: 100,
                error: None,
            }],
        };
        let ipv6_fingerprint = source_fingerprint(&source, true).unwrap();

        assert!(cache
            .fresh_entry(&source, &ipv6_fingerprint, 110, 101)
            .is_none());
    }

    #[test]
    fn config_parses_managed_routes_sources() {
        let config: Config = serde_json::from_value(json!({
            "company_name": "company",
            "username": "user",
            "managed_routes": {
                "enabled": true,
                "sources": [
                    { "name": "github", "type": "github_meta", "keys": ["web", "api", "git"] },
                    { "name": "redshift-prod", "type": "dns_hosts", "hosts": ["example.redshift.amazonaws.com"], "port": 5439 }
                ]
            }
        }))
        .unwrap();

        let sources = config.managed_routes.unwrap().sources.unwrap();
        assert_eq!(sources[0].name(), "github");
        assert_eq!(sources[1].source_type(), "dns_hosts");
    }
}

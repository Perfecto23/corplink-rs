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

use crate::api::{ClientFailure, FailureKind};
use crate::config::{Config, ManagedRouteSource, ManagedRoutesConfig};

const DEFAULT_CACHE_FILE: &str = ".run/managed-routes-cache.json";
const DEFAULT_STALE_TTL_SECS: i64 = 86_400;
const DEFAULT_GITHUB_META_URL: &str = "https://api.github.com/meta";
const DEFAULT_GITHUB_KEYS: &[&str] = &["web", "api", "git"];
const DEFAULT_DOH_URL: &str = "https://cloudflare-dns.com/dns-query";
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteFailureKind {
    RecoverableTransport,
    RateLimited,
    Server,
    Configuration,
    Protocol,
}

impl RouteFailureKind {
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RecoverableTransport | Self::RateLimited | Self::Server
        )
    }
}

#[derive(Debug)]
pub struct ManagedRouteFailure {
    kind: RouteFailureKind,
    source: String,
}

impl ManagedRouteFailure {
    pub fn kind(&self) -> RouteFailureKind {
        self.kind
    }
}

impl std::fmt::Display for ManagedRouteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "managed route {} failed for source {}",
            self.kind, self.source
        )
    }
}

impl std::error::Error for ManagedRouteFailure {}

impl std::fmt::Display for RouteFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RecoverableTransport => "transport",
            Self::RateLimited => "rate_limited",
            Self::Server => "server",
            Self::Configuration => "configuration",
            Self::Protocol => "protocol",
        })
    }
}

pub fn classify_route_error(error: &anyhow::Error) -> RouteFailureKind {
    if let Some(failure) = error.downcast_ref::<ManagedRouteFailure>() {
        return failure.kind();
    }
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = error.status() {
                return match status.as_u16() {
                    429 => RouteFailureKind::RateLimited,
                    500..=599 => RouteFailureKind::Server,
                    _ => RouteFailureKind::Protocol,
                };
            }
            if error.is_connect() || error.is_timeout() || error.is_request() {
                return RouteFailureKind::RecoverableTransport;
            }
        }
    }
    RouteFailureKind::Protocol
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteSourceStatus {
    Fresh,
    Cache,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RouteSourceReport {
    pub name: String,
    pub source_type: String,
    pub status: RouteSourceStatus,
    pub routes: Vec<String>,
    pub resolved_at: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RouteResolutionReport {
    pub routes: Vec<String>,
    pub sources: Vec<RouteSourceReport>,
    pub resolved_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManagedRoutesApplied {
    pub config_identity: String,
    pub generation: String,
    pub pid: u32,
    pub applied_at: i64,
    pub mode: AppliedRouteMode,
    pub resolution_routes: Vec<String>,
    pub sources: Vec<RouteSourceReport>,
    pub allowed_ips: Vec<String>,
    pub routes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppliedRouteMode {
    Kernel,
    Netstack,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RouteStatusResponse {
    pub last_applied: Option<ManagedRoutesApplied>,
}

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
    Ok(resolve_managed_routes_report(conf, true).await?.routes)
}

pub async fn resolve_managed_routes_report(
    conf: &Config,
    write_cache: bool,
) -> Result<RouteResolutionReport> {
    let Some(managed_routes) = conf.managed_routes.as_ref() else {
        return Ok(RouteResolutionReport {
            resolved_at: unix_now_secs(),
            ..RouteResolutionReport::default()
        });
    };
    if !managed_routes.enabled.unwrap_or(true) {
        return Ok(RouteResolutionReport {
            resolved_at: unix_now_secs(),
            ..RouteResolutionReport::default()
        });
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
    let mut source_reports = Vec::with_capacity(sources.len());

    for source in sources {
        validate_source_name(source.name())?;
        let source_type = source.source_type();
        let source_fingerprint = source_fingerprint(source, include_ipv6)?;
        let source_result = match resolve_source(source, include_ipv6).await {
            Ok(source_routes) => normalize_source_routes(source.name(), &source_routes),
            Err(error) => Err(error),
        };
        match source_result {
            Ok(source_routes) => {
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
                source_reports.push(RouteSourceReport {
                    name: source.name().to_string(),
                    source_type: source_type.to_string(),
                    status: RouteSourceStatus::Fresh,
                    routes: cache
                        .sources
                        .iter()
                        .find(|entry| {
                            entry.name == source.name() && entry.source_type == source_type
                        })
                        .map(|entry| entry.routes.clone())
                        .unwrap_or_default(),
                    resolved_at: now,
                    error: None,
                });
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
                    source_reports.push(RouteSourceReport {
                        name: source.name().to_string(),
                        source_type: source_type.to_string(),
                        status: RouteSourceStatus::Cache,
                        routes: entry.routes.clone(),
                        resolved_at: entry.resolved_at,
                        error: Some(safe_route_error(&err)),
                    });
                }
                None => {
                    source_reports.push(RouteSourceReport {
                        name: source.name().to_string(),
                        source_type: source_type.to_string(),
                        status: RouteSourceStatus::Error,
                        routes: Vec::new(),
                        resolved_at: now,
                        error: Some(safe_route_error(&err)),
                    });
                    let operation = format!("managed_routes:{}", source.name());
                    let failure = match classify_route_error(&err) {
                        RouteFailureKind::RecoverableTransport => {
                            ClientFailure::transport(operation)
                        }
                        RouteFailureKind::RateLimited => {
                            ClientFailure::new(FailureKind::RateLimited, operation, None, None)
                        }
                        RouteFailureKind::Server => {
                            ClientFailure::new(FailureKind::Server, operation, None, None)
                        }
                        RouteFailureKind::Configuration => ClientFailure::configuration(operation),
                        RouteFailureKind::Protocol => ClientFailure::protocol(operation, None),
                    };
                    return Err(anyhow::Error::new(failure));
                }
            },
        }
    }

    if write_cache {
        if let Err(err) = cache.save(&cache_path).await {
            log::warn!(
                "failed to save managed_routes cache {}: {:#}",
                cache_path.display(),
                err
            );
        }
    }

    dedupe_routes(&mut routes);
    log::info!("managed_routes resolved {} total routes", routes.len());
    Ok(RouteResolutionReport {
        routes,
        sources: source_reports,
        resolved_at: now,
    })
}

fn safe_route_error(error: &anyhow::Error) -> String {
    error
        .chain()
        .next()
        .map(|cause| cause.to_string())
        .unwrap_or_else(|| "managed route source failed".to_string())
}

pub async fn mark_managed_routes_applied(
    conf: &Config,
    report: &RouteResolutionReport,
    wg_conf: &crate::config::WgConf,
    generation: &str,
    pid: u32,
) -> Result<()> {
    let applied = ManagedRoutesApplied {
        config_identity: conf.session_identity_tag(),
        generation: generation.to_string(),
        pid,
        applied_at: unix_now_secs(),
        mode: if conf.socks5_listen.is_some() {
            AppliedRouteMode::Netstack
        } else {
            AppliedRouteMode::Kernel
        },
        resolution_routes: report.routes.clone(),
        sources: report.sources.clone(),
        allowed_ips: wg_conf.allowed_ips.clone(),
        routes: if conf.socks5_listen.is_some() {
            Vec::new()
        } else {
            wg_conf.routes.clone()
        },
    };
    let path = resolve_status_path(conf);
    write_json_atomically(&path, &applied).await
}

pub async fn read_managed_routes_status(conf: &Config) -> Result<Option<ManagedRoutesApplied>> {
    let path = resolve_status_path(conf);
    match fs::read_to_string(&path).await {
        Ok(data) => {
            let status: ManagedRoutesApplied = serde_json::from_str(&data).with_context(|| {
                format!("failed to parse managed routes status {}", path.display())
            })?;
            if status.config_identity != conf.session_identity_tag() {
                return Ok(None);
            }
            Ok(Some(status))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read managed routes status {}", path.display())),
    }
}

pub async fn routes_command(config_file: &str, write_cache: bool) -> Result<String> {
    let config = Config::read_only(config_file).await?;
    let report = resolve_managed_routes_report(&config, write_cache).await?;
    Ok(serde_json::to_string_pretty(&report)? + "\n")
}

pub async fn routes_status_command(config_file: &str) -> Result<String> {
    let config = Config::read_only(config_file).await?;
    let response = RouteStatusResponse {
        last_applied: read_managed_routes_status(&config).await?,
    };
    Ok(serde_json::to_string_pretty(&response)? + "\n")
}

fn resolve_status_path(conf: &Config) -> PathBuf {
    let cache_path = conf
        .managed_routes
        .as_ref()
        .map(|managed| resolve_cache_path(conf, managed))
        .unwrap_or_else(|| {
            let base = conf
                .conf_file
                .as_deref()
                .and_then(|file| Path::new(file).parent())
                .unwrap_or_else(|| Path::new("."));
            base.join(DEFAULT_CACHE_FILE)
        });
    let stem = cache_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("managed-routes");
    cache_path.with_file_name(format!("{stem}.status.json"))
}

async fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await.with_context(|| {
        format!(
            "failed to create managed routes state dir {}",
            parent.display()
        )
    })?;
    let data = serde_json::to_string_pretty(value)? + "\n";
    let file_name = path
        .file_name()
        .context("managed routes state path missing filename")?
        .to_string_lossy();
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = async {
        fs::write(&temp, data)
            .await
            .with_context(|| format!("failed to write managed routes state {}", temp.display()))?;
        fs::rename(&temp, path).await.with_context(|| {
            format!("failed to publish managed routes state {}", path.display())
        })?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
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
        let tmp_path = path.with_file_name(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            unix_now_secs()
        ));
        let data = serde_json::to_string_pretty(self).context("failed to serialize cache")? + "\n";
        let result = async {
            fs::write(&tmp_path, data).await.with_context(|| {
                format!("failed to write cache temp file {}", tmp_path.display())
            })?;
            fs::rename(&tmp_path, path)
                .await
                .with_context(|| format!("failed to replace cache file {}", path.display()))?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&tmp_path).await;
        }
        result
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

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn accept_with_timeout(
        listener: &std::net::TcpListener,
    ) -> (std::net::TcpStream, std::net::SocketAddr) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok(connection) => {
                    connection.0.set_nonblocking(false).unwrap();
                    return connection;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for local HTTP fixture request"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("local HTTP fixture accept failed: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn disabled_routes_return_report_without_writing_cache() {
        let dir = unique_test_dir("corplink-managed-report");
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            br#"{"company_name":"company","username":"user","managed_routes":{"enabled":false,"cache_file":"cache.json"}}"#,
        )
        .await
        .unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();

        let report = resolve_managed_routes_report(&config, false).await.unwrap();

        assert!(report.routes.is_empty());
        assert!(report.sources.is_empty());
        assert!(!dir.join("cache.json").exists());
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn preflight_report_uses_local_gateway_without_writing_cache() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = accept_with_timeout(&listener);
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = br#"{"web":["192.0.2.0/24"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir = unique_test_dir("corplink-managed-preflight");
        let config_path = dir.join("config.json");
        let cache_path = dir.join("cache.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"managed_routes\":{{\"enabled\":true,\"cache_file\":\"{}\",\"sources\":[{{\"type\":\"github_meta\",\"name\":\"local\",\"keys\":[\"web\"],\"meta_url\":\"http://127.0.0.1:{port}/meta\"}}]}}}}",
            cache_path.display()
        );
        fs::write(&config_path, source).await.unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();

        let report = resolve_managed_routes_report(&config, false).await.unwrap();

        assert_eq!(report.routes, vec!["192.0.2.0/24"]);
        assert_eq!(report.sources[0].status, RouteSourceStatus::Fresh);
        assert!(!cache_path.exists());
        server.join().unwrap();
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn applied_status_is_separate_from_resolution_and_readable_without_network() {
        let dir = unique_test_dir("corplink-managed-status");
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            br#"{"company_name":"company","username":"user","managed_routes":{"enabled":false}}"#,
        )
        .await
        .unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let report = resolve_managed_routes_report(&config, false).await.unwrap();
        let wg_conf = crate::config::WgConf {
            address: "10.0.0.2/24".to_string(),
            address6: String::new(),
            peer_address: "192.0.2.1:51820".to_string(),
            mtu: 1420,
            public_key: "public".to_string(),
            private_key: "private".to_string(),
            peer_key: "peer".to_string(),
            allowed_ips: vec!["10.0.0.0/8".to_string()],
            routes: vec!["10.0.0.0/8".to_string()],
            dns: "10.0.0.53".to_string(),
            protocol: 0,
        };

        mark_managed_routes_applied(&config, &report, &wg_conf, "generation-test", 1234)
            .await
            .unwrap();
        let status = read_managed_routes_status(&config).await.unwrap().unwrap();

        assert_eq!(status.generation, "generation-test");
        assert_eq!(status.pid, 1234);
        assert_eq!(status.mode, AppliedRouteMode::Kernel);
        assert_eq!(status.config_identity, config.session_identity_tag());
        assert_eq!(status.allowed_ips, wg_conf.allowed_ips);
        assert_eq!(status.routes, wg_conf.routes);
        assert!(status.resolution_routes.is_empty());
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn applied_status_isolated_between_config_identities() {
        let dir = unique_test_dir("corplink-managed-status-isolation");
        let config_a_path = dir.join("a.json");
        let config_b_path = dir.join("b.json");
        fs::write(
            &config_a_path,
            br#"{"company_name":"company","username":"a","managed_routes":{"enabled":false,"cache_file":"a.json"}}"#,
        )
        .await
        .unwrap();
        fs::write(
            &config_b_path,
            br#"{"company_name":"company","username":"b","managed_routes":{"enabled":false,"cache_file":"b.json"}}"#,
        )
        .await
        .unwrap();
        let config_a = Config::read_only(config_a_path.to_str().unwrap())
            .await
            .unwrap();
        let config_b = Config::read_only(config_b_path.to_str().unwrap())
            .await
            .unwrap();
        let report = resolve_managed_routes_report(&config_a, false)
            .await
            .unwrap();
        let wg_conf = crate::config::WgConf {
            address: "10.0.0.2/24".to_string(),
            address6: String::new(),
            peer_address: "192.0.2.1:51820".to_string(),
            mtu: 1420,
            public_key: "public".to_string(),
            private_key: "private".to_string(),
            peer_key: "peer".to_string(),
            allowed_ips: vec!["10.0.0.0/8".to_string()],
            routes: vec!["10.0.0.0/8".to_string()],
            dns: "10.0.0.53".to_string(),
            protocol: 0,
        };

        mark_managed_routes_applied(&config_a, &report, &wg_conf, "generation-a", 1)
            .await
            .unwrap();

        assert!(read_managed_routes_status(&config_a)
            .await
            .unwrap()
            .is_some());
        assert!(read_managed_routes_status(&config_b)
            .await
            .unwrap()
            .is_none());
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn netstack_applied_status_has_no_system_routes_but_keeps_allowed_ips() {
        let dir = unique_test_dir("corplink-managed-netstack-status");
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            br#"{"company_name":"company","username":"user","socks5_listen":"127.0.0.1:1080","managed_routes":{"enabled":false}}"#,
        )
        .await
        .unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let report = resolve_managed_routes_report(&config, false).await.unwrap();
        let wg_conf = crate::config::WgConf {
            address: "10.0.0.2/24".to_string(),
            address6: String::new(),
            peer_address: "192.0.2.1:51820".to_string(),
            mtu: 1420,
            public_key: "public".to_string(),
            private_key: "private".to_string(),
            peer_key: "peer".to_string(),
            allowed_ips: vec!["10.0.0.0/8".to_string()],
            routes: vec!["10.0.0.0/8".to_string()],
            dns: "10.0.0.53".to_string(),
            protocol: 0,
        };

        mark_managed_routes_applied(&config, &report, &wg_conf, "generation-netstack", 2)
            .await
            .unwrap();
        let status = read_managed_routes_status(&config).await.unwrap().unwrap();

        assert_eq!(status.mode, AppliedRouteMode::Netstack);
        assert_eq!(status.allowed_ips, wg_conf.allowed_ips);
        assert!(status.routes.is_empty());
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn source_failure_uses_matching_fresh_cache_without_marking_it_fresh() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for (status, body) in [
                ("200 OK", br#"{"web":["192.0.2.0/24"]}"#.as_slice()),
                ("500 Internal Server Error", br#"invalid"#.as_slice()),
            ] {
                let (mut stream, _) = accept_with_timeout(&listener);
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        let dir = unique_test_dir("corplink-managed-fallback");
        let config_path = dir.join("config.json");
        let cache_path = dir.join("cache.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"managed_routes\":{{\"enabled\":true,\"cache_file\":\"{}\",\"stale_ttl_secs\":3600,\"sources\":[{{\"type\":\"github_meta\",\"name\":\"local\",\"keys\":[\"web\"],\"meta_url\":\"http://127.0.0.1:{port}/meta\"}}]}}}}",
            cache_path.display()
        );
        fs::write(&config_path, source).await.unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let first = resolve_managed_routes_report(&config, true).await.unwrap();
        let second = resolve_managed_routes_report(&config, false).await.unwrap();

        assert_eq!(first.sources[0].status, RouteSourceStatus::Fresh);
        assert_eq!(second.routes, first.routes);
        assert_eq!(second.sources[0].status, RouteSourceStatus::Cache);
        assert!(second.sources[0].error.is_some());
        server.join().unwrap();
        fs::remove_dir_all(dir).await.unwrap();
    }

    async fn run_cached_payload_fallback(payload: &'static [u8]) -> (RouteResolutionReport, i64) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for body in [br#"{"web":["192.0.2.0/24"]}"#.as_slice(), payload] {
                let (mut stream, _) = accept_with_timeout(&listener);
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let dir = unique_test_dir("corplink-managed-payload");
        let config_path = dir.join("config.json");
        let cache_path = dir.join("cache.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"managed_routes\":{{\"enabled\":true,\"cache_file\":\"{}\",\"stale_ttl_secs\":3600,\"sources\":[{{\"type\":\"github_meta\",\"name\":\"local\",\"keys\":[\"web\"],\"meta_url\":\"http://127.0.0.1:{port}/meta\"}}]}}}}",
            cache_path.display()
        );
        fs::write(&config_path, source).await.unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let first = resolve_managed_routes_report(&config, true).await.unwrap();
        let first_resolved_at = first.sources[0].resolved_at;
        let report = resolve_managed_routes_report(&config, false).await.unwrap();
        server.join().unwrap();
        fs::remove_dir_all(dir).await.unwrap();
        (report, first_resolved_at)
    }

    #[tokio::test]
    async fn http_200_empty_source_payload_uses_matching_cache() {
        let (report, first_resolved_at) = run_cached_payload_fallback(br#"{"web":[]}"#).await;

        assert_eq!(report.routes, vec!["192.0.2.0/24"]);
        assert_eq!(report.sources[0].status, RouteSourceStatus::Cache);
        assert_eq!(report.sources[0].resolved_at, first_resolved_at);
    }

    #[tokio::test]
    async fn invalid_cidr_source_payload_uses_matching_cache_without_refresh() {
        let (report, first_resolved_at) =
            run_cached_payload_fallback(br#"{"web":["not-a-cidr"]}"#).await;

        assert_eq!(report.routes, vec!["192.0.2.0/24"]);
        assert_eq!(report.sources[0].status, RouteSourceStatus::Cache);
        assert_eq!(report.sources[0].resolved_at, first_resolved_at);
        assert!(report.sources[0].error.is_some());
    }

    #[tokio::test]
    async fn cold_http_429_is_exposed_as_rate_limited_failure() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = accept_with_timeout(&listener);
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let dir = unique_test_dir("corplink-managed-429");
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"managed_routes\":{{\"enabled\":true,\"cache_file\":\"{}\",\"sources\":[{{\"type\":\"github_meta\",\"name\":\"local\",\"keys\":[\"web\"],\"meta_url\":\"http://127.0.0.1:{port}/meta\"}}]}}}}",
            dir.join("cache.json").display()
        );
        fs::write(&config_path, source).await.unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let error = resolve_managed_routes_report(&config, false)
            .await
            .expect_err("cold 429 should fail");

        assert_eq!(
            crate::api::classify_error(&error),
            crate::api::FailureKind::RateLimited
        );
        server.join().unwrap();
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn cold_http_503_is_exposed_as_server_failure() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = accept_with_timeout(&listener);
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let dir = unique_test_dir("corplink-managed-503");
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"managed_routes\":{{\"enabled\":true,\"cache_file\":\"{}\",\"sources\":[{{\"type\":\"github_meta\",\"name\":\"local\",\"keys\":[\"web\"],\"meta_url\":\"http://127.0.0.1:{port}/meta\"}}]}}}}",
            dir.join("cache.json").display()
        );
        fs::write(&config_path, source).await.unwrap();
        let config = Config::read_only(config_path.to_str().unwrap())
            .await
            .unwrap();
        let error = resolve_managed_routes_report(&config, false)
            .await
            .expect_err("cold 503 should fail");

        assert_eq!(
            crate::api::classify_error(&error),
            crate::api::FailureKind::Server
        );
        server.join().unwrap();
        fs::remove_dir_all(dir).await.unwrap();
    }

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

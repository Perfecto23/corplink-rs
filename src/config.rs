use std::fmt;
use std::path::PathBuf;
use tokio::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state::{self, SessionState, State};
use crate::utils;

const DEFAULT_DEVICE_NAME: &str = "DollarOS";
const DEFAULT_INTERFACE_NAME: &str = "corplink";

pub const PLATFORM_LDAP: &str = "ldap";
pub const PLATFORM_CORPLINK: &str = "feilian";
// new feilian login that uses the v1 API (/api/v1/login with an AES-encrypted
// password), as served by the newer feilian backend. opt-in via config.
pub const PLATFORM_CORPLINK_V1: &str = "feilian_v1";
pub const PLATFORM_OIDC: &str = "OIDC";
// aka feishu
pub const PLATFORM_LARK: &str = "lark";
#[allow(dead_code)]
pub const PLATFORM_WEIXIN: &str = "weixin";
// aka dingding
#[allow(dead_code)]
pub const PLATFORM_DING_TALK: &str = "dingtalk";
// unknown
#[allow(dead_code)]
pub const PLATFORM_AAD: &str = "aad";

pub const STRATEGY_LATENCY: &str = "latency";
pub const STRATEGY_DEFAULT: &str = "default";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RouteMode {
    /// Only intranet routes returned by the server (mimics official split mode).
    #[default]
    Split,
    /// Full-tunnel routes from the server (typically 0.0.0.0/0, ::/0).
    Full,
}

impl fmt::Display for RouteMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RouteMode::Split => write!(f, "split"),
            RouteMode::Full => write!(f, "full"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ManagedRoutesConfig {
    /// Enable managed route sources. If the block is present, default is true.
    pub enabled: Option<bool>,
    /// Reserved for future runtime refresh. v1 resolves managed routes on startup.
    pub refresh_interval_secs: Option<u64>,
    /// How long cached source routes can be used when a source temporarily fails.
    pub stale_ttl_secs: Option<i64>,
    /// Include IPv6 routes from managed sources. Default is false.
    pub include_ipv6: Option<bool>,
    /// Cache file path. Relative paths are resolved next to the config file.
    pub cache_file: Option<String>,
    pub sources: Option<Vec<ManagedRouteSource>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagedRouteSource {
    GithubMeta {
        name: String,
        keys: Option<Vec<String>>,
        meta_url: Option<String>,
    },
    DnsHosts {
        name: String,
        hosts: Vec<String>,
        port: Option<u16>,
    },
}

impl ManagedRouteSource {
    pub fn name(&self) -> &str {
        match self {
            ManagedRouteSource::GithubMeta { name, .. } => name,
            ManagedRouteSource::DnsHosts { name, .. } => name,
        }
    }

    pub fn source_type(&self) -> &str {
        match self {
            ManagedRouteSource::GithubMeta { .. } => "github_meta",
            ManagedRouteSource::DnsHosts { .. } => "dns_hosts",
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub company_name: String,
    pub username: String,
    pub password: Option<String>,
    pub platform: Option<String>,
    pub code: Option<String>,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    pub server: Option<String>,
    pub interface_name: Option<String>,
    pub debug_wg: Option<bool>,
    #[serde(skip_serializing)]
    pub conf_file: Option<String>,
    #[serde(skip)]
    pub(crate) legacy_cookie_migration: bool,
    #[serde(skip)]
    pub(crate) declared_server: Option<String>,
    pub state: Option<State>,
    pub vpn_server_name: Option<String>,
    pub vpn_select_strategy: Option<String>,
    pub use_vpn_dns: Option<bool>,
    pub dns_backup_filename: Option<String>,
    pub auto_setup_routes: Option<bool>,
    /// "split" (default) or "full". Selects which route list from the server to apply.
    pub route_mode: Option<RouteMode>,
    /// Optional list of CIDR routes to exclude from AllowedIPs / system routes.
    /// Useful in full mode to punch holes for local LAN or the VPN peer IP itself,
    /// avoiding routing loops (e.g. 192.168.1.0/24, 10.0.0.5/32).
    pub vpn_disallowed_routes: Option<Vec<String>>,
    /// Optional list of CIDRs or bare IPs to append to AllowedIPs / system routes.
    /// Useful when the server-side split route list misses public SaaS ranges.
    #[serde(alias = "routes")]
    pub extra_allowed_ips: Option<Vec<String>>,
    /// Optional managed route sources for public SaaS endpoints behind IP allowlists.
    pub managed_routes: Option<ManagedRoutesConfig>,
    /// When set, run entirely in userspace (gVisor netstack) and expose a SOCKS5
    /// proxy at this listen address (e.g. "0.0.0.0:1080" or "127.0.0.1:1080")
    /// instead of creating a kernel TUN device. No system interface, routes, DNS
    /// changes or root privileges are required. Only TCP CONNECT is supported.
    pub socks5_listen: Option<String>,
    /// Optional SOCKS5 username/password authentication (RFC 1929). When
    /// `socks5_username` is set and non-empty, clients must authenticate with
    /// these credentials; otherwise the proxy accepts connections without auth.
    pub socks5_username: Option<String>,
    pub socks5_password: Option<String>,
    /// Force the WireGuard transport protocol instead of using the server-advertised
    /// `protocol_mode`. Accepts "udp" or "tcp" (case-insensitive). Some `protocol_mode: 1`
    /// (TCP) gateways also accept WireGuard over UDP -- for those the server even ships a
    /// `protocol_detect_config` (udp<->tcp switch thresholds) in the `/api/vpn/list` entry.
    /// Since WireGuard-over-TCP can collapse to a few KB/s on a lossy uplink (TCP-over-TCP
    /// head-of-line blocking), forcing "udp" can be far faster there. Leave unset to keep the
    /// default (follow server `protocol_mode`: 1 => tcp, otherwise udp).
    pub force_protocol: Option<String>,
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match serde_json::to_string_pretty(self) {
            Ok(s) => write!(f, "{}", s),
            Err(e) => write!(f, "<invalid config: {e}>"),
        }
    }
}

impl Config {
    /// Parse a user configuration and bind its source path without applying
    /// defaults, generating keys, or touching any sidecar/state file.
    pub async fn read_only(file: &str) -> Result<Config> {
        let conf_str = fs::read_to_string(file)
            .await
            .with_context(|| format!("failed to read config file {file}"))?;
        let mut conf: Config = serde_json::from_str(&conf_str)
            .with_context(|| format!("failed to parse config file {file}"))?;
        conf.conf_file = Some(file.to_string());
        conf.declared_server = conf.server.clone();
        Ok(conf)
    }

    pub async fn from_file(file: &str) -> Result<Config> {
        let mut conf = Self::read_only(file).await?;
        let mut update_session = false;
        if conf.interface_name.is_none() {
            conf.interface_name = Some(DEFAULT_INTERFACE_NAME.to_string());
            update_session = true;
        }

        let interface_name = conf
            .interface_name
            .as_deref()
            .context("interface name missing after applying default")?;
        let session_path = state::session_file_path(file, interface_name);
        conf.legacy_cookie_migration = !session_path.exists()
            && (conf.state.is_some()
                || conf.device_name.is_some()
                || conf.device_id.is_some()
                || conf.public_key.is_some()
                || conf.private_key.is_some()
                || conf.code.is_some());
        let loaded_session = state::load_session(&session_path).with_context(|| {
            format!(
                "failed to read authentication session state {}",
                session_path.display()
            )
        })?;
        let session_corrupt = session_path.exists() && loaded_session.is_none();
        let identity = conf.session_identity_tag();
        let session_stale = loaded_session
            .as_ref()
            .map(|session| session.identity.as_deref() != Some(identity.as_str()))
            .unwrap_or(false);
        let session_usable = loaded_session.is_some() && !session_corrupt && !session_stale;
        let private_key_was_explicit = conf.private_key.is_some();
        let public_key_was_explicit = conf.public_key.is_some();
        if session_corrupt {
            // Treat a damaged sidecar as an expired session. Keep the file for
            // forensics and write a fresh state only after all in-memory
            // defaults below have been resolved.
            log::warn!("authentication session state is unreadable; requiring login");
            conf.state = Some(State::Init);
        } else if session_stale {
            log::warn!("authentication session belongs to a different config; requiring login");
            conf.state = Some(State::Init);
        }

        if session_usable {
            let session = loaded_session.expect("usable session must be present");
            let session_public_key_matches =
                session.public_key.as_ref() == conf.public_key.as_ref();
            conf.legacy_cookie_migration |= session.legacy_cookie_migration;
            if conf.device_name.is_none() {
                conf.device_name = session.device_name;
            }
            if conf.device_id.is_none() {
                conf.device_id = session.device_id;
            }
            if conf.public_key.is_none() && !private_key_was_explicit {
                conf.public_key = session.public_key;
            }
            if conf.private_key.is_none()
                && (!public_key_was_explicit || session_public_key_matches)
            {
                conf.private_key = session.private_key;
            }
            if conf.code.is_none() {
                conf.code = session.code;
            }
            // State describes the current authentication session, so the
            // sidecar is authoritative whenever it is valid.
            conf.state = Some(session.state);
        }

        if conf.device_name.is_none() {
            conf.device_name = Some(DEFAULT_DEVICE_NAME.to_string());
            update_session = true;
        }
        if conf.device_id.is_none() {
            let device_name = conf
                .device_name
                .as_ref()
                .context("device name missing when generating device id")?;
            conf.device_id = Some(format!("{:x}", md5::compute(device_name)));
            update_session = true;
        }
        match &conf.private_key {
            Some(private_key) => match conf.public_key {
                Some(_) => {
                    // both keys exist, do nothing
                }
                None => {
                    // only private key exists, generate public from private
                    let public_key = utils::gen_public_key_from_private(private_key)?;
                    conf.public_key = Some(public_key);
                    update_session = true;
                }
            },
            None => {
                if public_key_was_explicit {
                    return Err(anyhow::Error::new(
                        crate::api::ClientFailure::configuration("wireguard key pair"),
                    ));
                }
                // no key exists, generate new
                let (public_key, private_key) = utils::gen_wg_keypair();
                (conf.public_key, conf.private_key) = (Some(public_key), Some(private_key));
                update_session = true;
            }
        }

        if conf.state.is_none() {
            conf.state = Some(State::Init);
            update_session = true;
        }
        if !session_path.exists() {
            update_session = true;
        }
        if update_session && (!session_path.exists() || session_usable) {
            conf.save_session().await?;
        }
        Ok(conf)
    }

    pub fn session_path(&self) -> Result<PathBuf> {
        let file = self
            .conf_file
            .as_ref()
            .context("config file path missing")?;
        let interface_name = self
            .interface_name
            .as_deref()
            .unwrap_or(DEFAULT_INTERFACE_NAME);
        Ok(state::session_file_path(file, interface_name))
    }

    /// Return whether a persisted session belongs to this config identity.
    /// Missing sidecars are treated as legacy-compatible: old config files may
    /// still carry their own device/key/state fields and can migrate on save.
    pub fn session_identity_matches(&self) -> Result<bool> {
        let path = self.session_path()?;
        if !path.exists() {
            return Ok(true);
        }
        Ok(state::load_session(&path)?
            .and_then(|session| session.identity)
            .map(|identity| identity == self.session_identity_tag())
            .unwrap_or(false))
    }

    pub fn session_snapshot(&self) -> SessionState {
        SessionState {
            identity: Some(self.session_identity_tag()),
            legacy_cookie_migration: self.legacy_cookie_migration,
            state: self.state.clone().unwrap_or_default(),
            device_name: self.device_name.clone(),
            device_id: self.device_id.clone(),
            public_key: self.public_key.clone(),
            private_key: self.private_key.clone(),
            code: self.code.clone(),
        }
    }

    pub(crate) fn session_identity_tag(&self) -> String {
        let file = self.conf_file.as_deref().unwrap_or_default();
        // CLI-relative paths and the launcher's absolute path identify the
        // same configuration, including when a caller uses a symlink alias.
        let identity_file = std::fs::canonicalize(file)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| file.to_string());
        let platform = self.platform.as_deref().unwrap_or_default();
        let server = self.declared_server.as_deref().unwrap_or_default();
        let material = format!(
            "{identity_file}\n{}\n{}\n{platform}\n{server}",
            self.company_name, self.username
        );
        let digest = Sha256::digest(material.as_bytes());
        format!("{digest:x}")
    }

    pub(crate) fn allow_legacy_cookie_migration(&self) -> bool {
        self.legacy_cookie_migration
    }

    pub async fn save_session(&self) -> Result<()> {
        let path = self.session_path()?;
        state::save_session(&path, &self.session_snapshot()).with_context(|| {
            format!(
                "failed to persist authentication session state {}",
                path.display()
            )
        })
    }

    pub fn save_session_sync(&self) -> Result<()> {
        let path = self.session_path()?;
        state::save_session(&path, &self.session_snapshot()).with_context(|| {
            format!(
                "failed to persist authentication session state {}",
                path.display()
            )
        })
    }
}

#[derive(Serialize, Clone)]
pub struct WgConf {
    // standard wg conf
    pub address: String,
    pub address6: String,
    pub peer_address: String,
    pub mtu: u32,
    pub public_key: String,
    pub private_key: String,
    pub peer_key: String,
    pub allowed_ips: Vec<String>,
    pub routes: Vec<String>,

    // extra confs
    pub dns: String,

    // corplink confs
    pub protocol: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_config_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("corplink-config-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[tokio::test]
    async fn read_only_parses_and_binds_without_generating_or_writing() {
        let path = test_config_path("read-only");
        let source = br#"{"company_name":"company","username":"user"}"#;
        fs::write(&path, source).unwrap();

        let config = Config::read_only(path.to_str().unwrap()).await.unwrap();

        assert_eq!(config.conf_file.as_deref(), path.to_str());
        assert!(config.interface_name.is_none());
        assert!(config.public_key.is_none());
        assert!(config.private_key.is_none());
        assert_eq!(fs::read(&path).unwrap(), source);
        assert!(!crate::state::session_file_path(path.to_str().unwrap(), "corplink").exists());

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relative_and_absolute_config_paths_share_session_identity() {
        let path = test_config_path("path-identity");
        let source = br#"{"company_name":"company","username":"user"}"#;
        fs::write(&path, source).unwrap();
        let mut relative = PathBuf::new();
        for _ in std::env::current_dir().unwrap().components().skip(1) {
            relative.push("..");
        }
        relative.push(path.strip_prefix("/").unwrap());
        let absolute_config = Config::read_only(path.to_str().unwrap()).await.unwrap();
        let relative_config = Config::read_only(relative.to_str().unwrap()).await.unwrap();

        assert_eq!(
            absolute_config.session_snapshot().identity,
            relative_config.session_snapshot().identity
        );
        assert_eq!(fs::read(&path).unwrap(), source);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn from_file_keeps_user_file_unchanged_and_persists_runtime_defaults() {
        let path = test_config_path("from-file");
        let source = br#"{"company_name":"company","username":"user"}"#;
        fs::write(&path, source).unwrap();

        let config = Config::from_file(path.to_str().unwrap()).await.unwrap();
        let session_path = crate::state::session_file_path(path.to_str().unwrap(), "corplink");
        let session = crate::state::load_session(&session_path).unwrap().unwrap();

        assert_eq!(fs::read(&path).unwrap(), source);
        assert_eq!(config.interface_name.as_deref(), Some("corplink"));
        assert_eq!(config.state, Some(State::Init));
        assert_eq!(session.state, State::Init);
        assert_eq!(session.public_key, config.public_key);
        assert_eq!(session.private_key, config.private_key);
        assert_eq!(session.device_id, config.device_id);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn corrupt_session_is_preserved_and_requires_login() {
        let path = test_config_path("corrupt-session");
        let source = br#"{"company_name":"company","username":"user"}"#;
        fs::write(&path, source).unwrap();
        let session_path = crate::state::session_file_path(path.to_str().unwrap(), "corplink");
        let corrupt = b"session-secret-and-not-json";
        fs::write(&session_path, corrupt).unwrap();

        let config = Config::from_file(path.to_str().unwrap()).await.unwrap();

        assert_eq!(config.state, Some(State::Init));
        assert_eq!(fs::read(&path).unwrap(), source);
        assert_eq!(fs::read(&session_path).unwrap(), corrupt);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn session_identity_prevents_cross_account_reuse() {
        let path = test_config_path("identity");
        let first = br#"{"company_name":"company","username":"first"}"#;
        fs::write(&path, first).unwrap();
        let first_config = Config::from_file(path.to_str().unwrap()).await.unwrap();
        let session_path = crate::state::session_file_path(path.to_str().unwrap(), "corplink");
        let old_session = fs::read(&session_path).unwrap();

        let second = br#"{"company_name":"company","username":"second"}"#;
        fs::write(&path, second).unwrap();
        let second_config = Config::from_file(path.to_str().unwrap()).await.unwrap();

        assert_eq!(second_config.state, Some(State::Init));
        assert_ne!(second_config.public_key, first_config.public_key);
        assert_eq!(fs::read(&session_path).unwrap(), old_session);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn explicit_private_key_derives_public_without_reusing_old_session_key() {
        let path = test_config_path("explicit-private");
        let source = br#"{"company_name":"company","username":"user"}"#;
        fs::write(&path, source).unwrap();
        let first = Config::from_file(path.to_str().unwrap()).await.unwrap();
        let old_public = first.public_key.clone();
        let (new_public, new_private) = crate::utils::gen_wg_keypair();
        assert_ne!(old_public, Some(new_public.clone()));

        let explicit = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"private_key\":\"{new_private}\"}}"
        );
        fs::write(&path, explicit).unwrap();
        let second = Config::from_file(path.to_str().unwrap()).await.unwrap();

        assert_eq!(second.private_key, Some(new_private.clone()));
        assert_eq!(second.public_key, Some(new_public));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn discovered_server_does_not_change_declared_session_identity() {
        let path = test_config_path("discovered-server");
        fs::write(&path, br#"{"company_name":"company","username":"user"}"#).unwrap();
        let mut config = Config::from_file(path.to_str().unwrap()).await.unwrap();
        config.server = Some("http://127.0.0.1:9".to_string());

        assert!(config.session_identity_matches().unwrap());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::fs::{self, OpenOptions};
#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

#[cfg(target_os = "linux")]
const LINUX_DEFAULT_BACKUP_FILENAME: &str = "resolv.conf.corplink";

#[cfg(target_os = "macos")]
const MACOS_DEFAULT_BACKUP_PATH: &str = "/var/run/corplink-rs/dns-backup.json";

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct MacOwnerIdentity {
    pid: u32,
    start_token: String,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct MacDnsOriginal {
    dns: String,
    search: String,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct MacDnsSnapshot {
    version: u8,
    owner: MacOwnerIdentity,
    services: HashMap<String, MacDnsOriginal>,
}

pub struct DNSManager {
    #[cfg(target_os = "macos")]
    service_dns: HashMap<String, String>,
    #[cfg(target_os = "macos")]
    service_dns_search: HashMap<String, String>,
    #[cfg(target_os = "macos")]
    backup_path: PathBuf,
    #[cfg(target_os = "macos")]
    networksetup_path: PathBuf,

    #[cfg(target_os = "linux")]
    backup_path: PathBuf,
    #[cfg(target_os = "linux")]
    resolv_path: PathBuf,
}

impl DNSManager {
    pub fn new(_backup_filename: Option<String>) -> DNSManager {
        DNSManager {
            #[cfg(target_os = "macos")]
            service_dns: HashMap::new(),
            #[cfg(target_os = "macos")]
            service_dns_search: HashMap::new(),
            #[cfg(target_os = "macos")]
            backup_path: _backup_filename
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(MACOS_DEFAULT_BACKUP_PATH)),
            #[cfg(target_os = "macos")]
            networksetup_path: PathBuf::from("networksetup"),

            #[cfg(target_os = "linux")]
            backup_path: {
                let filename = _backup_filename
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| LINUX_DEFAULT_BACKUP_FILENAME.to_string());
                Path::new(RESOLV_CONF_PATH)
                    .parent()
                    .unwrap_or_else(|| Path::new("/etc"))
                    .join(filename)
            },
            #[cfg(target_os = "linux")]
            resolv_path: PathBuf::from(RESOLV_CONF_PATH),
        }
    }

    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }

    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }

    #[cfg(target_os = "linux")]
    #[cfg(test)]
    fn with_paths(resolv_path: impl Into<PathBuf>, backup_path: impl Into<PathBuf>) -> DNSManager {
        DNSManager {
            backup_path: backup_path.into(),
            resolv_path: resolv_path.into(),
        }
    }

    #[cfg(target_os = "macos")]
    #[cfg(test)]
    fn with_test_paths(
        backup_path: impl Into<PathBuf>,
        networksetup_path: impl Into<PathBuf>,
    ) -> DNSManager {
        DNSManager {
            service_dns: HashMap::new(),
            service_dns_search: HashMap::new(),
            backup_path: backup_path.into(),
            networksetup_path: networksetup_path.into(),
        }
    }
}

#[cfg(target_os = "macos")]
impl DNSManager {
    fn collect_service_dns(&self) -> Result<HashMap<String, MacDnsOriginal>> {
        let output = Command::new(&self.networksetup_path)
            .arg("-listallnetworkservices")
            .output()
            .context("failed to list network services")?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "networksetup -listallnetworkservices failed with {}",
                output.status
            ));
        }

        let services = String::from_utf8_lossy(&output.stdout);
        let lines = services.lines();
        // Skip the first line's legend
        let mut originals = HashMap::new();
        for service in lines.skip(1) {
            // Remove leading '*' and trim whitespace
            let service = service.trim_start_matches('*').trim();
            if service.is_empty() {
                continue;
            }

            // get DNS servers
            let dns_output = Command::new(&self.networksetup_path)
                .arg("-getdnsservers")
                .arg(service)
                .output()
                .with_context(|| format!("failed to get dns servers for {service}"))?;
            if !dns_output.status.success() {
                return Err(anyhow::anyhow!(
                    "failed to get dns servers for {service}: {}",
                    dns_output.status
                ));
            }
            let dns_response = String::from_utf8_lossy(&dns_output.stdout)
                .trim()
                .to_string();
            // if dns config for this service is not empty, output should be ip addresses seperated in lines without space
            // otherwise, output should be "There aren't any DNS Servers set on xxx", use "Empty" instead, which can be recognized in 'networksetup -setdnsservers'
            let dns_response = if dns_response.is_empty() || dns_response.contains("There aren't") {
                "Empty".to_string()
            } else {
                dns_response
            };

            // get search domain
            let search_output = Command::new(&self.networksetup_path)
                .arg("-getsearchdomains")
                .arg(service)
                .output()
                .with_context(|| format!("failed to get search domains for {service}"))?;
            if !search_output.status.success() {
                return Err(anyhow::anyhow!(
                    "failed to get search domains for {service}: {}",
                    search_output.status
                ));
            }
            let search_response = String::from_utf8_lossy(&search_output.stdout)
                .trim()
                .to_string();
            let search_response =
                if search_response.is_empty() || search_response.contains("There aren't") {
                    "Empty".to_string()
                } else {
                    search_response
                };

            originals.insert(
                service.to_string(),
                MacDnsOriginal {
                    dns: dns_response.clone(),
                    search: search_response.clone(),
                },
            );

            log::debug!(
                "DNS collected for {}, dns servers: {}, search domain: {}",
                service,
                dns_response,
                search_response
            )
        }
        Ok(originals)
    }

    fn current_identity(&self) -> Result<MacOwnerIdentity> {
        Ok(MacOwnerIdentity {
            pid: std::process::id(),
            start_token: process_start_token(std::process::id())?
                .context("current process start token is unavailable")?,
        })
    }

    fn load_snapshot(&self) -> Result<MacDnsSnapshot> {
        let data = fs::read_to_string(&self.backup_path)
            .with_context(|| format!("failed to read DNS backup {}", self.backup_path.display()))?;
        let snapshot = serde_json::from_str(&data)
            .with_context(|| format!("DNS backup is corrupt: {}", self.backup_path.display()))?;
        Ok(snapshot)
    }

    fn write_snapshot(&self, snapshot: &MacDnsSnapshot) -> Result<()> {
        let parent = self
            .backup_path
            .parent()
            .context("DNS backup path has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create DNS backup directory {}", parent.display())
        })?;
        let data = serde_json::to_vec_pretty(snapshot).context("failed to serialize DNS backup")?;
        let temp = parent.join(format!(
            ".{}.{}.tmp",
            self.backup_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("dns-backup"),
            std::process::id()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            options.mode(0o600);
            let mut file = options.open(&temp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.backup_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.with_context(|| {
            format!(
                "failed to atomically persist DNS backup {}",
                self.backup_path.display()
            )
        })
    }

    fn prepare_snapshot(&mut self) -> Result<MacDnsSnapshot> {
        let identity = self.current_identity()?;
        if self.backup_path.exists() {
            let snapshot = self.load_snapshot()?;
            match snapshot_owner_status(&snapshot.owner, &identity)? {
                SnapshotOwnerStatus::Current => {
                    self.load_originals(&snapshot);
                    return Ok(snapshot);
                }
                SnapshotOwnerStatus::ActiveOther => {
                    return Err(anyhow::anyhow!(
                        "DNS backup is owned by a live process; refusing to overwrite {}",
                        snapshot.owner.pid
                    ));
                }
                SnapshotOwnerStatus::Unknown => {
                    return Err(anyhow::anyhow!(
                        "DNS backup owner identity is unknown; refusing to overwrite"
                    ));
                }
                SnapshotOwnerStatus::Dead => {
                    self.apply_originals(&snapshot.services)?;
                    fs::remove_file(&self.backup_path).with_context(|| {
                        format!(
                            "failed to remove restored DNS backup {}",
                            self.backup_path.display()
                        )
                    })?;
                }
            }
        }

        let originals = self.collect_service_dns()?;
        let snapshot = MacDnsSnapshot {
            version: 1,
            owner: identity,
            services: originals,
        };
        self.write_snapshot(&snapshot)?;
        self.load_originals(&snapshot);
        Ok(snapshot)
    }

    fn load_originals(&mut self, snapshot: &MacDnsSnapshot) {
        self.service_dns = snapshot
            .services
            .iter()
            .map(|(service, original)| (service.clone(), original.dns.clone()))
            .collect();
        self.service_dns_search = snapshot
            .services
            .iter()
            .map(|(service, original)| (service.clone(), original.search.clone()))
            .collect();
    }

    fn apply_originals(&self, originals: &HashMap<String, MacDnsOriginal>) -> Result<()> {
        let mut errors = Vec::new();
        for (service, original) in originals {
            if let Err(error) = self.set_service_dns(service, &original.dns) {
                errors.push(error.to_string());
            }
            if let Err(error) = self.set_service_search(service, &original.search) {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }

    fn set_service_dns(&self, service: &str, dns: &str) -> Result<()> {
        let status = Command::new(&self.networksetup_path)
            .arg("-setdnsservers")
            .arg(service)
            .args(if dns == "Empty" || dns.is_empty() {
                vec!["Empty"]
            } else {
                dns.lines().collect()
            })
            .status()
            .with_context(|| format!("failed to set dns servers for {service}"))?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "failed to set dns servers for {service}: {status}"
            ));
        }
        Ok(())
    }

    fn set_service_search(&self, service: &str, search: &str) -> Result<()> {
        let status = Command::new(&self.networksetup_path)
            .arg("-setsearchdomains")
            .arg(service)
            .args(if search == "Empty" || search.is_empty() {
                vec!["Empty"]
            } else {
                search.lines().collect()
            })
            .status()
            .with_context(|| format!("failed to set search domains for {service}"))?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "failed to set search domains for {service}: {status}"
            ));
        }
        Ok(())
    }

    pub fn set_dns(&mut self, dns_servers: Vec<&str>, dns_search: Vec<&str>) -> Result<()> {
        if dns_servers.is_empty() {
            return Ok(());
        }
        let snapshot = self.prepare_snapshot()?;
        for service in snapshot.services.keys() {
            self.set_service_dns(service, &dns_servers.join("\n"))?;
            if !dns_search.is_empty() {
                self.set_service_search(service, &dns_search.join("\n"))?;
            }
            log::debug!("DNS set for {} with {}", service, dns_servers.join(","));
        }

        Ok(())
    }

    pub fn restore_dns(&self) -> Result<()> {
        let (originals, remove_snapshot) = if self.backup_path.exists() {
            let snapshot = self.load_snapshot()?;
            let current = self.current_identity()?;
            match snapshot_owner_status(&snapshot.owner, &current)? {
                SnapshotOwnerStatus::Current | SnapshotOwnerStatus::Dead => {
                    (snapshot.services, true)
                }
                SnapshotOwnerStatus::ActiveOther => {
                    return Err(anyhow::anyhow!(
                        "DNS backup is owned by a live process; refusing to restore"
                    ));
                }
                SnapshotOwnerStatus::Unknown => {
                    return Err(anyhow::anyhow!(
                        "DNS backup owner identity is unknown; refusing to restore"
                    ));
                }
            }
        } else {
            (
                self.service_dns
                    .iter()
                    .map(|(service, dns)| {
                        (
                            service.clone(),
                            MacDnsOriginal {
                                dns: dns.clone(),
                                search: self
                                    .service_dns_search
                                    .get(service)
                                    .cloned()
                                    .unwrap_or_else(|| "Empty".to_string()),
                            },
                        )
                    })
                    .collect(),
                false,
            )
        };
        self.apply_originals(&originals)?;
        if remove_snapshot && self.backup_path.exists() {
            fs::remove_file(&self.backup_path).with_context(|| {
                format!(
                    "failed to remove restored DNS backup {}",
                    self.backup_path.display()
                )
            })?;
        }
        log::debug!("DNS reset");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotOwnerStatus {
    Current,
    ActiveOther,
    Dead,
    Unknown,
}

#[cfg(target_os = "macos")]
fn process_start_token(pid: u32) -> Result<Option<String>> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .with_context(|| format!("failed to inspect process identity for pid {pid}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(anyhow::anyhow!(
            "process start token output is empty for pid {pid}"
        ));
    }
    Ok(Some(token))
}

#[cfg(target_os = "macos")]
fn snapshot_owner_status(
    owner: &MacOwnerIdentity,
    current: &MacOwnerIdentity,
) -> Result<SnapshotOwnerStatus> {
    if owner.pid == current.pid && owner.start_token == current.start_token {
        return Ok(SnapshotOwnerStatus::Current);
    }
    let Some(token) = process_start_token(owner.pid)? else {
        return Ok(SnapshotOwnerStatus::Dead);
    };
    if token == owner.start_token {
        Ok(SnapshotOwnerStatus::ActiveOther)
    } else {
        Ok(SnapshotOwnerStatus::Dead)
    }
}

#[cfg(target_os = "linux")]
impl DNSManager {
    pub fn set_dns(&mut self, dns_servers: Vec<&str>, dns_search: Vec<&str>) -> Result<()> {
        if dns_servers.is_empty() {
            return Ok(());
        }

        if self.backup_path.exists() {
            log::warn!(
                "existing backup at {} — a previous instance likely did not exit \
                 gracefully; keeping that file as the authoritative pre-override",
                self.backup_path.display()
            );
        } else {
            fs::rename(&self.resolv_path, &self.backup_path).with_context(|| {
                format!(
                    "failed to back up {} to {}",
                    self.resolv_path.display(),
                    self.backup_path.display()
                )
            })?;
            log::info!(
                "renamed {} -> {} for backup",
                self.resolv_path.display(),
                self.backup_path.display()
            );
        }

        let new_content = render_resolv_conf(&dns_servers, &dns_search);
        if let Err(write_error) = fs::write(&self.resolv_path, &new_content) {
            let restore_error = fs::rename(&self.backup_path, &self.resolv_path).err();
            return Err(anyhow::anyhow!(
                "failed to write {}: {}; restore error: {}",
                self.resolv_path.display(),
                write_error,
                restore_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "none".to_string())
            ));
        }

        log::info!(
            "DNS overridden in {}; servers={:?} search={:?}",
            self.resolv_path.display(),
            dns_servers,
            dns_search
        );
        Ok(())
    }

    pub fn restore_dns(&self) -> Result<()> {
        if !self.backup_path.exists() {
            return Ok(());
        }
        match fs::rename(&self.backup_path, &self.resolv_path) {
            Ok(()) => {
                log::info!(
                    "restored {} from {} (via rename)",
                    self.resolv_path.display(),
                    self.backup_path.display()
                );
                Ok(())
            }
            Err(e) => Err(e).with_context(|| {
                format!(
                    "could not restore {} by renaming {} back; backup retained",
                    self.resolv_path.display(),
                    self.backup_path.display()
                )
            }),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl DNSManager {
    pub fn set_dns(&mut self, _dns_servers: Vec<&str>, _dns_search: Vec<&str>) -> Result<()> {
        Ok(())
    }
    pub fn restore_dns(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn render_resolv_conf(dns_servers: &[&str], dns_search: &[&str]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by corplink-rs (will be restored on graceful exit)\n");
    for dns in dns_servers {
        out.push_str(&format!("nameserver {dns}\n"));
    }
    if !dns_search.is_empty() {
        out.push_str(&format!("search {}\n", dns_search.join(" ")));
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_filename_when_none_given() {
        let m = DNSManager::new(None);
        let expected = Path::new(RESOLV_CONF_PATH)
            .parent()
            .unwrap()
            .join(LINUX_DEFAULT_BACKUP_FILENAME);
        assert_eq!(m.backup_path(), expected.as_path());
    }

    #[test]
    fn default_filename_when_empty_string_given() {
        let m = DNSManager::new(Some(String::new()));
        let expected = Path::new(RESOLV_CONF_PATH)
            .parent()
            .unwrap()
            .join(LINUX_DEFAULT_BACKUP_FILENAME);
        assert_eq!(m.backup_path(), expected.as_path());
    }

    #[test]
    fn custom_filename_joined_with_resolv_conf_parent() {
        let m = DNSManager::new(Some("my.bak".to_string()));
        assert_eq!(m.backup_path(), Path::new("/etc/my.bak"));
    }

    #[test]
    fn backup_path_always_in_resolv_conf_dir() {
        // Invariant: because we only take a filename and join it with
        // RESOLV_CONF_PATH's parent, the backup is always on the same fs
        // as /etc/resolv.conf — rename(2) cannot EXDEV.
        let resolv_dir = Path::new(RESOLV_CONF_PATH).parent().unwrap();
        for filename in ["resolv.conf.corplink", "other.bak", "x"] {
            let m = DNSManager::new(Some(filename.to_string()));
            assert_eq!(m.backup_path().parent().unwrap(), resolv_dir);
        }
    }

    #[test]
    fn render_single_dns_no_search() {
        let out = render_resolv_conf(&["10.8.8.18"], &[]);
        assert!(out.contains("nameserver 10.8.8.18\n"));
        assert!(!out.contains("search "));
    }

    #[test]
    fn render_multiple_dns() {
        let out = render_resolv_conf(&["10.8.8.18", "114.114.114.114"], &[]);
        assert!(out.contains("nameserver 10.8.8.18\n"));
        assert!(out.contains("nameserver 114.114.114.114\n"));
    }

    #[test]
    fn render_with_search_domains() {
        let out = render_resolv_conf(&["10.8.8.18"], &["bytedance.net", "corp.local"]);
        assert!(out.contains("search bytedance.net corp.local\n"));
    }

    #[test]
    fn render_starts_with_comment_marker() {
        let out = render_resolv_conf(&["1.1.1.1"], &[]);
        assert!(
            out.starts_with("# "),
            "expected a comment banner, got: {out}"
        );
    }

    #[test]
    fn set_and_restore_use_the_original_file_and_surface_restore_errors() {
        let dir = std::env::temp_dir().join(format!("corplink-dns-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let resolv = dir.join("resolv.conf");
        let backup = dir.join("resolv.conf.corplink");
        fs::write(&resolv, b"nameserver 192.0.2.1\n").unwrap();
        let mut manager = DNSManager::with_paths(&resolv, &backup);

        manager.set_dns(vec!["10.8.8.18"], vec![]).unwrap();
        assert!(backup.exists());
        assert!(String::from_utf8(fs::read(&resolv).unwrap())
            .unwrap()
            .contains("nameserver 10.8.8.18"));
        manager.restore_dns().unwrap();
        assert_eq!(fs::read(&resolv).unwrap(), b"nameserver 192.0.2.1\n");
        assert!(!backup.exists());

        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_networksetup(dir: &Path) -> PathBuf {
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("dns"),
            b"There aren't any DNS Servers set on Wi-Fi\n",
        )
        .unwrap();
        fs::write(
            state.join("search"),
            b"There aren't any Search Domains set on Wi-Fi\n",
        )
        .unwrap();
        let script = dir.join("networksetup");
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n  -listallnetworkservices) printf '%s\\n' 'An asterisk (*) denotes that a network service is disabled.' 'Wi-Fi' ;;\n  -getdnsservers) cat '{}' ;;\n  -getsearchdomains) cat '{}' ;;\n  -setdnsservers|-setsearchdomains) if [ -f '{}'/fail ]; then exit 1; fi; printf '%s\\n' \"$@\" >> '{}'/calls; exit 0 ;;\n  *) exit 1 ;;\nesac\n",
            state.join("dns").display(),
            state.join("search").display(),
            state.display(),
            state.display(),
        );
        fs::write(&script, body).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    #[test]
    fn default_snapshot_path_is_persistent_runtime_path() {
        let manager = DNSManager::new(None);
        assert_eq!(manager.backup_path(), Path::new(MACOS_DEFAULT_BACKUP_PATH));
    }

    #[test]
    fn snapshot_is_written_before_macos_dns_override() {
        let dir = std::env::temp_dir().join(format!("corplink-macos-dns-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup = dir.join("dns-backup.json");
        let mut manager = DNSManager::with_test_paths(&backup, &command);

        manager.set_dns(vec!["10.8.8.18"], vec![]).unwrap();

        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(&backup).unwrap()).unwrap();
        assert_eq!(snapshot["services"]["Wi-Fi"]["dns"], "Empty");
        assert_eq!(snapshot["services"]["Wi-Fi"]["search"], "Empty");
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    fn write_snapshot(backup: &Path, dns: &str, search: &str) {
        let mut services = HashMap::new();
        services.insert(
            "Wi-Fi".to_string(),
            MacDnsOriginal {
                dns: dns.to_string(),
                search: search.to_string(),
            },
        );
        let snapshot = MacDnsSnapshot {
            version: 1,
            owner: MacOwnerIdentity {
                pid: u32::MAX,
                start_token: "dead-owner".to_string(),
            },
            services,
        };
        fs::write(backup, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    }

    #[test]
    fn dead_owner_is_restored_before_new_override() {
        let dir =
            std::env::temp_dir().join(format!("corplink-macos-dns-dead-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup = dir.join("dns-backup.json");
        write_snapshot(&backup, "1.1.1.1", "corp.local");
        let mut manager = DNSManager::with_test_paths(&backup, &command);

        manager.set_dns(vec!["10.8.8.18"], vec![]).unwrap();

        let calls = fs::read_to_string(dir.join("state/calls")).unwrap();
        assert!(calls.contains("1.1.1.1"));
        assert!(calls.contains("corp.local"));
        assert!(calls.contains("10.8.8.18"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_write_failure_does_not_modify_dns() {
        let dir = std::env::temp_dir().join(format!(
            "corplink-macos-dns-write-fail-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup_dir = dir.join("backup-directory");
        fs::create_dir_all(&backup_dir).unwrap();
        let mut manager = DNSManager::with_test_paths(&backup_dir, &command);

        assert!(manager.set_dns(vec!["10.8.8.18"], vec![]).is_err());
        assert!(!dir.join("state/calls").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_restore_keeps_snapshot_for_retry() {
        let dir =
            std::env::temp_dir().join(format!("corplink-macos-dns-partial-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup = dir.join("dns-backup.json");
        write_snapshot(&backup, "1.1.1.1", "corp.local");
        fs::write(dir.join("state/fail"), b"1").unwrap();
        let mut manager = DNSManager::with_test_paths(&backup, &command);

        assert!(manager.set_dns(vec!["10.8.8.18"], vec![]).is_err());
        assert!(backup.exists());
        fs::remove_file(dir.join("state/fail")).unwrap();
        manager.restore_dns().unwrap();
        assert!(!backup.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_snapshot_refuses_override_and_preserves_evidence() {
        let dir =
            std::env::temp_dir().join(format!("corplink-macos-dns-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup = dir.join("dns-backup.json");
        fs::write(&backup, b"not-json").unwrap();
        let mut manager = DNSManager::with_test_paths(&backup, &command);

        assert!(manager.set_dns(vec!["10.8.8.18"], vec![]).is_err());
        assert_eq!(fs::read(&backup).unwrap(), b"not-json");
        assert!(!dir.join("state/calls").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restore_rejects_active_other_owner_after_set_failure() {
        let dir = std::env::temp_dir().join(format!(
            "corplink-macos-dns-active-owner-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let command = fake_networksetup(&dir);
        let backup = dir.join("dns-backup.json");
        let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
        let owner_pid = owner.id();
        let owner_token = process_start_token(owner_pid)
            .unwrap()
            .expect("sleep process should have a start token");
        let mut services = HashMap::new();
        services.insert(
            "Wi-Fi".to_string(),
            MacDnsOriginal {
                dns: "1.1.1.1".to_string(),
                search: "corp.local".to_string(),
            },
        );
        let snapshot = MacDnsSnapshot {
            version: 1,
            owner: MacOwnerIdentity {
                pid: owner_pid,
                start_token: owner_token,
            },
            services,
        };
        let original_snapshot = serde_json::to_vec(&snapshot).unwrap();
        fs::write(&backup, &original_snapshot).unwrap();
        let mut manager = DNSManager::with_test_paths(&backup, &command);

        assert!(manager.set_dns(vec!["10.8.8.18"], vec![]).is_err());
        assert!(manager.restore_dns().is_err());
        assert_eq!(fs::read(&backup).unwrap(), original_snapshot);
        assert!(!dir.join("state/calls").exists());

        let _ = owner.kill();
        let _ = owner.wait();
        fs::remove_dir_all(dir).unwrap();
    }
}

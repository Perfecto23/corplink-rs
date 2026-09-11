//! Ownership boundary for local WireGuard/TUN/netstack and DNS resources.
//!
//! Client owns remote authentication and VPN disconnect requests. This module
//! owns resources acquired on the current machine and releases them before the
//! caller attempts remote logout.

use anyhow::{anyhow, Context, Result};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::config::WgConf;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::dns::DNSManager;
use crate::wg::{self, UAPIClient, WgHealth};

const DEFAULT_HANDSHAKE_STALE_AFTER: Duration = Duration::from_secs(300);

pub type AdapterFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// The operating-system resource boundary. Production and tests use the same
/// NetworkSession lifecycle through this interface.
pub trait NetworkAdapter: Send {
    fn start(&mut self, conf: &WgConf) -> Result<()>;
    fn configure<'a>(&'a mut self, conf: &'a WgConf) -> AdapterFuture<'a>;
    fn set_dns(&mut self, dns: &str) -> Result<()>;
    fn restore_dns(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn health(&self) -> Result<WgHealth>;
}

#[derive(Clone, Copy, Debug)]
pub enum NetworkMode<'a> {
    Kernel,
    Netstack {
        listen: &'a str,
        username: &'a str,
        password: &'a str,
    },
}

#[derive(Clone)]
enum RealMode {
    Kernel,
    Netstack {
        listen: String,
        username: String,
        password: String,
    },
}

struct RealNetworkAdapter {
    name: String,
    mode: RealMode,
    with_wg_log: bool,
    uapi: UAPIClient,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    dns: Option<DNSManager>,
}

impl RealNetworkAdapter {
    fn new(
        name: String,
        mode: NetworkMode<'_>,
        with_wg_log: bool,
        #[cfg(any(target_os = "linux", target_os = "macos"))] dns_backup_filename: Option<String>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] use_vpn_dns: bool,
    ) -> Self {
        let real_mode = match mode {
            NetworkMode::Kernel => RealMode::Kernel,
            NetworkMode::Netstack {
                listen,
                username,
                password,
            } => RealMode::Netstack {
                listen: listen.to_string(),
                username: username.to_string(),
                password: password.to_string(),
            },
        };
        Self {
            uapi: UAPIClient { name: name.clone() },
            name,
            mode: real_mode,
            with_wg_log,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            dns: (use_vpn_dns && matches!(mode, NetworkMode::Kernel))
                .then(|| DNSManager::new(dns_backup_filename)),
        }
    }
}

impl NetworkAdapter for RealNetworkAdapter {
    fn start(&mut self, conf: &WgConf) -> Result<()> {
        match &self.mode {
            RealMode::Kernel => wg::start_wg_go(&self.name, conf.protocol, self.with_wg_log),
            RealMode::Netstack {
                listen,
                username,
                password,
            } => wg::start_wg_go_netstack(conf, listen, username, password, self.with_wg_log),
        }
    }

    fn configure<'a>(&'a mut self, conf: &'a WgConf) -> AdapterFuture<'a> {
        let mode = self.mode.clone();
        Box::pin(async move {
            match mode {
                RealMode::Kernel => self.uapi.config_wg(conf).await,
                RealMode::Netstack { .. } => self.uapi.config_wg_netstack(conf).await,
            }
        })
    }

    fn set_dns(&mut self, dns: &str) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(manager) = self.dns.as_mut() {
            return manager.set_dns(vec![dns], vec![]);
        }
        let _ = dns;
        Ok(())
    }

    fn restore_dns(&mut self) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(manager) = self.dns.as_ref() {
            return manager.restore_dns();
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        wg::stop_wg_go();
        Ok(())
    }

    fn health(&self) -> Result<WgHealth> {
        self.uapi.health(DEFAULT_HANDSHAKE_STALE_AFTER)
    }
}

pub struct NetworkSession {
    name: String,
    adapter: Box<dyn NetworkAdapter>,
    started: bool,
    closed: bool,
    dns_attempted: bool,
    dns_applied: bool,
}

impl NetworkSession {
    pub async fn acquire(
        name: impl Into<String>,
        conf: &WgConf,
        mode: NetworkMode<'_>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] use_vpn_dns: bool,
        #[cfg(any(target_os = "linux", target_os = "macos"))] dns_backup_filename: Option<String>,
        with_wg_log: bool,
    ) -> Result<Self> {
        let name = name.into();
        let adapter = RealNetworkAdapter::new(
            name.clone(),
            mode,
            with_wg_log,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            dns_backup_filename,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            use_vpn_dns,
        );
        Self::acquire_with_adapter(
            name,
            conf,
            Box::new(adapter),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            use_vpn_dns,
            &conf.dns,
        )
        .await
    }

    pub async fn acquire_with_adapter(
        name: impl Into<String>,
        conf: &WgConf,
        adapter: Box<dyn NetworkAdapter>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] use_vpn_dns: bool,
        dns: &str,
    ) -> Result<Self> {
        let mut session = Self {
            name: name.into(),
            adapter,
            started: false,
            closed: false,
            dns_attempted: false,
            dns_applied: false,
        };

        if let Err(error) = session.adapter.start(conf) {
            return Err(error).context("failed to acquire network resources");
        }
        session.started = true;

        if let Err(error) = session.adapter.configure(conf).await {
            let _ = session.adapter.stop();
            session.started = false;
            return Err(error).context("failed to configure network resources");
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if use_vpn_dns {
            session.dns_attempted = true;
            if let Err(error) = session.adapter.set_dns(dns) {
                let cleanup = session.close().await.err();
                return Err(anyhow!(
                    "failed to set VPN DNS: {error}; cleanup: {}",
                    cleanup
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                ));
            }
            session.dns_applied = true;
        }

        Ok(session)
    }

    pub fn health(&self) -> Result<WgHealth> {
        self.adapter.health()
    }

    pub async fn wait_until_ready(&self, deadline: Duration) -> Result<Duration> {
        let started = tokio::time::Instant::now();
        loop {
            match self.health()? {
                WgHealth::Healthy(age) => return Ok(age),
                WgHealth::NoHandshake | WgHealth::Stale(_) => {}
            }
            if started.elapsed() >= deadline {
                return Err(anyhow!(
                    "network session {} did not observe a handshake before the {}s deadline",
                    self.name,
                    deadline.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut errors = Vec::new();
        if self.started {
            if let Err(error) = self.adapter.stop() {
                errors.push(format!("network stop failed: {error}"));
            } else {
                self.started = false;
            }
        }
        if self.dns_attempted {
            if let Err(error) = self.adapter.restore_dns() {
                errors.push(format!("DNS restore failed: {error}"));
            } else {
                self.dns_attempted = false;
                self.dns_applied = false;
            }
        }
        if errors.is_empty() {
            self.closed = true;
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Drop for NetworkSession {
    fn drop(&mut self) {
        if self.started {
            if let Err(error) = self.adapter.stop() {
                log::error!(
                    "network session {} drop cleanup failed while stopping: {}",
                    self.name,
                    error
                );
            } else {
                self.started = false;
            }
        }
        if self.dns_attempted {
            if let Err(error) = self.adapter.restore_dns() {
                log::error!(
                    "network session {} drop cleanup failed while restoring DNS: {}",
                    self.name,
                    error
                );
            } else {
                self.dns_attempted = false;
                self.dns_applied = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn conf() -> WgConf {
        WgConf {
            address: "100.64.0.2/32".to_string(),
            address6: String::new(),
            peer_address: "198.51.100.1:443".to_string(),
            mtu: 1420,
            public_key: "public".to_string(),
            private_key: "private".to_string(),
            peer_key: "peer".to_string(),
            allowed_ips: vec!["10.0.0.0/8".to_string()],
            routes: vec!["10.0.0.0/8".to_string()],
            dns: "10.0.0.53".to_string(),
            protocol: 0,
        }
    }

    struct FakeAdapter {
        events: Arc<Mutex<Vec<&'static str>>>,
        fail_dns: bool,
        restore_failures: usize,
        health: WgHealth,
    }

    impl NetworkAdapter for FakeAdapter {
        fn start(&mut self, _conf: &WgConf) -> Result<()> {
            self.events.lock().unwrap().push("start");
            Ok(())
        }

        fn configure<'a>(&'a mut self, _conf: &'a WgConf) -> AdapterFuture<'a> {
            let events = Arc::clone(&self.events);
            Box::pin(async move {
                events.lock().unwrap().push("configure");
                Ok(())
            })
        }

        fn set_dns(&mut self, _dns: &str) -> Result<()> {
            self.events.lock().unwrap().push("set_dns");
            if self.fail_dns {
                Err(anyhow!("fake DNS set failed after partial apply"))
            } else {
                Ok(())
            }
        }

        fn restore_dns(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("restore_dns");
            if self.restore_failures > 0 {
                self.restore_failures -= 1;
                Err(anyhow!("fake DNS restore failed"))
            } else {
                Ok(())
            }
        }

        fn stop(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("stop");
            Ok(())
        }

        fn health(&self) -> Result<WgHealth> {
            Ok(self.health.clone())
        }
    }

    #[tokio::test]
    async fn dns_acquisition_failure_rolls_back_local_resources() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FakeAdapter {
            events: Arc::clone(&events),
            fail_dns: true,
            restore_failures: 0,
            health: WgHealth::Healthy(Duration::from_secs(1)),
        };
        let result = NetworkSession::acquire_with_adapter(
            "test",
            &conf(),
            Box::new(adapter),
            true,
            "10.0.0.53",
        )
        .await;
        let error = match result {
            Ok(_) => panic!("DNS acquisition should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("fake DNS set failed"));
        assert_eq!(
            *events.lock().unwrap(),
            vec!["start", "configure", "set_dns", "stop", "restore_dns"]
        );
    }

    #[tokio::test]
    async fn close_retries_dns_restore_after_partial_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FakeAdapter {
            events: Arc::clone(&events),
            fail_dns: false,
            restore_failures: 1,
            health: WgHealth::Healthy(Duration::from_secs(1)),
        };
        let mut session = NetworkSession::acquire_with_adapter(
            "test",
            &conf(),
            Box::new(adapter),
            true,
            "10.0.0.53",
        )
        .await
        .unwrap();
        assert!(session.close().await.is_err());
        assert!(!session.is_closed());
        session.close().await.unwrap();
        assert!(session.is_closed());
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "start",
                "configure",
                "set_dns",
                "stop",
                "restore_dns",
                "restore_dns"
            ]
        );
    }

    #[tokio::test]
    async fn never_handshaking_session_hits_a_deadline() {
        let adapter = FakeAdapter {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_dns: false,
            restore_failures: 0,
            health: WgHealth::NoHandshake,
        };
        let session = NetworkSession::acquire_with_adapter(
            "test",
            &conf(),
            Box::new(adapter),
            false,
            "10.0.0.53",
        )
        .await
        .unwrap();
        let error = session.wait_until_ready(Duration::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("handshake"));
    }
}

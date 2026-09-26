mod api;
mod client;
mod config;
mod dns;
mod managed_routes;
mod network_session;
mod qrcode;
mod resp;
mod runtime;
mod state;
mod template;
mod totp;
mod utils;
mod wg;

#[cfg(windows)]
use is_elevated;

use std::env;
use std::future::Future;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::process::exit;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use client::Client;
use config::{Config, WgConf};
use network_session::{NetworkMode, NetworkSession};
use runtime::{RecoveryPolicy, RetryDecision, RuntimeStore};
use tokio::sync::watch;

fn print_usage_and_exit(name: &str, conf: &str) {
    println!("usage:\n\t{} {}", name, conf);
    exit(1);
}

fn parse_arg() -> String {
    let mut conf_file = String::from("config.json");
    let mut args = env::args();
    let name = args.next().unwrap();
    match args.len() {
        0 => {}
        1 => {
            let arg = args.next().unwrap();
            match arg.as_str() {
                "-h" | "--help" => print_usage_and_exit(&name, &conf_file),
                _ => conf_file = arg,
            }
        }
        _ => print_usage_and_exit(&name, &conf_file),
    }
    conf_file
}

enum ReadOnlyCommand {
    Routes { config: String, write_cache: bool },
    RoutesStatus { config: String },
}

fn parse_read_only_command() -> Result<Option<ReadOnlyCommand>> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(None);
    };
    match command.as_str() {
        "routes" => {
            let config = args.next().context("routes requires a config path")?;
            let mut write_cache = false;
            for arg in args {
                if arg == "--write-cache" {
                    write_cache = true;
                } else {
                    return Err(anyhow!("unknown routes argument: {arg}"));
                }
            }
            Ok(Some(ReadOnlyCommand::Routes {
                config,
                write_cache,
            }))
        }
        "routes-status" => {
            let config = args
                .next()
                .context("routes-status requires a config path")?;
            if let Some(arg) = args.next() {
                return Err(anyhow!("unknown routes-status argument: {arg}"));
            }
            Ok(Some(ReadOnlyCommand::RoutesStatus { config }))
        }
        _ => Ok(None),
    }
}

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const ETIMEDOUT: i32 = 110;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{}", runtime::redact(&err.to_string()));
        exit(EPERM);
    }
}

async fn run() -> Result<()> {
    if let Some(command) = parse_read_only_command()? {
        let output = match command {
            ReadOnlyCommand::Routes {
                config,
                write_cache,
            } => managed_routes::routes_command(&config, write_cache).await?,
            ReadOnlyCommand::RoutesStatus { config } => {
                managed_routes::routes_status_command(&config).await?
            }
        };
        print!("{output}");
        return Ok(());
    }

    let mut logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    logger.format(|buf, record| {
        let message = runtime::redact(&record.args().to_string());
        writeln!(buf, "{} - {}", record.level(), message)
    });
    let _ = logger.try_init();
    print_version();

    let conf_file = parse_arg();
    let mut conf = Config::from_file(&conf_file)
        .await
        .context("failed to load config")?;
    let runtime_store = RuntimeStore::from_env();
    if let Some(store) = runtime_store.as_ref() {
        let _ = store.mark_initial();
    }
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let name = conf
        .interface_name
        .clone()
        .context("interface name missing in config")?;
    let socks5_listen = conf.socks5_listen.clone();
    let socks5_username = conf.socks5_username.clone().unwrap_or_default();
    let socks5_password = conf.socks5_password.clone().unwrap_or_default();
    let netstack_mode = socks5_listen.is_some();
    let socks5_dns_tcp = conf.socks5_dns_tcp.unwrap_or(false);

    if !netstack_mode {
        check_privilege();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let use_vpn_dns = conf.use_vpn_dns.unwrap_or(false);
    #[cfg(target_os = "macos")]
    let dns_backup_filename = conf.dns_backup_filename.clone().map(|filename| {
        let path = Path::new(&filename);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(&conf_file)
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        }
        .to_string_lossy()
        .into_owned()
    });
    #[cfg(target_os = "linux")]
    let dns_backup_filename = conf.dns_backup_filename.clone();

    let mut recovery_policy = RecoveryPolicy::new();
    while conf.server.is_none() {
        let result = runtime::run_bounded(
            client::get_company_url(conf.company_name.as_str()),
            Duration::from_secs(10),
            &mut shutdown_rx,
        )
        .await;
        let error = match result {
            runtime::OperationOutcome::Completed(Ok(resp)) => {
                conf.server = Some(resp.domain);
                break;
            }
            runtime::OperationOutcome::Completed(Err(error)) => error,
            runtime::OperationOutcome::TimedOut => {
                anyhow::Error::new(api::ClientFailure::transport("company_match deadline"))
            }
            runtime::OperationOutcome::Cancelled => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_stopped();
                }
                return Ok(());
            }
        };
        match recovery_policy.on_failure(api::classify_error(&error)) {
            RetryDecision::Retry { attempt, delay } => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store
                        .mark_degraded("company discovery temporarily unavailable; retry pending");
                }
                log::warn!(
                    "company discovery retry {attempt}: {}",
                    runtime::redact(&error.to_string())
                );
                if wait_for_retry(delay, &mut shutdown_rx).await {
                    if let Some(store) = runtime_store.as_ref() {
                        let _ = store.mark_stopped();
                    }
                    return Ok(());
                }
            }
            _ => return Err(terminal_failure_error(runtime_store.as_ref(), error)),
        }
    }

    let with_wg_log = conf.debug_wg.unwrap_or_default();
    let platform = conf.platform.clone();
    let mut client = Client::new(conf).context("failed to initialize client")?;
    recovery_policy.reset_after_ready();
    let handshake_deadline = duration_from_env("CORPLINK_HANDSHAKE_DEADLINE_SECS", 60);

    loop {
        if *shutdown_rx.borrow() {
            if let Some(store) = runtime_store.as_ref() {
                let _ = store.mark_stopping();
                let _ = store.mark_stopped();
            }
            return Ok(());
        }
        if client.need_login() {
            if let Some(store) = runtime_store.as_ref() {
                let _ = store.mark_authenticating();
            }
            log::info!("not login yet, try to login");
            let login_result = tokio::select! {
                result = client.login() => result,
                _ = shutdown_rx.changed() => {
                    if let Some(store) = runtime_store.as_ref() {
                        let _ = store.mark_stopping();
                        let _ = store.mark_stopped();
                    }
                    return Ok(());
                }
            };
            if let Err(error) = login_result {
                let kind = api::classify_error(&error);
                match recovery_policy.on_failure(kind) {
                    RetryDecision::Retry { attempt, delay } => {
                        if let Some(store) = runtime_store.as_ref() {
                            let _ =
                                store.mark_degraded("temporary authentication transport failure");
                        }
                        log::warn!(
                            "authentication retry {attempt} after {}s: {}",
                            delay.as_secs(),
                            runtime::redact(&error.to_string())
                        );
                        if wait_for_retry(delay, &mut shutdown_rx).await {
                            if let Some(store) = runtime_store.as_ref() {
                                let _ = store.mark_stopping();
                                let _ = store.mark_stopped();
                            }
                            return Ok(());
                        }
                        continue;
                    }
                    RetryDecision::Reauthenticate => continue,
                    RetryDecision::Fail => {}
                }
                return Err(terminal_failure(
                    runtime_store.as_ref(),
                    &format!(
                        "authentication failed: {}",
                        runtime::redact(&error.to_string())
                    ),
                ));
            }
            log::info!("login success");
        }

        if let Some(store) = runtime_store.as_ref() {
            let _ = store.mark_initial();
        }
        log::info!("try to connect");
        let connect_result = tokio::select! {
            result = client.connect_vpn() => result,
            _ = shutdown_rx.changed() => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_stopping();
                    let _ = store.mark_stopped();
                }
                return Ok(());
            }
        };
        let wg_conf = match connect_result {
            Ok(conf) => conf,
            Err(error) => {
                let kind = api::classify_error(&error);
                match recovery_policy.on_failure(kind) {
                    RetryDecision::Reauthenticate => {
                        log::warn!("authentication session expired; retrying login");
                        continue;
                    }
                    RetryDecision::Retry { attempt, delay } => {
                        if let Some(store) = runtime_store.as_ref() {
                            let _ = store.mark_degraded("temporary VPN connection failure");
                        }
                        log::warn!(
                            "connection retry {attempt} after {}s: {}",
                            delay.as_secs(),
                            runtime::redact(&error.to_string())
                        );
                        if wait_for_retry(delay, &mut shutdown_rx).await {
                            if let Some(store) = runtime_store.as_ref() {
                                let _ = store.mark_stopping();
                                let _ = store.mark_stopped();
                            }
                            return Ok(());
                        }
                        continue;
                    }
                    RetryDecision::Fail => {}
                }
                return Err(terminal_failure(
                    runtime_store.as_ref(),
                    &format!(
                        "VPN connection failed: {}",
                        runtime::redact(&error.to_string())
                    ),
                ));
            }
        };

        let mode = match socks5_listen.as_deref() {
            Some(listen) => NetworkMode::Netstack {
                listen,
                username: &socks5_username,
                password: &socks5_password,
                dns_tcp: socks5_dns_tcp,
            },
            None => NetworkMode::Kernel,
        };
        let acquisition_result = tokio::select! {
            result = NetworkSession::acquire(
                name.clone(),
                &wg_conf,
                mode,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                use_vpn_dns,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                dns_backup_filename.clone(),
                with_wg_log,
            ) => result,
            _ = shutdown_rx.changed() => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_stopping();
                    let _ = store.mark_stopped();
                }
                return Ok(());
            }
        };
        let mut session = match acquisition_result {
            Ok(session) => session,
            Err(error) => {
                let message = runtime::redact(&error.to_string());
                let remote_error = disconnect_remote(&mut client, &wg_conf, platform.as_deref())
                    .await
                    .err();
                return Err(terminal_failure(
                    runtime_store.as_ref(),
                    &format!(
                        "local network acquisition failed: {message}; cleanup={}",
                        format_cleanup_errors(None, remote_error.as_ref())
                    ),
                ));
            }
        };

        let generation = runtime_store
            .as_ref()
            .map(|store| store.generation().to_string())
            .unwrap_or_else(|| "foreground".to_string());
        if let Err(error) = client
            .mark_managed_routes_applied(&wg_conf, &generation, std::process::id())
            .await
        {
            log::warn!(
                "managed routes were active but their status record could not be published: {}",
                runtime::redact(&error.to_string())
            );
        }

        if let Some(listen) = socks5_listen.as_deref() {
            if socks5_username.is_empty() {
                log::info!("socks5 proxy ready at {listen} (no auth)");
            } else {
                log::info!("socks5 proxy ready at {listen} (username/password auth required)");
            }
        }

        match supervise_session(
            &mut session,
            handshake_deadline,
            &mut shutdown_rx,
            runtime_store.as_ref(),
            &mut recovery_policy,
            Duration::from_secs(5),
            || disconnect_remote(&mut client, &wg_conf, platform.as_deref()),
        )
        .await?
        {
            SessionOutcome::Retry => continue,
            SessionOutcome::Stopped => {
                log::info!("reach exit");
                return Ok(());
            }
        }
    }
}

fn terminal_failure(store: Option<&RuntimeStore>, reason: &str) -> anyhow::Error {
    terminal_failure_error(store, anyhow!(runtime::redact(reason)))
}

fn terminal_failure_error(store: Option<&RuntimeStore>, error: anyhow::Error) -> anyhow::Error {
    let safe_reason = runtime::redact(&error.to_string());
    if let Some(store) = store {
        let _ = store.mark_failed(&safe_reason);
    }
    error
}

async fn disconnect_remote(
    client: &mut Client,
    wg_conf: &WgConf,
    platform: Option<&str>,
) -> Result<()> {
    let mut errors = Vec::new();
    match tokio::time::timeout(Duration::from_secs(15), client.disconnect_vpn(wg_conf)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(format!(
            "remote disconnect: {}",
            runtime::redact(&error.to_string())
        )),
        Err(_) => errors.push("remote disconnect deadline exceeded".to_string()),
    }
    if platform == Some(config::PLATFORM_CORPLINK_V1) {
        match tokio::time::timeout(Duration::from_secs(15), client.logout()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(format!(
                "remote logout: {}",
                runtime::redact(&error.to_string())
            )),
            Err(_) => errors.push("remote logout deadline exceeded".to_string()),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOutcome {
    Retry,
    Stopped,
}

enum SessionEnd {
    Stopped,
    Failed {
        context: &'static str,
        message: String,
    },
}

async fn supervise_session<F, Fut>(
    session: &mut NetworkSession,
    handshake_deadline: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
    runtime_store: Option<&RuntimeStore>,
    recovery_policy: &mut RecoveryPolicy,
    health_interval: Duration,
    disconnect: F,
) -> Result<SessionOutcome>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let handshake_result = tokio::select! {
        biased;
        _ = shutdown_rx.changed() => None,
        result = session.wait_until_ready(handshake_deadline) => Some(result),
    };
    let end = match handshake_result {
        None => SessionEnd::Stopped,
        Some(Ok(handshake_age)) => {
            if let Some(store) = runtime_store {
                let _ = store.mark_ready(handshake_age);
            }
            recovery_policy.reset_after_ready();

            match tokio::select! {
                biased;
                _ = shutdown_rx.changed() => None,
                result = monitor_network(session, runtime_store, health_interval) => Some(result),
            } {
                None => SessionEnd::Stopped,
                Some(Ok(())) => unreachable!("health monitor only returns after a failure"),
                Some(Err(error)) => SessionEnd::Failed {
                    context: "connection health failed",
                    message: runtime::redact(&error.to_string()),
                },
            }
        }
        Some(Err(error)) => SessionEnd::Failed {
            context: "VPN never became ready",
            message: runtime::redact(&error.to_string()),
        },
    };

    if matches!(end, SessionEnd::Stopped) {
        if let Some(store) = runtime_store {
            let _ = store.mark_stopping();
        }
    }
    // Local resources must be released before remote disconnect. A retry is
    // safe only after this first step succeeds.
    let local_error = session.close().await.err();
    let remote_error = disconnect().await.err();
    let cleanup = format_cleanup_errors(local_error.as_ref(), remote_error.as_ref());

    match end {
        SessionEnd::Stopped => {
            if cleanup != "none" {
                return Err(terminal_failure(
                    runtime_store,
                    &format!("stop cleanup incomplete: {cleanup}"),
                ));
            }
            if let Some(store) = runtime_store {
                let _ = store.mark_stopped();
            }
            Ok(SessionOutcome::Stopped)
        }
        SessionEnd::Failed { context, message } => {
            if local_error.is_some() {
                return Err(terminal_failure(
                    runtime_store,
                    &format!("{context}; local cleanup failed: {cleanup}"),
                ));
            }

            match recovery_policy.on_transient_failure() {
                RetryDecision::Retry { attempt, delay } => {
                    if let Some(store) = runtime_store {
                        let _ = store.mark_degraded(&format!("{context}; retry pending"));
                    }
                    log::warn!(
                        "{context}; retry {attempt} after {}s: {message}; cleanup={cleanup}",
                        delay.as_secs()
                    );
                    if wait_for_retry(delay, shutdown_rx).await {
                        if let Some(store) = runtime_store {
                            let _ = store.mark_stopping();
                        }
                        if cleanup != "none" {
                            return Err(terminal_failure(
                                runtime_store,
                                &format!("stop cleanup incomplete: {cleanup}"),
                            ));
                        }
                        if let Some(store) = runtime_store {
                            let _ = store.mark_stopped();
                        }
                        return Ok(SessionOutcome::Stopped);
                    }
                    Ok(SessionOutcome::Retry)
                }
                RetryDecision::Reauthenticate | RetryDecision::Fail => Err(terminal_failure(
                    runtime_store,
                    &format!("{context}: {message}; cleanup={cleanup}"),
                )),
            }
        }
    }
}

fn format_cleanup_errors(local: Option<&anyhow::Error>, remote: Option<&anyhow::Error>) -> String {
    let mut errors = Vec::new();
    if let Some(error) = local {
        errors.push(format!("local: {}", runtime::redact(&error.to_string())));
    }
    if let Some(error) = remote {
        errors.push(runtime::redact(&error.to_string()));
    }
    if errors.is_empty() {
        "none".to_string()
    } else {
        errors.join("; ")
    }
}

async fn monitor_network(
    session: &mut NetworkSession,
    runtime_store: Option<&RuntimeStore>,
    interval: Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(interval).await;
        match session.health().await? {
            wg::WgHealth::Healthy(age) => {
                log::info!("VPN health is current; handshake age={}s", age.as_secs());
                if let Some(store) = runtime_store {
                    let _ = store.mark_health(age);
                }
            }
            wg::WgHealth::NoHandshake => return Err(anyhow!("wireguard handshake was lost")),
            wg::WgHealth::Stale(age) => {
                return Err(anyhow!(
                    "wireguard handshake is stale at {}s",
                    age.as_secs()
                ))
            }
        }
    }
}

fn duration_from_env(name: &str, default_secs: u64) -> Duration {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

async fn wait_for_retry(delay: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown_rx.changed() => true,
    }
}

// Resolve on ctrl+c or SIGTERM so local resources are closed before remote logout.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("failed to install SIGTERM handler: {}", e);
                None
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    log::warn!("failed to receive signal: {}", e);
                }
                log::info!("ctrl+c received");
            }
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => log::info!("SIGTERM received"),
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            log::warn!("failed to receive signal: {}", e);
        }
        log::info!("ctrl+c received");
    }
}

fn check_privilege() {
    #[cfg(unix)]
    match sudo::escalate_if_needed() {
        Ok(_) => {}
        Err(_) => {
            log::error!("please run as root");
            exit(EPERM);
        }
    }

    #[cfg(windows)]
    if !is_elevated::is_elevated() {
        log::error!("please run as administrator");
        exit(EPERM);
    }
}

fn print_version() {
    log::info!(
        "running {}@{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use network_session::AdapterFuture;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeAdapter {
        events: Arc<Mutex<Vec<&'static str>>>,
        stop_failures: usize,
        health: Arc<Mutex<Vec<wg::WgHealth>>>,
        block_health: bool,
    }

    impl network_session::NetworkAdapter for FakeAdapter {
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
            Ok(())
        }

        fn restore_dns(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("restore_dns");
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("stop");
            if self.stop_failures > 0 {
                self.stop_failures -= 1;
                Err(anyhow!("fake network stop failed"))
            } else {
                Ok(())
            }
        }

        fn health(&self) -> network_session::HealthFuture {
            let mut health = self.health.lock().unwrap();
            if self.block_health && health.len() == 1 {
                self.events.lock().unwrap().push("health_pending");
                return Box::pin(std::future::pending());
            }
            let value = if health.len() > 1 {
                health.remove(0)
            } else {
                health.first().cloned().unwrap_or(wg::WgHealth::NoHandshake)
            };
            Box::pin(async move { Ok(value) })
        }
    }

    fn test_conf() -> WgConf {
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

    fn test_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "corplink-main-{name}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    async fn session_with_stop_failures(
        stop_failures: usize,
        health: Vec<wg::WgHealth>,
    ) -> (NetworkSession, Arc<Mutex<Vec<&'static str>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FakeAdapter {
            events: Arc::clone(&events),
            stop_failures,
            health: Arc::new(Mutex::new(health)),
            block_health: false,
        };
        let conf = test_conf();
        let session = NetworkSession::acquire_with_adapter(
            "test",
            &conf,
            Box::new(adapter),
            false,
            "10.0.0.53",
        )
        .await
        .unwrap();
        (session, events)
    }

    #[tokio::test]
    async fn handshake_cleanup_failure_stops_supervision_before_retry() {
        let (mut session, events) =
            session_with_stop_failures(1, vec![wg::WgHealth::NoHandshake]).await;
        let (_sender, mut shutdown_rx) = watch::channel(false);
        let state_path = test_path("handshake-cleanup-failure");
        let store = RuntimeStore::new(&state_path, "generation-handshake-failure");
        let mut policy = RecoveryPolicy::new();
        let remote_calls = Arc::new(Mutex::new(0_u32));
        let remote_calls_for_cleanup = Arc::clone(&remote_calls);
        let events_for_cleanup = Arc::clone(&events);

        let result = supervise_session(
            &mut session,
            Duration::ZERO,
            &mut shutdown_rx,
            Some(&store),
            &mut policy,
            Duration::ZERO,
            move || async move {
                *remote_calls_for_cleanup.lock().unwrap() += 1;
                events_for_cleanup.lock().unwrap().push("remote");
                Ok(())
            },
        )
        .await;

        assert!(matches!(result, Err(_)));
        assert_eq!(*remote_calls.lock().unwrap(), 1);
        assert!(!session.is_closed());
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["start", "configure", "stop", "remote"]
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["phase"], "failed");
        assert_eq!(state["intent"], "failed");
        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn health_cleanup_failure_stops_supervision_before_retry() {
        let (mut session, events) = session_with_stop_failures(
            1,
            vec![
                wg::WgHealth::Healthy(Duration::from_secs(1)),
                wg::WgHealth::Stale(Duration::from_secs(301)),
            ],
        )
        .await;
        let (_sender, mut shutdown_rx) = watch::channel(false);
        let state_path = test_path("health-cleanup-failure");
        let store = RuntimeStore::new(&state_path, "generation-health-failure");
        let mut policy = RecoveryPolicy::new();
        let remote_calls = Arc::new(Mutex::new(0_u32));
        let remote_calls_for_cleanup = Arc::clone(&remote_calls);
        let events_for_cleanup = Arc::clone(&events);

        let result = supervise_session(
            &mut session,
            Duration::ZERO,
            &mut shutdown_rx,
            Some(&store),
            &mut policy,
            Duration::ZERO,
            move || async move {
                *remote_calls_for_cleanup.lock().unwrap() += 1;
                events_for_cleanup.lock().unwrap().push("remote");
                Ok(())
            },
        )
        .await;

        assert!(matches!(result, Err(_)));
        assert_eq!(*remote_calls.lock().unwrap(), 1);
        assert!(!session.is_closed());
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["start", "configure", "stop", "remote"]
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["phase"], "failed");
        assert_eq!(state["intent"], "failed");
        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn stop_cleanup_failure_cannot_publish_stopped() {
        let (mut session, _events) =
            session_with_stop_failures(1, vec![wg::WgHealth::NoHandshake]).await;
        let state_path = test_path("stop-failure");
        let store = RuntimeStore::new(&state_path, "generation-stop-failure");
        let (sender, mut shutdown_rx) = watch::channel(false);
        sender.send(true).unwrap();
        let mut policy = RecoveryPolicy::new();

        let result = supervise_session(
            &mut session,
            Duration::from_secs(30),
            &mut shutdown_rx,
            Some(&store),
            &mut policy,
            Duration::ZERO,
            || async { Ok(()) },
        )
        .await;
        assert!(matches!(result, Err(_)));
        assert!(!session.is_closed());
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["phase"], "failed");
        assert_eq!(state["intent"], "failed");
        assert_ne!(state["phase"], "stopped");
        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn successful_handshake_cleanup_returns_retry() {
        let (mut session, events) =
            session_with_stop_failures(0, vec![wg::WgHealth::NoHandshake]).await;
        let (_sender, mut shutdown_rx) = watch::channel(false);
        let state_path = test_path("handshake-retry");
        let store = RuntimeStore::new(&state_path, "generation-handshake-retry");
        let mut policy = RecoveryPolicy::new();
        let events_for_cleanup = Arc::clone(&events);

        let result = supervise_session(
            &mut session,
            Duration::ZERO,
            &mut shutdown_rx,
            Some(&store),
            &mut policy,
            Duration::ZERO,
            move || async move {
                events_for_cleanup.lock().unwrap().push("remote");
                Ok(())
            },
        )
        .await;

        assert!(matches!(result, Ok(SessionOutcome::Retry)));
        assert!(session.is_closed());
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["start", "configure", "stop", "remote"]
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["phase"], "degraded");
        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn successful_shutdown_cleanup_returns_stopped() {
        let (mut session, events) =
            session_with_stop_failures(0, vec![wg::WgHealth::NoHandshake]).await;
        let state_path = test_path("shutdown-stopped");
        let store = RuntimeStore::new(&state_path, "generation-shutdown-stopped");
        let (sender, mut shutdown_rx) = watch::channel(false);
        sender.send(true).unwrap();
        let mut policy = RecoveryPolicy::new();
        let events_for_cleanup = Arc::clone(&events);

        let result = supervise_session(
            &mut session,
            Duration::from_secs(30),
            &mut shutdown_rx,
            Some(&store),
            &mut policy,
            Duration::ZERO,
            move || async move {
                events_for_cleanup.lock().unwrap().push("remote");
                Ok(())
            },
        )
        .await;

        assert!(matches!(result, Ok(SessionOutcome::Stopped)));
        assert!(session.is_closed());
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["start", "configure", "stop", "remote"]
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["phase"], "stopped");
        assert_eq!(state["intent"], "stopped");
        let _ = fs::remove_file(state_path);
    }
    #[tokio::test]
    async fn stop_can_cancel_a_pending_health_read() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = FakeAdapter {
            events: Arc::clone(&events),
            stop_failures: 0,
            block_health: true,
            health: Arc::new(Mutex::new(vec![wg::WgHealth::Healthy(Duration::ZERO); 2])),
        };
        let mut session = NetworkSession::acquire_with_adapter(
            "test",
            &test_conf(),
            Box::new(adapter),
            false,
            "10.0.0.53",
        )
        .await
        .unwrap();
        let (sender, mut receiver) = watch::channel(false);
        let stop_events = Arc::clone(&events);
        let stop = tokio::spawn(async move {
            while !stop_events.lock().unwrap().contains(&"health_pending") {
                tokio::task::yield_now().await;
            }
            sender.send(true).unwrap();
        });
        let mut policy = RecoveryPolicy::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            supervise_session(
                &mut session,
                Duration::ZERO,
                &mut receiver,
                None,
                &mut policy,
                Duration::ZERO,
                || async { Ok(()) },
            ),
        )
        .await
        .expect("health read blocked stop")
        .unwrap();
        assert!(matches!(outcome, SessionOutcome::Stopped));
        assert!(session.is_closed());
        stop.await.unwrap();
        assert!(events.lock().unwrap().contains(&"stop"));
    }
}

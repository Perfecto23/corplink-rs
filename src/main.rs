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

    if conf.server.is_none() {
        let discovery = async {
            client::get_company_url(conf.company_name.as_str())
                .await
                .with_context(|| {
                    format!(
                        "failed to fetch company server from company name {}",
                        conf.company_name
                    )
                })
        };
        match runtime::run_bounded(discovery, Duration::from_secs(10), &mut shutdown_rx).await {
            runtime::OperationOutcome::Completed(result) => {
                let resp = match result {
                    Ok(resp) => resp,
                    Err(error) => {
                        return Err(terminal_failure_error(runtime_store.as_ref(), error));
                    }
                };
                log::info!(
                    "company name is {}(zh)/{}(en) server is {}",
                    resp.zh_name,
                    resp.en_name,
                    resp.domain
                );
                // Keep discovery in memory; user configuration is not runtime state.
                conf.server = Some(resp.domain);
            }
            runtime::OperationOutcome::Cancelled => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_stopping();
                    let _ = store.mark_stopped();
                }
                return Ok(());
            }
            runtime::OperationOutcome::TimedOut => {
                return Err(terminal_failure_error(
                    runtime_store.as_ref(),
                    anyhow::Error::new(api::ClientFailure::transport("company_match deadline")),
                ));
            }
        }
    }

    let with_wg_log = conf.debug_wg.unwrap_or_default();
    let platform = conf.platform.clone();
    let mut client = Client::new(conf).context("failed to initialize client")?;
    let mut recovery_policy = RecoveryPolicy::new(3);
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
                            "authentication retry {attempt}/3 after {}s: {}",
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
                            "connection retry {attempt}/3 after {}s: {}",
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
                        format_cleanup_errors(None, remote_error)
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

        let handshake_result = tokio::select! {
            result = session.wait_until_ready(handshake_deadline) => Some(result),
            _ = shutdown_rx.changed() => None,
        };
        let handshake_age = match handshake_result {
            None => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_stopping();
                }
                let local_error = session.close().await.err();
                let remote_error = disconnect_remote(&mut client, &wg_conf, platform.as_deref())
                    .await
                    .err();
                let cleanup = format_cleanup_errors(local_error, remote_error);
                if let Some(store) = runtime_store.as_ref() {
                    if cleanup == "none" {
                        let _ = store.mark_stopped();
                    } else {
                        let _ = store.mark_failed(&format!("stop cleanup incomplete: {cleanup}"));
                    }
                }
                if cleanup != "none" {
                    return Err(anyhow!("stop cleanup incomplete: {cleanup}"));
                }
                return Ok(());
            }
            Some(result) => match result {
                Ok(age) => age,
                Err(error) => {
                    let message = runtime::redact(&error.to_string());
                    let local_error = session.close().await.err();
                    let remote_error =
                        disconnect_remote(&mut client, &wg_conf, platform.as_deref())
                            .await
                            .err();
                    let cleanup = format_cleanup_errors(local_error, remote_error);
                    match recovery_policy.on_transient_failure() {
                        RetryDecision::Retry { attempt, delay } => {
                            if let Some(store) = runtime_store.as_ref() {
                                let _ = store
                                    .mark_degraded("handshake deadline exceeded; retry pending");
                            }
                            log::warn!(
                            "handshake retry {attempt}/3 after {}s: {message}; cleanup={cleanup}",
                            delay.as_secs()
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
                        RetryDecision::Reauthenticate | RetryDecision::Fail => {}
                    }
                    return Err(terminal_failure(
                        runtime_store.as_ref(),
                        &format!("VPN never became ready: {message}; cleanup={cleanup}"),
                    ));
                }
            },
        };

        if let Some(store) = runtime_store.as_ref() {
            let _ = store.mark_ready(handshake_age);
        }
        recovery_policy.reset_after_ready();

        let health_result = tokio::select! {
            _ = shutdown_rx.changed() => None,
            result = monitor_network(&session, runtime_store.as_ref()) => Some(result),
        };

        if health_result.is_none() {
            if let Some(store) = runtime_store.as_ref() {
                let _ = store.mark_stopping();
            }
            let local_error = session.close().await.err();
            let remote_error = disconnect_remote(&mut client, &wg_conf, platform.as_deref())
                .await
                .err();
            let cleanup = format_cleanup_errors(local_error, remote_error);
            if let Some(store) = runtime_store.as_ref() {
                if cleanup == "none" {
                    let _ = store.mark_stopped();
                } else {
                    let _ = store.mark_failed(&format!("stop cleanup incomplete: {cleanup}"));
                }
            }
            if cleanup != "none" {
                return Err(anyhow!("stop cleanup incomplete: {cleanup}"));
            }
            log::info!("reach exit");
            return Ok(());
        }

        let health_error = match health_result.expect("health result is present") {
            Ok(()) => unreachable!("health monitor only returns after a failure"),
            Err(error) => error,
        };
        let local_error = session.close().await.err();
        let remote_error = disconnect_remote(&mut client, &wg_conf, platform.as_deref())
            .await
            .err();
        let cleanup = format_cleanup_errors(local_error, remote_error);
        let message = runtime::redact(&health_error.to_string());
        match recovery_policy.on_transient_failure() {
            RetryDecision::Retry { attempt, delay } => {
                if let Some(store) = runtime_store.as_ref() {
                    let _ = store.mark_degraded(&format!("connection health failed: {message}"));
                }
                log::warn!(
                    "connection health retry {attempt}/3 after {}s: {message}; cleanup={cleanup}",
                    delay.as_secs()
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
            RetryDecision::Reauthenticate | RetryDecision::Fail => {}
        }
        return Err(terminal_failure(
            runtime_store.as_ref(),
            &format!("connection health failed: {message}; cleanup={cleanup}"),
        ));
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

fn format_cleanup_errors(local: Option<anyhow::Error>, remote: Option<anyhow::Error>) -> String {
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
    session: &NetworkSession,
    runtime_store: Option<&RuntimeStore>,
) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        match session.health()? {
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

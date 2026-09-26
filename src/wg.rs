use std::ffi::{c_void, CStr, CString};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::{config, utils};

#[allow(clippy::all)]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals
)]
mod libwg {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

fn start_wg(log_level: i32, protocol: i32, interface_name: &str) -> Result<i32> {
    let input_cstring =
        CString::new(interface_name.as_bytes()).context("buff contains null character")?;
    unsafe { Ok(libwg::startWg(log_level, protocol, input_cstring.as_ptr())) }
}

fn stop_wg() {
    unsafe {
        libwg::stopWg();
    }
}

fn start_wg_netstack(
    log_level: i32,
    protocol: i32,
    addresses: &str,
    dns: &str,
    socks_listen: &str,
    socks_user: &str,
    socks_pass: &str,
    mtu: i32,
    dns_tcp: bool,
) -> Result<i32> {
    let c_addresses = CString::new(addresses).context("addresses contains null character")?;
    let c_dns = CString::new(dns).context("dns contains null character")?;
    let c_socks = CString::new(socks_listen).context("socks_listen contains null character")?;
    let c_user = CString::new(socks_user).context("socks_user contains null character")?;
    let c_pass = CString::new(socks_pass).context("socks_pass contains null character")?;
    unsafe {
        Ok(libwg::startWgNetstack(
            log_level,
            protocol,
            c_addresses.as_ptr(),
            c_dns.as_ptr(),
            c_socks.as_ptr(),
            c_user.as_ptr(),
            c_pass.as_ptr(),
            mtu,
            i32::from(dns_tcp),
        ))
    }
}

fn uapi(buff: &[u8]) -> Result<Vec<u8>> {
    let input_cstring = CString::new(buff).context("buff contains null character")?;
    unsafe {
        let result_ptr = libwg::uapi(input_cstring.as_ptr());
        if result_ptr.is_null() {
            return Err(anyhow!("libwg::uapi() returned null pointer"));
        }
        let result = CStr::from_ptr(result_ptr).to_bytes().to_vec();
        libc::free(result_ptr as *mut c_void);
        Ok(result)
    }
}

pub fn stop_wg_go() {
    stop_wg();
}

pub fn start_wg_go(name: &str, protocol: i32, with_log: bool) -> Result<()> {
    log::info!("start wg-corplink");
    let mut log_level = libwg::LogLevelError;
    if with_log {
        log_level = libwg::LogLevelVerbose;
    }
    let ret = start_wg(log_level, protocol, name)?;
    if !matches!(ret, 0) {
        return Err(anyhow!("start_wg returned non-zero code: {ret}"));
    }
    Ok(())
}

// start wg-corplink in userspace netstack mode and expose a SOCKS5 proxy.
// no kernel TUN device, no system routes/dns and no root are needed.
pub fn start_wg_go_netstack(
    conf: &config::WgConf,
    socks_listen: &str,
    socks_user: &str,
    socks_pass: &str,
    dns_tcp: bool,
    with_log: bool,
) -> Result<()> {
    log::info!("start wg-corplink in netstack/socks5 mode");
    let log_level = if with_log {
        libwg::LogLevelVerbose
    } else {
        libwg::LogLevelError
    };
    let mut addrs = vec![conf.address.clone()];
    if !conf.address6.is_empty() {
        addrs.push(conf.address6.clone());
    }
    let addresses = addrs.join(",");
    let ret = start_wg_netstack(
        log_level,
        conf.protocol,
        &addresses,
        &conf.dns,
        socks_listen,
        socks_user,
        socks_pass,
        conf.mtu as i32,
        dns_tcp,
    )?;
    if !matches!(ret, 0) {
        return Err(anyhow!("start_wg_netstack returned non-zero code: {ret}"));
    }
    Ok(())
}

pub struct UAPIClient {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WgHealth {
    Healthy(Duration),
    NoHandshake,
    Stale(Duration),
}

pub fn parse_wg_health(data: &[u8], stale_after: Duration) -> Result<WgHealth> {
    let response =
        String::from_utf8(data.to_vec()).context("failed to decode wireguard UAPI response")?;
    let mut handshake = None;
    for line in response.lines() {
        if let Some(value) = line.strip_prefix("last_handshake_time_sec=") {
            handshake = Some(
                value
                    .parse::<u64>()
                    .with_context(|| format!("invalid last_handshake_time_sec value {value:?}"))?,
            );
        }
        if let Some(errno) = line.strip_prefix("errno=") {
            if errno != "0" {
                return Err(anyhow!("wireguard UAPI returned errno={errno}"));
            }
        }
    }
    let timestamp = handshake.context("wireguard UAPI did not return last_handshake_time_sec")?;
    if timestamp == 0 {
        return Ok(WgHealth::NoHandshake);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let age = now
        .checked_sub(Duration::from_secs(timestamp))
        .unwrap_or_default();
    if age <= stale_after {
        Ok(WgHealth::Healthy(age))
    } else {
        Ok(WgHealth::Stale(age))
    }
}

impl UAPIClient {
    pub async fn config_wg(&mut self, conf: &config::WgConf) -> Result<()> {
        let mut buff = String::from("set=1\n");
        // standard wg-go uapi operations
        // see https://www.wireguard.com/xplatform/#configuration-protocol
        let private_key = utils::b64_decode_to_hex(&conf.private_key)?;
        let public_key = utils::b64_decode_to_hex(&conf.peer_key)?;
        buff.push_str(format!("private_key={private_key}\n").as_str());
        buff.push_str("replace_peers=true\n".to_string().as_str());
        buff.push_str(format!("public_key={public_key}\n").as_str());
        buff.push_str("replace_allowed_ips=true\n".to_string().as_str());
        buff.push_str(format!("endpoint={}\n", conf.peer_address).as_str());
        buff.push_str("persistent_keepalive_interval=10\n".to_string().as_str());
        for allowed_ip in &conf.allowed_ips {
            if allowed_ip.contains("/") {
                buff.push_str(format!("allowed_ip={allowed_ip}\n").as_str());
            } else {
                buff.push_str(format!("allowed_ip={allowed_ip}/32\n").as_str());
            }
        }

        // wg-corplink uapi operations
        let addr = &conf.address;
        let addr6 = &conf.address6;
        let mtu = conf.mtu;
        buff.push_str(format!("address={addr}\n").as_str());
        if !addr6.is_empty() {
            buff.push_str(format!("address={addr6}\n").as_str());
        }
        buff.push_str(format!("mtu={mtu}\n").as_str());
        buff.push_str("up=true\n".to_string().as_str());
        for route in &conf.routes {
            if route.contains("/") {
                buff.push_str(format!("route={route}\n").as_str());
            } else {
                let prefix_len = if route.contains(":") { 128 } else { 32 };
                buff.push_str(format!("route={route}/{prefix_len}\n").as_str());
            }
        }
        // end operation

        buff.push('\n');
        log::info!("send config to uapi");
        let data = uapi(buff.as_bytes()).context("call uapi")?;
        let s = String::from_utf8(data).context("failed to decode uapi response")?;
        if !s.contains("errno=0") {
            return Err(anyhow!("uapi returns unexpected result: {}", s));
        }
        Ok(())
    }

    // configure wg for netstack mode. only the standard wg-go uapi operations
    // are sent: the interface address/mtu and the device "up" state are handled
    // by netstack at creation time, and there are no system routes to install.
    pub async fn config_wg_netstack(&mut self, conf: &config::WgConf) -> Result<()> {
        let mut buff = String::from("set=1\n");
        let private_key = utils::b64_decode_to_hex(&conf.private_key)?;
        let public_key = utils::b64_decode_to_hex(&conf.peer_key)?;
        buff.push_str(format!("private_key={private_key}\n").as_str());
        buff.push_str("replace_peers=true\n");
        buff.push_str(format!("public_key={public_key}\n").as_str());
        buff.push_str("replace_allowed_ips=true\n");
        buff.push_str(format!("endpoint={}\n", conf.peer_address).as_str());
        buff.push_str("persistent_keepalive_interval=10\n");
        for allowed_ip in &conf.allowed_ips {
            if allowed_ip.contains("/") {
                buff.push_str(format!("allowed_ip={allowed_ip}\n").as_str());
            } else {
                buff.push_str(format!("allowed_ip={allowed_ip}/32\n").as_str());
            }
        }
        buff.push('\n');
        log::info!("send netstack config to uapi");
        let data = uapi(buff.as_bytes()).context("call uapi")?;
        let s = String::from_utf8(data).context("failed to decode uapi response")?;
        if !s.contains("errno=0") {
            return Err(anyhow!("uapi returns unexpected result: {}", s));
        }
        Ok(())
    }

    pub fn health(&self, stale_after: Duration) -> Result<WgHealth> {
        let data = uapi(b"get=1\n\n")
            .with_context(|| format!("failed to query wireguard health for {}", self.name))?;
        parse_wg_health(&data, stale_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_handshake_is_explicitly_unhealthy() {
        assert!(matches!(
            parse_wg_health(
                b"last_handshake_time_sec=0\nerrno=0\n\n",
                Duration::from_secs(5)
            ),
            Ok(WgHealth::NoHandshake)
        ));
    }

    #[test]
    fn malformed_uapi_is_an_error_instead_of_healthy() {
        let error = parse_wg_health(b"errno=0\n\n", Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("last_handshake_time_sec"));
    }

    #[test]
    fn nonzero_uapi_errno_is_not_hidden_by_a_handshake_field() {
        let error = parse_wg_health(
            b"last_handshake_time_sec=1\nerrno=5\n\n",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("errno=5"));
    }

    #[test]
    fn current_handshake_is_healthy() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(matches!(
            parse_wg_health(
                format!("last_handshake_time_sec={now}\nerrno=0\n\n").as_bytes(),
                Duration::from_secs(5)
            ),
            Ok(WgHealth::Healthy(_))
        ));
    }
}

use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::{fs, io};

use anyhow::{anyhow, bail, Context, Result};
use cookie::Cookie as RawCookie;
use cookie_store::{Cookie, CookieStore};
use reqwest::header;
use reqwest::{ClientBuilder, Response, Url};
use reqwest_cookie_store::CookieStoreMutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use sha2::Digest;
use tokio::io::AsyncBufReadExt;

use crate::api::{ApiName, ApiUrl, ClientFailure, URL_GET_COMPANY};
use crate::config::{
    Config, WgConf, PLATFORM_CORPLINK, PLATFORM_CORPLINK_V1, PLATFORM_LARK, PLATFORM_LDAP,
    PLATFORM_OIDC, STRATEGY_DEFAULT, STRATEGY_LATENCY,
};
use crate::managed_routes::RouteResolutionReport;
use crate::qrcode::TerminalQrCode;
use crate::resp::*;
use crate::state::{self, State};
use crate::totp::{totp_offset, TIME_STEP};
use crate::utils;

const COOKIE_FILE_SUFFIX: &str = "cookies.jsonl";
const USER_AGENT: &str = "CorpLink/201000 (GooglePixel; Android 10; en)";
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(300);

async fn wait_for_interaction(operation: &'static str) -> Result<()> {
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    match tokio::time::timeout(INTERACTION_TIMEOUT, input.read_line(&mut line)).await {
        Ok(Ok(count)) if count > 0 => Ok(()),
        Ok(_) | Err(_) => Err(anyhow::Error::new(ClientFailure::interaction(operation))),
    }
}

async fn read_interactive_line(operation: &'static str) -> Result<String> {
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    match tokio::time::timeout(INTERACTION_TIMEOUT, input.read_line(&mut line)).await {
        Ok(Ok(count)) if count > 0 => Ok(line.trim().to_string()),
        Ok(_) | Err(_) => Err(anyhow::Error::new(ClientFailure::interaction(operation))),
    }
}

#[derive(Clone)]
pub struct Client {
    conf: Config,
    cookie: Arc<CookieStoreMutex>,
    cookie_file: path::PathBuf,
    cookie_corrupted: bool,
    cookie_corrupted_path: Option<path::PathBuf>,
    c: reqwest::Client,
    api_url: ApiUrl,
    date_offset_sec: i32,
    managed_routes_report: Option<RouteResolutionReport>,
}

struct LoadedCookieStore {
    store: CookieStore,
    corrupted: bool,
}

unsafe impl Send for Client {}

unsafe impl Sync for Client {}

pub async fn get_company_url(code: &str) -> anyhow::Result<RespCompany> {
    let c = ClientBuilder::new()
        // allow invalid certs because this cert is signed by corplink
        .danger_accept_invalid_certs(true)
        .build()
        .context("build client")?;
    let mut m = Map::new();
    m.insert("code".to_string(), json!(code));
    let body = serde_json::to_string(&m).context("serialize company request body")?;

    let resp = c
        .post(URL_GET_COMPANY)
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow::Error::new(ClientFailure::transport("company_match")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::Error::new(ClientFailure::http(
            "company_match",
            status.as_u16(),
        )));
    }
    let resp = resp
        .json::<Resp<RespCompany>>()
        .await
        .map_err(|_| anyhow::Error::new(ClientFailure::protocol("company_match", None)))?;
    match resp.code {
        0 => resp.data.context("company response missing data"),
        _ => Err(anyhow::Error::new(ClientFailure::api(
            "company_match",
            resp.code,
        ))),
    }
}

impl Client {
    pub fn new(conf: Config) -> Result<Client> {
        let f = conf.conf_file.clone().context("config file path missing")?;
        let interface_name = conf
            .interface_name
            .clone()
            .context("interface name missing in config")?;
        let identity_cookie_file = cookie_file_path_for_identity(
            &f,
            &interface_name,
            &conf.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );
        let legacy_cookie_file = cookie_file_path(&f, &interface_name, COOKIE_FILE_SUFFIX);
        log::info!("cookie file is: {}", identity_cookie_file.to_string_lossy());

        let session_identity_matches = conf
            .session_identity_matches()
            .context("failed to validate authentication session identity")?;
        let (loaded_cookie_store, migrate_legacy_cookie) =
            if session_identity_matches && identity_cookie_file.exists() {
                (load_cookie_store(&identity_cookie_file)?, false)
            } else if session_identity_matches
                && conf.allow_legacy_cookie_migration()
                && legacy_cookie_file.exists()
            {
                (load_cookie_store(&legacy_cookie_file)?, true)
            } else {
                if !session_identity_matches {
                    log::warn!("cookie store belongs to a different config; requiring login");
                }
                (
                    LoadedCookieStore {
                        store: CookieStore::default(),
                        corrupted: false,
                    },
                    false,
                )
            };
        let mut cookie_store = loaded_cookie_store.store;
        let cookie_corrupted_path = if loaded_cookie_store.corrupted && migrate_legacy_cookie {
            Some(legacy_cookie_file.clone())
        } else {
            None
        };
        let has_expired = cookie_store.iter_any().any(|cookie| cookie.is_expired());
        if has_expired {
            log::info!("some cookies are expired");
        }

        let mut headers = header::HeaderMap::new();

        if let Some(server) = conf.server.as_ref() {
            let server_url = Url::from_str(server.as_str())
                .with_context(|| format!("invalid server url: {server}"))?;

            if let Some(device_id) = conf.device_id.as_ref() {
                cookie_store
                    .insert_raw(&RawCookie::new("device_id", device_id), &server_url)
                    .context("failed to insert device_id cookie")?;
            }
            if let Some(device_name) = conf.device_name.as_ref() {
                cookie_store
                    .insert_raw(&RawCookie::new("device_name", device_name), &server_url)
                    .context("failed to insert device_name cookie")?;
            }

            if let Some(domain) = server_url.domain().or_else(|| server_url.host_str()) {
                if let Some(csrf_token) = cookie_store.get(domain, "/", "csrf-token") {
                    let value = header::HeaderValue::from_str(csrf_token.value())
                        .context("invalid csrf-token header value")?;
                    headers.insert("csrf-token", value);
                }
            }
        }

        let cookie_store = Arc::new(CookieStoreMutex::new(cookie_store));

        let c = ClientBuilder::new()
            // allow invalid certs because this cert is signed by corplink
            .danger_accept_invalid_certs(true)
            // for debug
            // .proxy(reqwest::Proxy::all("socks5://192.168.111.233:8001").unwrap())
            .user_agent(USER_AGENT)
            .cookie_provider(Arc::clone(&cookie_store))
            .default_headers(headers)
            .timeout(Duration::from_millis(10000))
            .build()
            .context("build http client")?;
        let conf_bak = conf.clone();
        let mut client = Client {
            conf,
            cookie: Arc::clone(&cookie_store),
            cookie_file: identity_cookie_file,
            cookie_corrupted: loaded_cookie_store.corrupted,
            cookie_corrupted_path,
            c,
            api_url: ApiUrl::new(&conf_bak)?,
            date_offset_sec: 0,
            managed_routes_report: None,
        };
        if loaded_cookie_store.corrupted {
            log::warn!("cookie store is unreadable; requiring login");
            client.conf.state = Some(State::Init);
            client.conf.save_session()?;
        } else if migrate_legacy_cookie {
            client.save_cookie()?;
        }
        Ok(client)
    }

    fn change_state(&mut self, state: State) -> Result<()> {
        self.conf.state = Some(state);
        self.conf.save_session()?;
        Ok(())
    }

    fn save_cookie(&mut self) -> Result<()> {
        let c = self
            .cookie
            .lock()
            .map_err(|e| anyhow!("failed to lock cookie store: {e}"))?;
        let mut bytes = Vec::new();
        c.save_incl_expired_and_nonpersistent_json(&mut bytes)
            .map_err(|_| anyhow!("failed to serialize cookies for persistence"))?;
        if self.cookie_corrupted {
            let corrupt_path = self
                .cookie_corrupted_path
                .as_ref()
                .unwrap_or(&self.cookie_file);
            state::backup_existing_private(corrupt_path, "corrupt")?
                .context("failed to preserve corrupt cookie store")?;
        }
        state::atomic_write_private(&self.cookie_file, &bytes).with_context(|| {
            format!(
                "failed to persist cookies to disk: {}",
                self.cookie_file.display()
            )
        })?;
        self.cookie_corrupted = false;
        self.cookie_corrupted_path = None;
        if self.conf.legacy_cookie_migration {
            self.conf.legacy_cookie_migration = false;
            self.conf.save_session()?;
        }
        Ok(())
    }

    async fn request<T: DeserializeOwned + fmt::Debug>(
        &mut self,
        api: ApiName,
        body: Option<Map<String, Value>>,
    ) -> Result<Resp<T>> {
        let url = self.api_url.get_api_url(&api);

        let rb = match body {
            Some(body) => {
                let body = serde_json::to_string(&body)
                    .with_context(|| format!("failed to serialize request body for {api:?}"))?;
                self.c
                    .post(url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
            }
            None => self.c.get(url),
        };

        let resp = rb
            .send()
            .await
            .map_err(|_| anyhow::Error::new(ClientFailure::transport(api.as_str())))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let failure = ClientFailure::http(api.as_str(), status);
            if failure.kind().requires_login() {
                self.change_state(State::Init)?;
            }
            return Err(anyhow::Error::new(failure));
        }

        self.parse_time_offset_from_date_header(&resp);

        for (name, _) in resp.headers() {
            if name.as_str().eq_ignore_ascii_case("set-cookie") {
                log::info!("found set-cookie in header, saving cookie");
                self.save_cookie()?;
                break;
            }
        }
        let text = resp
            .text()
            .await
            .map_err(|_| anyhow::Error::new(ClientFailure::protocol(api.as_str(), None)))?;
        // Parse the envelope generically first. When the server-side session has
        // expired the server returns a non-zero code (e.g. 101) with a `data`
        // whose shape doesn't match T (ListVPN, for instance, gets an object where
        // it expects an array). Deserializing straight into Resp<T> would fail here
        // and bypass the code-based logout/retry handling, leaving a stale-session
        // run dead with a confusing parse error. So only coerce `data` into T once
        // we know code == 0; otherwise keep the code/message so callers can react.
        let raw: Resp<Value> = serde_json::from_str(&text)
            .map_err(|_| anyhow::Error::new(ClientFailure::protocol(api.as_str(), None)))?;
        let data = match (raw.code, raw.data) {
            (0, Some(v)) => Some(serde_json::from_value::<T>(v).map_err(|_| {
                anyhow::Error::new(ClientFailure::protocol(api.as_str(), Some(raw.code)))
            })?),
            _ => None,
        };
        let resp = Resp::<T> {
            code: raw.code,
            message: raw.message,
            data,
            action: raw.action,
        };
        log::debug!("api={} returned code={}", api.as_str(), resp.code);
        Ok(resp)
    }

    fn parse_time_offset_from_date_header(&mut self, resp: &Response) {
        let headers = resp.headers();
        if let Some(date) = headers.get("date") {
            match date.to_str() {
                Ok(date) => match httpdate::parse_http_date(date) {
                    Ok(date) => {
                        let now = SystemTime::now();
                        self.date_offset_sec = if now < date {
                            let date_offset = date
                                .duration_since(now)
                                .unwrap_or_else(|_| Duration::from_secs(0));
                            date_offset.as_secs().try_into().unwrap_or_default()
                        } else {
                            let date_offset = now
                                .duration_since(date)
                                .unwrap_or_else(|_| Duration::from_secs(0));
                            let offset: i32 = date_offset.as_secs().try_into().unwrap_or_default();
                            -offset
                        };
                    }
                    Err(e) => {
                        log::warn!("failed to parse date in header, ignore it: {}", e);
                    }
                },
                Err(e) => log::warn!("failed to read date header: {}", e),
            }
        }
    }

    pub fn need_login(&self) -> bool {
        matches!(self.conf.state.as_ref(), None | Some(State::Init))
    }

    async fn check_tps_token(&mut self, token: &String) -> Result<String> {
        // tps confirmed, try to login with token
        let mut m = Map::new();
        m.insert("token".to_string(), json!(token));

        let resp = self
            .request::<RespLogin>(ApiName::TpsTokenCheck, Some(m))
            .await?;
        match resp.code {
            0 => resp.data.map(|d| d.url).ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::TpsTokenCheck.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::TpsTokenCheck.as_str(),
                resp.code,
            ))),
        }
    }

    async fn get_otp_uri_from_tps(
        &mut self,
        method: &str,
        url: &String,
        token: &String,
    ) -> Result<String> {
        log::info!("please scan the QR code or visit the following link to auth corplink:\n{url}");
        match TerminalQrCode::from_bytes(url.as_bytes()) {
            Ok(qr) => qr.print(),
            Err(e) => {
                log::warn!("failed to generate qr code: {e}");
            }
        }
        match method {
            PLATFORM_LARK | PLATFORM_OIDC => {
                log::info!("press enter if you finish auth");
                wait_for_interaction("scan the QR code").await?;
                self.check_tps_token(token).await
            }
            _ => {
                // TODO: add all tps login support
                bail!("unsupported platform, please contact the developer");
            }
        }
    }

    async fn corplink_login(&mut self) -> Result<String> {
        let resp = self.get_corplink_login_method().await?;
        for method in resp.auth {
            match method.as_str() {
                "password" => {
                    if let Some(password) = &self.conf.password {
                        if !password.is_empty() {
                            log::info!("try to login with password");
                            return self.login_with_password(PLATFORM_CORPLINK).await;
                        }
                    }
                    log::info!("no password provided, trying other methods");
                    continue;
                }
                "email" => {
                    log::info!("try to login with code from email");
                    return self.login_with_email().await;
                }
                _ => {
                    log::info!("unsupported method {method}, trying other methods");
                }
            }
        }
        bail!("failed to login with corplink")
    }

    async fn ldap_login(&mut self) -> Result<String> {
        // I don't know why but we must get login method before login
        let resp = self.get_corplink_login_method().await?;
        for method in resp.auth {
            if method != "password" {
                continue;
            }
            if let Some(password) = &self.conf.password {
                return if !password.is_empty() {
                    self.login_with_password(PLATFORM_LDAP).await
                } else {
                    bail!("no password provided")
                };
            }
        }
        bail!("failed to login with ldap")
    }

    fn is_platform_or_default(&self, platform: &str) -> bool {
        if let Some(p) = &self.conf.platform {
            return p.is_empty() || platform == p;
        }
        true
    }

    async fn request_otp_code(&mut self) -> Result<String> {
        let m = Map::new();
        let resp = self.request::<RespOtp>(ApiName::Otp, Some(m)).await?;
        match resp.code {
            0 => resp.data.map(|data| data.url).ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::Otp.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::Otp.as_str(),
                resp.code,
            ))),
        }
    }

    async fn get_otp_uri_by_otp(
        &mut self,
        tps_login: &HashMap<String, RespTpsLoginMethod>,
        method: &String,
    ) -> Result<String> {
        let url = self.get_otp_uri(tps_login, method).await?;
        if url.is_empty() {
            self.request_otp_code().await
        } else {
            Ok(url)
        }
    }
    async fn get_otp_uri(
        &mut self,
        tps_login: &HashMap<String, RespTpsLoginMethod>,
        method: &String,
    ) -> Result<String> {
        if let Some(resp) = tps_login
            .get(method)
            .filter(|_| self.is_platform_or_default(method))
        {
            log::info!("try to login with third party platform {method}");
            return self
                .get_otp_uri_from_tps(method, &resp.login_url, &resp.token)
                .await;
        }
        match method.as_str() {
            PLATFORM_CORPLINK => {
                if self.is_platform_or_default(PLATFORM_CORPLINK) {
                    log::info!("try to login with platform {PLATFORM_CORPLINK}");
                    return self.corplink_login().await;
                }
            }
            PLATFORM_LDAP => {
                if self.is_platform_or_default(PLATFORM_LDAP) {
                    log::info!("try to login with platform {PLATFORM_LDAP}");
                    return self.ldap_login().await;
                }
            }
            _ => {}
        }
        Ok(String::new())
    }

    // new feilian v1 login (/api/v1/login with AES-encrypted password).
    // opt-in via `"platform": "feilian_v1"`; the old login paths are untouched.
    async fn login_v1(&mut self) -> Result<()> {
        let password = self
            .conf
            .password
            .as_ref()
            .filter(|p| !p.is_empty())
            .context("platform feilian_v1 requires a password")?
            .clone();
        log::info!("try to login with platform feilian_v1");
        let enc = utils::feilian_v1_encrypt_password(&password);
        let mut m = Map::new();
        m.insert("login_scene".to_string(), json!(PLATFORM_CORPLINK));
        m.insert("account_type".to_string(), json!("userid"));
        m.insert("account".to_string(), json!(&self.conf.username));
        m.insert("password".to_string(), json!(enc));

        let resp = self
            .request::<RespLoginV1>(ApiName::LoginPasswordV1, Some(m))
            .await?;
        match resp.code {
            0 => {
                let data = resp.data.ok_or_else(|| {
                    anyhow::Error::new(ClientFailure::protocol(
                        ApiName::LoginPasswordV1.as_str(),
                        Some(resp.code),
                    ))
                })?;
                if data.result != "success" {
                    return Err(anyhow::Error::new(ClientFailure::api(
                        ApiName::LoginPasswordV1.as_str(),
                        resp.code,
                    )));
                }
                log::info!("login success");
                self.change_state(State::Login)?;

                // fetch the TOTP secret so 2fa codes can be generated locally,
                // mirroring the legacy login() flow. the v1 backend serves the
                // same /api/v2/p/otp endpoint and otpauth uri format.
                match self.request_otp_code().await {
                    Ok(otp_uri) if !otp_uri.is_empty() => {
                        let url = Url::parse(&otp_uri).context("failed to parse otp uri")?;
                        for (k, v) in url.query_pairs() {
                            if k == "secret" {
                                log::info!("received TOTP enrollment");
                                self.conf.code = Some(v.to_string());
                                self.conf.save_session()?;
                                break;
                            }
                        }
                    }
                    Ok(_) => {
                        log::info!(
                            "no otp code from server, will ask for 2fa code when connecting"
                        );
                    }
                    Err(e) => log::warn!("failed to get otp code: {e}"),
                }
                Ok(())
            }
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::LoginPasswordV1.as_str(),
                resp.code,
            ))),
        }
    }

    // choose right login method and login
    pub async fn login(&mut self) -> Result<()> {
        if self.conf.platform.as_deref() == Some(PLATFORM_CORPLINK_V1) {
            return self.login_v1().await;
        }
        let resp = self.get_login_method().await?;
        let tps_login_resp = self.get_tps_login_method().await?;
        let mut tps_login = HashMap::new();
        for resp in tps_login_resp {
            tps_login.insert(resp.alias.clone(), resp);
        }
        for method in resp.login_orders {
            let otp_uri = self.get_otp_uri_by_otp(&tps_login, &method).await;
            if let Err(e) = otp_uri {
                log::warn!("failed to login with method {method}: {e}");
                continue;
            }
            let otp_uri = otp_uri?;
            if otp_uri.is_empty() {
                log::info!("no otp code from server, will ask for 2fa code when connecting");
                self.change_state(State::Login)?;
                return Ok(());
            }
            self.change_state(State::Login)?;

            let url = Url::parse(&otp_uri).context("failed to parse otp uri")?;
            for (k, v) in url.query_pairs() {
                if k == "secret" {
                    log::info!("received TOTP enrollment");
                    self.conf.code = Some(v.to_string());
                    self.conf.save_session()?;
                    break;
                }
            }

            if let Some(code) = &self.conf.code {
                if !code.is_empty() {
                    return Ok(());
                }
            }
            log::warn!("failed to get otp code");
            return Ok(());
        }
        bail!("no available login method, please provide a valid platform")
    }

    async fn get_login_method(&mut self) -> Result<RespLoginMethod> {
        let resp = self
            .request::<RespLoginMethod>(ApiName::LoginMethod, None)
            .await?;
        match resp.code {
            0 => resp.data.ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::LoginMethod.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::LoginMethod.as_str(),
                resp.code,
            ))),
        }
    }

    // get 3rd party login methods and links, only lark(feishu) is tested
    async fn get_tps_login_method(&mut self) -> Result<Vec<RespTpsLoginMethod>> {
        let resp = self
            .request::<Vec<RespTpsLoginMethod>>(ApiName::TpsLoginMethod, None)
            .await?;
        match resp.code {
            0 => Ok(resp.data.unwrap_or_default()),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::TpsLoginMethod.as_str(),
                resp.code,
            ))),
        }
    }

    // get corplink login method, knowing result can be password or email
    async fn get_corplink_login_method(&mut self) -> Result<RespCorplinkLoginMethod> {
        let mut m = Map::new();
        m.insert("forget_password".to_string(), json!(false));
        m.insert("user_name".to_string(), json!(&self.conf.username));

        let resp = self
            .request::<RespCorplinkLoginMethod>(ApiName::CorplinkLoginMethod, Some(m))
            .await?;
        match resp.code {
            0 => resp.data.ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::CorplinkLoginMethod.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::CorplinkLoginMethod.as_str(),
                resp.code,
            ))),
        }
    }

    async fn login_with_password(&mut self, platform: &str) -> Result<String> {
        let mut password = self
            .conf
            .password
            .as_ref()
            .context("password is required for password login")?
            .clone();
        let mut m = Map::new();
        match platform {
            PLATFORM_LDAP => {
                m.insert("platform".to_string(), json!(PLATFORM_LDAP));
            }
            PLATFORM_CORPLINK => {
                if password.len() != 64 {
                    let mut sha = sha2::Sha256::new();
                    sha.update(password.as_bytes());
                    password = format!("{:x}", sha.finalize());
                } // else: password already convert to sha256sum
            }
            _ => {
                bail!("invalid platform {platform}")
            }
        }
        m.insert("password".to_string(), json!(password));
        m.insert("user_name".to_string(), json!(&self.conf.username));

        let resp = self
            .request::<RespLogin>(ApiName::LoginPassword, Some(m))
            .await?;
        match resp.code {
            0 => resp.data.map(|data| data.url).ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::LoginPassword.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::LoginPassword.as_str(),
                resp.code,
            ))),
        }
    }

    async fn request_email_code(&mut self) -> Result<()> {
        let mut m = Map::new();
        m.insert("forget_password".to_string(), json!(false));
        m.insert("code_type".to_string(), json!("email"));
        m.insert("user_name".to_string(), json!(&self.conf.username));

        let resp = self
            .request::<Map<String, Value>>(ApiName::RequestEmailCode, Some(m))
            .await?;
        match resp.code {
            0 => Ok(()),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::RequestEmailCode.as_str(),
                resp.code,
            ))),
        }
    }

    async fn login_with_email(&mut self) -> Result<String> {
        // tell server to send code to email
        log::info!("try to request code for email");
        self.request_email_code().await?;

        log::info!("input your code from email:");
        let code = read_interactive_line("email verification code").await?;
        let mut m = Map::new();
        m.insert("forget_password".to_string(), json!(false));
        m.insert("code_type".to_string(), json!("email"));
        m.insert("code".to_string(), json!(&code));

        let resp = self
            .request::<RespLogin>(ApiName::LoginEmail, Some(m))
            .await?;
        match resp.code {
            0 => resp.data.map(|data| data.url).ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::LoginEmail.as_str(),
                    Some(resp.code),
                ))
            }),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::LoginEmail.as_str(),
                resp.code,
            ))),
        }
    }

    async fn handle_logout_err(&mut self, operation: &'static str, code: i32) -> Result<()> {
        self.change_state(State::Init)
            .context("failed to reset state after authentication expiry")?;
        Err(anyhow::Error::new(ClientFailure::api(operation, code)))
    }

    async fn list_vpn(&mut self) -> Result<Vec<RespVpnInfo>> {
        let resp = self
            .request::<Vec<RespVpnInfo>>(ApiName::ListVPN, None)
            .await?;
        match resp.code {
            0 => resp.data.ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::ListVPN.as_str(),
                    Some(resp.code),
                ))
            }),
            101 => {
                self.handle_logout_err(ApiName::ListVPN.as_str(), resp.code)
                    .await?;
                unreachable!()
            }
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::ListVPN.as_str(),
                resp.code,
            ))),
        }
    }

    async fn get_first_vpn_by_latency(
        &mut self,
        vpn_info: Vec<RespVpnInfo>,
    ) -> Option<RespVpnInfo> {
        let mut fast_vpn = None;
        let mut min_latency = i64::MAX;
        for vpn in vpn_info {
            let latency = match self.ping_vpn(vpn.ip.clone(), vpn.api_port).await {
                Ok(latency) => latency,
                Err(err) => {
                    log::warn!("failed to ping {}:{}: {}", vpn.ip, vpn.api_port, err);
                    -1
                }
            };

            log::info!(
                "server name {}{}",
                vpn.en_name,
                match latency {
                    -1 => " timeout".to_string(),
                    _ => format!(", latency {}ms", latency),
                }
            );
            if latency != -1 && latency < min_latency {
                fast_vpn = Some(vpn);
                min_latency = latency;
            }
        }
        fast_vpn
    }

    async fn get_first_available_vpn(&mut self, vpn_info: Vec<RespVpnInfo>) -> Option<RespVpnInfo> {
        for vpn in vpn_info {
            let latency = match self.ping_vpn(vpn.ip.clone(), vpn.api_port).await {
                Ok(latency) => latency,
                Err(err) => {
                    log::warn!("failed to ping {}:{}: {}", vpn.ip, vpn.api_port, err);
                    -1
                }
            };
            if latency != -1 {
                return Some(vpn);
            }
        }
        None
    }

    // ping vpn and return latency in ms. Will return Err on error
    async fn ping_vpn(&mut self, ip: String, api_port: u16) -> Result<i64> {
        {
            // config cookie
            let mut cookie = self
                .cookie
                .lock()
                .map_err(|e| anyhow!("failed to lock cookie store: {e}"))?;
            let server_url = self
                .conf
                .server
                .as_ref()
                .context("server url is required to ping vpn")?;

            let mut url = Url::from_str(server_url)
                .with_context(|| format!("invalid server url: {server_url}"))?;
            let mut cookies: Vec<Cookie> = Vec::new();
            for c in cookie.iter_any() {
                if c.domain.matches(&url.clone()) {
                    cookies.push(c.clone());
                }
            }
            url.set_host(Some(ip.as_str()))
                .context("failed to set ping host")?;
            url.set_port(Some(api_port))
                .or_else(|_| bail!("failed to set ping port"))?;
            for c in cookies {
                let mut c = cookie::Cookie::new(c.name().to_string(), c.value().to_string());
                c.set_domain(ip.clone());
                let c = Cookie::try_from_raw_cookie(&c, &url.clone())
                    .context("failed to convert raw cookie")?;
                cookie
                    .insert(c, &url.clone())
                    .context("failed to insert ping cookie")?;
            }
            self.api_url.vpn_param.url = url.to_string().trim_end_matches('/').to_string();
        }
        self.save_cookie()?;
        let req_start = Utc::now().timestamp_millis();
        let resp = self.request::<String>(ApiName::PingVPN, None).await?;
        let req_end = Utc::now().timestamp_millis();
        let latency = req_end - req_start;
        match resp.code {
            0 => Ok(latency),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::PingVPN.as_str(),
                resp.code,
            ))),
        }
    }

    async fn fetch_peer_info(&mut self, public_key: &String) -> Result<RespWgInfo> {
        let mut otp = String::new();
        if let Some(code) = &self.conf.code {
            if !code.is_empty() {
                let code = utils::b32_decode(code)?;
                let offset = self.date_offset_sec / TIME_STEP as i32;
                let raw_otp = totp_offset(code.as_slice(), offset);
                otp = format!("{:06}", raw_otp.code);
                log::info!("2fa code generated, {} seconds left", raw_otp.secs_left);
            }
        }
        if otp.is_empty() {
            let is_tps_login = matches!(
                self.conf.platform.as_deref(),
                Some(PLATFORM_LARK | PLATFORM_OIDC)
            );
            if is_tps_login {
                log::info!("use empty 2fa code (tps login already verified)");
            } else {
                log::info!("input your 2fa code:");
                otp = read_interactive_line("two-factor authentication code").await?;
            }
        }
        let mut m = Map::new();
        m.insert("public_key".to_string(), json!(public_key));
        m.insert("otp".to_string(), json!(otp));
        let resp = self
            .request::<RespWgInfo>(ApiName::ConnectVPN, Some(m))
            .await?;
        match resp.code {
            0 => resp.data.ok_or_else(|| {
                anyhow::Error::new(ClientFailure::protocol(
                    ApiName::ConnectVPN.as_str(),
                    Some(resp.code),
                ))
            }),
            101 => {
                self.handle_logout_err(ApiName::ConnectVPN.as_str(), resp.code)
                    .await?;
                unreachable!()
            }
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::ConnectVPN.as_str(),
                resp.code,
            ))),
        }
    }

    pub async fn connect_vpn(&mut self) -> Result<WgConf> {
        let vpn_info = self.list_vpn().await?;

        log::info!(
            "found {} vpn(s), details: {:?}",
            vpn_info.len(),
            vpn_info
                .iter()
                .map(|i| i.en_name.clone())
                .collect::<Vec<String>>()
        );
        let filtered_vpn = vpn_info
            .into_iter()
            .filter(|vpn| {
                if let Some(server_name) = self.conf.vpn_server_name.clone() {
                    if vpn.en_name != server_name {
                        log::info!("skip {}, expect {}", vpn.en_name, server_name);
                        return false;
                    }
                }
                true
            })
            .filter(|vpn| {
                let mode = match vpn.protocol_mode {
                    1 => "tcp",
                    2 => "udp",
                    _ => "unknown protocol",
                };
                match mode {
                    "udp" => true,
                    "tcp" => true,
                    _ => {
                        log::info!(
                            "server name {} is not support {} wg for now",
                            vpn.en_name,
                            mode
                        );
                        false
                    }
                }
            })
            .collect();

        let vpn = match self.conf.vpn_select_strategy.clone() {
            Some(strategy) => match strategy.as_str() {
                STRATEGY_LATENCY => self.get_first_vpn_by_latency(filtered_vpn).await,
                STRATEGY_DEFAULT => self.get_first_available_vpn(filtered_vpn).await,
                _ => bail!("unsupported strategy"),
            },
            None => self.get_first_available_vpn(filtered_vpn).await,
        };

        let vpn = match vpn {
            Some(ref vpn) => vpn,
            None => bail!("no vpn available"),
        };
        let vpn_addr = format!("{}:{}", vpn.ip, vpn.vpn_port);
        log::info!("try connect to {}, address {}", vpn.en_name, vpn_addr);
        self.set_vpn_target(vpn)?;

        let key = self
            .conf
            .public_key
            .as_ref()
            .context("public key missing in config")?
            .clone();
        log::info!("try to get wg conf from remote");
        let wg_info = self.fetch_peer_info(&key).await?;
        let mtu = wg_info.setting.vpn_mtu;
        let dns = wg_info.setting.vpn_dns;
        let peer_key = wg_info.public_key;
        let public_key = self
            .conf
            .public_key
            .as_ref()
            .context("public key missing in config")?
            .clone();
        let private_key = self
            .conf
            .private_key
            .as_ref()
            .context("private key missing in config")?
            .clone();
        let ip_mask = wg_info.ip_mask.parse::<u32>().context("invalid ip mask")?;
        let address = format!("{}/{}", wg_info.ip, ip_mask);
        let address6 = (!wg_info.ipv6.is_empty())
            .then_some(format!("{}/128", wg_info.ipv6))
            .unwrap_or("".into());
        let mut allowed_ips = match self.conf.route_mode.clone().unwrap_or_default() {
            crate::config::RouteMode::Split => {
                log::info!("route_mode = split");
                [
                    wg_info.setting.vpn_route_split,
                    wg_info.setting.v6_route_split.unwrap_or_default(),
                ]
                .concat()
            }
            crate::config::RouteMode::Full => {
                log::info!("route_mode = full");
                let v4 = wg_info.setting.vpn_route_full;
                let v6 = wg_info.setting.v6_route_full.unwrap_or_default();
                log::info!(
                    "route_mode=full, server returned vpn_route_full ({} entries): {:?}",
                    v4.len(),
                    v4
                );
                log::info!(
                    "route_mode=full, server returned v6_route_full ({} entries): {:?}",
                    v6.len(),
                    v6
                );
                if v4.is_empty() && v6.is_empty() {
                    bail!(
                        "route_mode=full but server returned no routes (vpn_route_full / v6_route_full both empty); \
                         refuse to fall back to 0.0.0.0/0 to avoid peer-IP routing loop that blocks all traffic"
                    );
                }
                [v4, v6].concat()
            }
        };
        append_extra_allowed_ips(&mut allowed_ips, self.conf.extra_allowed_ips.as_ref())?;
        let managed_report =
            crate::managed_routes::resolve_managed_routes_report(&self.conf, true).await?;
        append_routes(&mut allowed_ips, "managed_routes", &managed_report.routes)?;
        self.managed_routes_report = Some(managed_report);

        // Carve user-specified CIDRs out of allowed_ips. This removes any IPs in
        // vpn_disallowed_routes from the VPN's AllowedIPs (and the system routes
        // derived from them), which is the standard way to avoid routing loops in
        // full-tunnel mode — e.g. listing the local LAN or a CIDR covering the
        // VPN peer endpoint so their packets don't get captured by the tunnel.
        //
        // Semantics: CIDR subtraction. An entry like "10.68.0.0/16" carves that
        // whole range out of each allowed_ip, even when allowed_ip is a larger
        // supernet such as "0.0.0.0/0" — which expands to a minimal set of
        // smaller CIDRs covering "allowed minus disallowed".
        if let Some(disallowed) = self.conf.vpn_disallowed_routes.as_ref() {
            if !disallowed.is_empty() {
                let before = allowed_ips.len();
                for d in disallowed {
                    let mut carved = Vec::with_capacity(allowed_ips.len());
                    for a in &allowed_ips {
                        carved.extend(crate::utils::subtract_cidr_from_cidr(a, d));
                    }
                    allowed_ips = carved;
                }
                log::info!(
                    "vpn_disallowed_routes applied: {} -> {} entries (carved: {:?})",
                    before,
                    allowed_ips.len(),
                    disallowed
                );
            }
        }

        // Auto-carve the VPN peer endpoint IP out of allowed_ips. In full-tunnel mode
        // the server typically returns 0.0.0.0/0, which would match the outer UDP
        // packets going to the peer itself, producing a routing loop (black hole).
        // Mirrors wg-quick's behavior of excluding the endpoint from routes. No-op
        // when the peer IP isn't covered by any allowed_ip (e.g. split mode).
        match vpn.ip.parse::<std::net::IpAddr>() {
            Ok(peer_ip) => {
                let peer_cidr = match peer_ip {
                    std::net::IpAddr::V4(_) => format!("{}/32", peer_ip),
                    std::net::IpAddr::V6(_) => format!("{}/128", peer_ip),
                };
                let before = allowed_ips.len();
                let mut carved = Vec::with_capacity(allowed_ips.len());
                for a in &allowed_ips {
                    carved.extend(crate::utils::subtract_cidr_from_cidr(a, &peer_cidr));
                }
                if carved.len() != before {
                    log::info!(
                        "auto-carved peer endpoint {} out of allowed_ips: {} -> {} entries",
                        peer_cidr,
                        before,
                        carved.len()
                    );
                }
                allowed_ips = carved;
            }
            Err(e) => {
                log::warn!(
                    "could not parse vpn.ip {:?} as IP, skipping peer-IP carve-out: {}",
                    vpn.ip,
                    e
                );
            }
        }
        dedupe_allowed_ips(&mut allowed_ips);
        log::info!(
            "final allowed_ips ({} entries): {:?}",
            allowed_ips.len(),
            allowed_ips
        );
        let auto_setup_routes = self.conf.auto_setup_routes.unwrap_or(true);
        let routes = if auto_setup_routes {
            allowed_ips.clone()
        } else {
            log::info!("auto_setup_routes is disabled, skip setting routes");
            Vec::new()
        };

        // corplink config
        let wg_conf = WgConf {
            address,
            address6,
            peer_address: vpn_addr,
            mtu,
            public_key,
            private_key,
            peer_key,
            allowed_ips,
            routes,
            dns,
            // `force_protocol`, when set, overrides the server-advertised `protocol_mode`
            protocol: match self.conf.force_protocol.as_deref() {
                Some(p) if p.eq_ignore_ascii_case("udp") => 0,
                Some(p) if p.eq_ignore_ascii_case("tcp") => 1,
                _ => match vpn.protocol_mode {
                    // tcp
                    1 => 1,
                    // udp
                    _ => 0,
                },
            },
        };
        Ok(wg_conf)
    }

    fn set_vpn_target(&mut self, vpn: &RespVpnInfo) -> Result<()> {
        let server_url = self
            .conf
            .server
            .as_ref()
            .context("server url is required to connect vpn")?;
        let mut url = Url::from_str(server_url)
            .with_context(|| format!("invalid server url: {server_url}"))?;
        url.set_host(Some(vpn.ip.as_str()))
            .context("failed to set vpn host")?;
        url.set_port(Some(vpn.api_port))
            .or_else(|_| bail!("failed to set vpn port"))?;
        self.api_url.vpn_param.url = url.to_string().trim_end_matches('/').to_string();
        Ok(())
    }

    pub fn managed_routes_report(&self) -> Option<&RouteResolutionReport> {
        self.managed_routes_report.as_ref()
    }

    pub async fn mark_managed_routes_applied(
        &self,
        wg_conf: &WgConf,
        generation: &str,
        pid: u32,
    ) -> Result<()> {
        let Some(report) = self.managed_routes_report.as_ref() else {
            return Ok(());
        };
        crate::managed_routes::mark_managed_routes_applied(
            &self.conf, report, wg_conf, generation, pid,
        )
        .await
    }

    pub async fn keep_alive_vpn(&mut self, conf: &WgConf, interval: u64) {
        loop {
            log::info!("keep alive");
            match self.report_vpn_status(conf).await {
                Ok(_) => (),
                Err(err) => {
                    log::warn!("keep alive error: {}", err);
                    return;
                }
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    pub async fn report_vpn_status(&mut self, conf: &WgConf) -> Result<()> {
        let mut m = Map::new();
        m.insert("ip".to_string(), json!(conf.address));
        m.insert("public_key".to_string(), json!(conf.public_key));
        m.insert(
            "mode".to_string(),
            json!(match self.conf.route_mode.clone().unwrap_or_default() {
                crate::config::RouteMode::Split => "Split",
                crate::config::RouteMode::Full => "Full",
            }),
        );
        m.insert("type".to_string(), json!("100"));

        let resp = self
            .request::<Map<String, Value>>(ApiName::KeepAliveVPN, Some(m))
            .await?;
        match resp.code {
            0 => Ok(()),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::KeepAliveVPN.as_str(),
                resp.code,
            ))),
        }
    }

    pub async fn disconnect_vpn(&mut self, wg_conf: &WgConf) -> Result<()> {
        let mut m = Map::new();
        m.insert("ip".to_string(), json!(wg_conf.address));
        m.insert("public_key".to_string(), json!(wg_conf.public_key));
        m.insert(
            "mode".to_string(),
            json!(match self.conf.route_mode.clone().unwrap_or_default() {
                crate::config::RouteMode::Split => "Split",
                crate::config::RouteMode::Full => "Full",
            }),
        );
        m.insert("type".to_string(), json!("101"));
        let resp = self
            .request::<Map<String, Value>>(ApiName::DisconnectVPN, Some(m))
            .await?;
        match resp.code {
            0 => Ok(()),
            _ => Err(anyhow::Error::new(ClientFailure::api(
                ApiName::DisconnectVPN.as_str(),
                resp.code,
            ))),
        }
    }

    // log out the current terminal, freeing its server-side session/terminal
    // quota (servers cap concurrent terminals, e.g. nankai allows only 3).
    // best-effort: callers treat failures as non-fatal since we're exiting.
    pub async fn logout(&mut self) -> Result<()> {
        let url = self.api_url.get_api_url(&ApiName::Logout);
        let mut req = self.c.get(url);
        // /api/logout validates a csrf-token header (double-submit against the
        // cookie). the token is only known after login, so read it from the
        // cookie store here rather than relying on the default headers.
        if let Some(server) = self.conf.server.as_ref() {
            if let Ok(server_url) = Url::parse(server) {
                if let Some(domain) = server_url.domain().or_else(|| server_url.host_str()) {
                    let token = {
                        let store = self
                            .cookie
                            .lock()
                            .map_err(|e| anyhow!("failed to lock cookie store: {e}"))?;
                        store
                            .get(domain, "/", "csrf-token")
                            .map(|c| c.value().to_string())
                    };
                    if let Some(token) = token {
                        if let Ok(value) = header::HeaderValue::from_str(&token) {
                            req = req.header("csrf-token", value);
                        }
                    }
                }
            }
        }
        // The endpoint may reply with a redirect (not JSON). Check the HTTP
        // result, but always clear the local session so a failed remote logout
        // cannot leave the next run believing it is authenticated.
        let response = req
            .send()
            .await
            .map_err(|_| anyhow::Error::new(ClientFailure::transport(ApiName::Logout.as_str())));
        let state_result = self.change_state(State::Init);
        state_result.context("failed to persist local logout state")?;
        let resp = response?;
        if resp.status().is_success() || resp.status().is_redirection() {
            log::info!("logout (current terminal) completed");
            Ok(())
        } else {
            Err(anyhow::Error::new(ClientFailure::http(
                ApiName::Logout.as_str(),
                resp.status().as_u16(),
            )))
        }
    }
}

fn cookie_file_path(conf_file: &str, interface_name: &str, suffix: &str) -> path::PathBuf {
    let dir = path::Path::new(conf_file)
        .parent()
        .unwrap_or_else(|| path::Path::new("."));
    dir.join(format!("{interface_name}_{suffix}"))
}

fn cookie_file_path_for_identity(
    conf_file: &str,
    interface_name: &str,
    identity: &str,
    suffix: &str,
) -> path::PathBuf {
    let dir = path::Path::new(conf_file)
        .parent()
        .unwrap_or_else(|| path::Path::new("."));
    dir.join(format!("{interface_name}_{identity}_{suffix}"))
}

fn load_cookie_store(cookie_file: &path::Path) -> Result<LoadedCookieStore> {
    match fs::File::open(cookie_file).map(io::BufReader::new) {
        Ok(file) => match CookieStore::load_json_all(file) {
            Ok(store) => Ok(LoadedCookieStore {
                store,
                corrupted: false,
            }),
            Err(_) => Ok(LoadedCookieStore {
                store: CookieStore::default(),
                corrupted: true,
            }),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(LoadedCookieStore {
            store: CookieStore::default(),
            corrupted: false,
        }),
        Err(err) => Err(err)
            .with_context(|| format!("failed to open cookie file {}", cookie_file.display())),
    }
}

fn append_extra_allowed_ips(
    allowed_ips: &mut Vec<String>,
    extra_allowed_ips: Option<&Vec<String>>,
) -> Result<()> {
    let Some(extra_allowed_ips) = extra_allowed_ips else {
        return Ok(());
    };
    if extra_allowed_ips.is_empty() {
        return Ok(());
    }

    append_routes(allowed_ips, "extra_allowed_ips", extra_allowed_ips)
}

fn append_routes(allowed_ips: &mut Vec<String>, label: &str, routes: &[String]) -> Result<()> {
    if routes.is_empty() {
        return Ok(());
    }

    let before = allowed_ips.len();
    let mut seen: HashSet<String> = allowed_ips.iter().cloned().collect();
    for route in routes {
        let normalized = crate::utils::normalize_route(route)
            .with_context(|| format!("invalid {label} entry {route:?}"))?;
        if seen.insert(normalized.clone()) {
            allowed_ips.push(normalized);
        }
    }
    log::info!(
        "{} applied: {} -> {} entries (configured: {})",
        label,
        before,
        allowed_ips.len(),
        routes.len()
    );
    Ok(())
}

fn dedupe_allowed_ips(allowed_ips: &mut Vec<String>) {
    let before = allowed_ips.len();
    let mut seen = HashSet::with_capacity(allowed_ips.len());
    allowed_ips.retain(|route| seen.insert(route.clone()));
    if allowed_ips.len() != before {
        log::info!(
            "deduped allowed_ips: {} -> {} entries",
            before,
            allowed_ips.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_extra_allowed_ips_normalizes_bare_ips_and_dedupes() {
        let mut allowed_ips = vec!["10.0.0.0/8".to_string(), "140.82.112.0/20".to_string()];
        let extra_allowed_ips = vec![
            "140.82.112.0/20".to_string(),
            "20.205.243.166".to_string(),
            "2001:db8::1".to_string(),
        ];

        append_extra_allowed_ips(&mut allowed_ips, Some(&extra_allowed_ips)).unwrap();

        assert_eq!(
            allowed_ips,
            vec![
                "10.0.0.0/8",
                "140.82.112.0/20",
                "20.205.243.166/32",
                "2001:db8::1/128"
            ]
        );
    }

    #[test]
    fn append_extra_allowed_ips_rejects_invalid_routes() {
        let mut allowed_ips = Vec::new();
        let extra_allowed_ips = vec!["20.205.243.166/129".to_string()];

        let err = append_extra_allowed_ips(&mut allowed_ips, Some(&extra_allowed_ips))
            .expect_err("invalid prefix should fail");

        assert!(err.to_string().contains("invalid extra_allowed_ips entry"));
    }

    #[test]
    fn cookie_file_path_uses_config_parent_and_suffix() {
        let path = cookie_file_path(
            "/tmp/corplink/config.local.json",
            "utun12345",
            "cookies.jsonl",
        );

        assert_eq!(
            path,
            path::PathBuf::from("/tmp/corplink/utun12345_cookies.jsonl")
        );
    }

    #[tokio::test]
    async fn corrupted_cookie_store_requires_login_without_overwriting_evidence() {
        let dir =
            std::env::temp_dir().join(format!("corplink-cookie-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            br#"{"company_name":"company","username":"user","server":"http://127.0.0.1"}"#,
        )
        .unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();
        let cookie_path = cookie_file_path_for_identity(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            &config.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );
        let corrupt = b"not-json-cookie-store";
        fs::write(&cookie_path, corrupt).unwrap();

        let client = Client::new(config).unwrap();

        assert!(client.need_login());
        assert_eq!(fs::read(cookie_path).unwrap(), corrupt);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rate_limit_failure_preserves_the_authenticated_session() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = b"secret response body must not escape";
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-http-failure-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();

        let mut client = Client::new(config).unwrap();
        assert!(!client.need_login());
        let error = match client.connect_vpn().await {
            Ok(_) => panic!("429 must fail"),
            Err(error) => error,
        };

        assert_eq!(
            crate::api::classify_error(&error),
            crate::api::FailureKind::RateLimited
        );
        assert!(!client.need_login());
        assert!(error.to_string().contains("429"));
        assert!(!error.to_string().contains("secret response body"));
        server_thread.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn latency_selection_negotiates_with_the_selected_gateway() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        fn serve(
            listener: TcpListener,
            responses: Vec<(u64, String)>,
            seen: Arc<Mutex<Vec<String>>>,
        ) -> std::thread::JoinHandle<()> {
            std::thread::spawn(move || {
                for (delay_ms, body) in responses {
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    let (mut stream, _) = loop {
                        match listener.accept() {
                            Ok(connection) => break connection,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "timed out waiting for expected gateway request"
                                );
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("gateway accept failed: {error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut request = [0_u8; 8192];
                    let count = stream.read(&mut request).unwrap();
                    let request = String::from_utf8_lossy(&request[..count]);
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    seen.lock().unwrap().push(request_line);
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            })
        }

        let list_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let b_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        list_listener.set_nonblocking(true).unwrap();
        a_listener.set_nonblocking(true).unwrap();
        b_listener.set_nonblocking(true).unwrap();
        let list_port = list_listener.local_addr().unwrap().port();
        let a_port = a_listener.local_addr().unwrap().port();
        let b_port = b_listener.local_addr().unwrap().port();
        let a_seen = Arc::new(Mutex::new(Vec::new()));
        let b_seen = Arc::new(Mutex::new(Vec::new()));
        let list_seen = Arc::new(Mutex::new(Vec::new()));
        let list_body = format!(
            "{{\"code\":0,\"data\":[{{\"api_port\":{a_port},\"vpn_port\":51820,\"ip\":\"127.0.0.1\",\"protocol_mode\":2,\"name\":\"a\",\"en_name\":\"A\",\"icon\":\"\",\"id\":1,\"timeout\":10}},{{\"api_port\":{b_port},\"vpn_port\":51821,\"ip\":\"127.0.0.1\",\"protocol_mode\":2,\"name\":\"b\",\"en_name\":\"B\",\"icon\":\"\",\"id\":2,\"timeout\":10}}]}}"
        );
        let conn_body = r#"{"code":0,"data":{"ip":"10.0.0.2","ipv6":"","ip_mask":"24","public_key":"peer-key","setting":{"vpn_mtu":1420,"vpn_dns":"10.0.0.53","vpn_dns_backup":"","vpn_dns_domain_split":null,"vpn_route_full":[],"vpn_route_split":["10.0.0.0/8"],"v6_route_full":null,"v6_route_split":null},"mode":0}}"#;
        let list_thread = serve(list_listener, vec![(0, list_body)], Arc::clone(&list_seen));
        let a_thread = serve(
            a_listener,
            vec![
                (0, r#"{"code":0,"data":"ok"}"#.to_string()),
                (0, conn_body.to_string()),
            ],
            Arc::clone(&a_seen),
        );
        let b_thread = serve(
            b_listener,
            vec![(80, r#"{"code":0,"data":"ok"}"#.to_string())],
            Arc::clone(&b_seen),
        );

        let dir =
            std::env::temp_dir().join(format!("corplink-latency-selection-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let server = format!("http://127.0.0.1:{list_port}");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"platform\":\"lark\",\"server\":\"{server}\",\"vpn_select_strategy\":\"latency\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();

        let mut client = Client::new(config).unwrap();
        let wg = client.connect_vpn().await.unwrap();

        assert_eq!(wg.peer_address, "127.0.0.1:51820");
        assert_eq!(a_seen.lock().unwrap().len(), 2);
        assert_eq!(b_seen.lock().unwrap().len(), 1);
        assert!(a_seen.lock().unwrap()[0].contains("/vpn/ping?"));
        assert!(a_seen.lock().unwrap()[1].contains("/vpn/conn?"));
        assert!(b_seen.lock().unwrap()[0].contains("/vpn/ping?"));
        assert_eq!(list_seen.lock().unwrap().len(), 1);

        list_thread.join().unwrap();
        a_thread.join().unwrap();
        b_thread.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn authentication_http_failure_resets_session_state() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = b"authentication response must not be logged";
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-auth-failure-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();

        let mut client = Client::new(config).unwrap();
        assert!(!client.need_login());
        let error = match client.connect_vpn().await {
            Ok(_) => panic!("401 must fail"),
            Err(error) => error,
        };

        assert_eq!(
            crate::api::classify_error(&error),
            crate::api::FailureKind::AuthenticationExpired
        );
        assert!(client.need_login());
        assert!(error.to_string().contains("401"));
        assert!(!error
            .to_string()
            .contains("authentication response must not be logged"));
        server_thread.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn changed_account_never_reuses_old_cookie_after_auth_failure_and_restart() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_server = Arc::clone(&requests);
        let server_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("gateway accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0_u8; 8192];
                let count = stream.read(&mut request).unwrap();
                requests_for_server
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..count]).to_string());
                let body = b"expired";
                write!(
                    stream,
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-cookie-identity-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let first_source = format!(
            "{{\"company_name\":\"company\",\"username\":\"account-a\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, first_source).unwrap();
        let first = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        let first_cookie_path = cookie_file_path_for_identity(
            config_path.to_str().unwrap(),
            first.interface_name.as_deref().unwrap(),
            &first.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );
        let mut cookie_store = CookieStore::default();
        let server_url = Url::parse(&server).unwrap();
        cookie_store
            .insert_raw(&RawCookie::new("account", "account-a"), &server_url)
            .unwrap();
        let mut cookie_bytes = Vec::new();
        cookie_store
            .save_incl_expired_and_nonpersistent_json(&mut cookie_bytes)
            .unwrap();
        fs::write(&first_cookie_path, cookie_bytes).unwrap();

        let second_source = format!(
            "{{\"company_name\":\"company\",\"username\":\"account-b\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, second_source).unwrap();
        let mut second = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        second.state = Some(State::Login);
        second.save_session().unwrap();
        let mut first_client = Client::new(second).unwrap();
        let first_error = match first_client.connect_vpn().await {
            Ok(_) => panic!("expired account must fail"),
            Err(error) => error,
        };
        assert_eq!(
            crate::api::classify_error(&first_error),
            crate::api::FailureKind::AuthenticationExpired
        );
        assert!(first_client.need_login());

        let mut restarted = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        restarted.state = Some(State::Login);
        restarted.save_session().unwrap();
        let mut second_client = Client::new(restarted).unwrap();
        let _second_error = match second_client.connect_vpn().await {
            Ok(_) => panic!("expired account must fail after restart"),
            Err(error) => error,
        };
        assert!(second_client.need_login());

        server_thread.join().unwrap();
        for request in requests.lock().unwrap().iter() {
            assert!(!request.contains("account=account-a"));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn recovering_from_corrupt_cookie_preserves_original_before_set_cookie_write() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = br#"{"code":0,"data":[]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: recovered=1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-cookie-recovery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();
        let cookie_path = cookie_file_path_for_identity(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            &config.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );
        let corrupt = b"corrupt-cookie-evidence";
        fs::write(&cookie_path, corrupt).unwrap();

        let mut client = Client::new(config).unwrap();
        let _ = client.connect_vpn().await;
        server_thread.join().unwrap();

        assert_ne!(fs::read(&cookie_path).unwrap(), corrupt);
        let backup = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("cookies.jsonl.corrupt.")
            })
            .expect("corrupt cookie backup should remain");
        assert_eq!(fs::read(backup.path()).unwrap(), corrupt);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_cookie_jsonl_is_migrated_to_identity_specific_store() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let request = Arc::new(Mutex::new(String::new()));
        let request_for_server = Arc::clone(&request);
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0_u8; 4096];
            let count = stream.read(&mut bytes).unwrap();
            *request_for_server.lock().unwrap() =
                String::from_utf8_lossy(&bytes[..count]).to_string();
            let body = b"expired";
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-legacy-cookie-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"device_name\":\"legacy-device\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();
        let legacy_path = cookie_file_path(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            COOKIE_FILE_SUFFIX,
        );
        let mut cookie_store = CookieStore::default();
        let server_url = Url::parse(&server).unwrap();
        cookie_store
            .insert(
                Cookie::try_from_raw_cookie(
                    &RawCookie::build("legacy", "1")
                        .domain("127.0.0.1")
                        .path("/")
                        .finish(),
                    &server_url,
                )
                .unwrap(),
                &server_url,
            )
            .unwrap();
        let mut cookie_bytes = Vec::new();
        cookie_store
            .save_incl_expired_and_nonpersistent_json(&mut cookie_bytes)
            .unwrap();
        fs::write(&legacy_path, cookie_bytes).unwrap();

        let identity_path = cookie_file_path_for_identity(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            &config.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );
        // Simulate a process dying after Config::from_file wrote the session
        // sidecar but before Client copied the legacy cookie store.
        let resumed = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        let mut client = Client::new(resumed).unwrap();
        let _ = client.connect_vpn().await;
        server_thread.join().unwrap();

        assert!(identity_path.exists());
        let captured_request = request.lock().unwrap().clone();
        assert!(
            captured_request.contains("legacy=1"),
            "captured request was: {captured_request}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn corrupt_legacy_cookie_recovers_to_identity_store_on_set_cookie() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_server = Arc::clone(&requests);
        let server_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 8192];
                let count = stream.read(&mut request).unwrap();
                requests_for_server
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..count]).to_string());
                let body = br#"{"code":0,"data":[]}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: recovered=1; Path=/; Max-Age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "corplink-legacy-cookie-recovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"device_name\":\"legacy-device\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        let legacy_path = cookie_file_path(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            COOKIE_FILE_SUFFIX,
        );
        fs::write(&legacy_path, b"corrupt-legacy-cookie").unwrap();
        let identity_path = cookie_file_path_for_identity(
            config_path.to_str().unwrap(),
            config.interface_name.as_deref().unwrap(),
            &config.session_identity_tag(),
            COOKIE_FILE_SUFFIX,
        );

        let mut first_client = Client::new(config).unwrap();
        assert!(first_client.need_login());
        let _ = first_client.connect_vpn().await;
        assert!(identity_path.exists());

        let restarted = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        let mut second_client = Client::new(restarted).unwrap();
        let _ = second_client.connect_vpn().await;
        server_thread.join().unwrap();

        assert!(requests.lock().unwrap()[1].contains("recovered=1"));
        assert_eq!(fs::read(&legacy_path).unwrap(), b"corrupt-legacy-cookie");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn logout_checks_http_result_and_persists_local_init_state() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = format!("http://127.0.0.1:{port}");
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = b"logout failure body";
            write!(
                stream,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let dir =
            std::env::temp_dir().join(format!("corplink-logout-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let source = format!(
            "{{\"company_name\":\"company\",\"username\":\"user\",\"platform\":\"lark\",\"server\":\"{server}\"}}"
        );
        fs::write(&config_path, source).unwrap();
        let mut config = Config::from_file(config_path.to_str().unwrap())
            .await
            .unwrap();
        config.state = Some(State::Login);
        config.save_session().unwrap();
        let mut client = Client::new(config).unwrap();
        assert!(!client.need_login());

        let error = client.logout().await.expect_err("503 logout must fail");
        assert_eq!(
            crate::api::classify_error(&error),
            crate::api::FailureKind::Server
        );
        assert!(client.need_login());
        assert!(!error.to_string().contains("logout failure body"));
        server_thread.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}

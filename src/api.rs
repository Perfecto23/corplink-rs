use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::Config;
use crate::template::Template;

pub const URL_GET_COMPANY: &str = "https://corplink.volcengine.cn/api/match";

/// The bounded failure classes exposed to the runtime and CLI layers.
///
/// A caller can use this classification without parsing user-facing error text.
/// The text representation intentionally contains only operation/status/code
/// context; response bodies and authentication material never enter it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// A connection, timeout, or other transport failure that may recover.
    RecoverableTransport,
    /// The server asked the caller to slow down or retry later.
    RateLimited,
    /// The remote service returned a transient 5xx failure.
    Server,
    /// The server rejected the current authentication/session.
    AuthenticationExpired,
    /// A human action (OTP, QR confirmation, or email code) is required.
    InteractionRequired,
    /// Local configuration or state persistence is invalid/unavailable.
    Configuration,
    /// The remote contract or response shape is invalid.
    Protocol,
}

impl FailureKind {
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RecoverableTransport | Self::RateLimited | Self::Server
        )
    }

    pub fn requires_login(self) -> bool {
        matches!(self, Self::AuthenticationExpired)
    }
}

#[derive(Debug)]
pub struct ClientFailure {
    kind: FailureKind,
    operation: String,
    status: Option<u16>,
    code: Option<i32>,
}

impl ClientFailure {
    pub fn new(
        kind: FailureKind,
        operation: impl Into<String>,
        status: Option<u16>,
        code: Option<i32>,
    ) -> Self {
        Self {
            kind,
            operation: operation.into(),
            status,
            code,
        }
    }

    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn code(&self) -> Option<i32> {
        self.code
    }

    pub fn transport(operation: impl Into<String>) -> Self {
        Self::new(FailureKind::RecoverableTransport, operation, None, None)
    }

    pub fn interaction(operation: impl Into<String>) -> Self {
        Self::new(FailureKind::InteractionRequired, operation, None, None)
    }

    pub fn configuration(operation: impl Into<String>) -> Self {
        Self::new(FailureKind::Configuration, operation, None, None)
    }

    pub fn protocol(operation: impl Into<String>, code: Option<i32>) -> Self {
        Self::new(FailureKind::Protocol, operation, None, code)
    }

    pub fn http(operation: impl Into<String>, status: u16) -> Self {
        let kind = match status {
            408 | 425 => FailureKind::RecoverableTransport,
            429 => FailureKind::RateLimited,
            500..=599 => FailureKind::Server,
            401 | 403 | 419 => FailureKind::AuthenticationExpired,
            _ => FailureKind::Protocol,
        };
        Self::new(kind, operation, Some(status), None)
    }

    pub fn api(operation: impl Into<String>, code: i32) -> Self {
        let kind = if code == 101 {
            FailureKind::AuthenticationExpired
        } else {
            FailureKind::Protocol
        };
        Self::new(kind, operation, None, Some(code))
    }
}

impl fmt::Display for ClientFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self
            .status
            .map(|value| format!(" http {}", value))
            .unwrap_or_default();
        let code = self
            .code
            .map(|value| format!(" api code {}", value))
            .unwrap_or_default();
        if self.kind == FailureKind::AuthenticationExpired {
            write!(
                f,
                "authentication expired: {} failed with{}{}",
                self.operation, status, code
            )
        } else {
            write!(
                f,
                "{}: {} failed with{}{}",
                self.kind, self.operation, status, code
            )
        }
    }
}

impl Error for ClientFailure {}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::RecoverableTransport => "transport error",
            Self::RateLimited => "rate limited",
            Self::Server => "server error",
            Self::AuthenticationExpired => "authentication expired",
            Self::InteractionRequired => "interaction required",
            Self::Configuration => "configuration error",
            Self::Protocol => "protocol error",
        };
        f.write_str(label)
    }
}

/// Classify a Client error without requiring callers to inspect its text.
pub fn classify_error(error: &anyhow::Error) -> FailureKind {
    error
        .downcast_ref::<ClientFailure>()
        .map(ClientFailure::kind)
        .unwrap_or(FailureKind::Protocol)
}

pub(crate) const CORPLINK_APP_VERSION: &str = "201000";

const URL_GET_LOGIN_METHOD: &str = "{{url}}/api/login/setting?os={{os}}&os_version={{version}}";
const URL_GET_TPS_LOGIN_METHOD: &str = "{{url}}/api/tpslogin/link?os={{os}}&os_version={{version}}";
const URL_GET_TPS_TOKEN_CHECK: &str =
    "{{url}}/api/tpslogin/token/check?os={{os}}&os_version={{version}}";
const URL_GET_CORPLINK_LOGIN_METHOD: &str = "{{url}}/api/lookup?os={{os}}&os_version={{version}}";
const URL_REQUEST_CODE: &str = "{{url}}/api/login/code/send?os={{os}}&os_version={{version}}";
const URL_VERIFY_CODE: &str = "{{url}}/api/login/code/verify?os={{os}}&os_version={{version}}";
const URL_LOGIN_PASSWORD: &str = "{{url}}/api/login?os={{os}}&os_version={{version}}";
const URL_LOGIN_PASSWORD_V1: &str =
    "{{url}}/api/v1/login?os={{os}}&os_version={{version}}&client_source=FeiLian";
const URL_LIST_VPN: &str =
    "{{url}}/api/vpn/list?os={{os}}&os_version={{version}}&app_version={{app_version}}";

const URL_PING_VPN_HOST: &str = "{{url}}/vpn/ping?os={{os}}&os_version={{version}}";
const URL_FETCH_PEER_INFO: &str = "{{url}}/vpn/conn?os={{os}}&os_version={{version}}";
const URL_OPERATE_VPN: &str = "{{url}}/vpn/report?os={{os}}&os_version={{version}}";
const URL_OTP: &str = "{{url}}/api/v2/p/otp?os={{os}}&os_version={{version}}";
// log out the current terminal so it frees the server-side session/terminal
// quota. logout_all=false only signs out this device. responds with a 302.
const URL_LOGOUT: &str = "{{url}}/api/logout?os={{os}}&os_version={{version}}&logout_all=false";

#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub enum ApiName {
    LoginMethod,
    TpsLoginMethod,
    TpsTokenCheck,
    CorplinkLoginMethod,
    RequestEmailCode,
    LoginPassword,
    LoginPasswordV1,
    LoginEmail,
    ListVPN,

    PingVPN,
    ConnectVPN,
    KeepAliveVPN,
    DisconnectVPN,
    Otp,
    Logout,
}

impl ApiName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LoginMethod => "login_method",
            Self::TpsLoginMethod => "tps_login_method",
            Self::TpsTokenCheck => "tps_token_check",
            Self::CorplinkLoginMethod => "corplink_login_method",
            Self::RequestEmailCode => "request_email_code",
            Self::LoginPassword => "login_password",
            Self::LoginPasswordV1 => "login_password_v1",
            Self::LoginEmail => "login_email",
            Self::ListVPN => "list_vpn",
            Self::PingVPN => "ping_vpn",
            Self::ConnectVPN => "connect_vpn",
            Self::KeepAliveVPN => "keep_alive_vpn",
            Self::DisconnectVPN => "disconnect_vpn",
            Self::Otp => "otp",
            Self::Logout => "logout",
        }
    }
}

#[derive(Clone, Serialize)]
struct UserUrlParam {
    app_version: String,
    url: String,
    os: String,
    version: String,
}

#[derive(Clone, Serialize)]
pub struct VpnUrlParam {
    pub url: String,
    os: String,
    version: String,
}

#[derive(Clone)]
pub struct ApiUrl {
    user_param: UserUrlParam,
    pub vpn_param: VpnUrlParam,
    api_template: HashMap<ApiName, Template>,
}

impl ApiUrl {
    pub fn new(conf: &Config) -> Result<ApiUrl> {
        let os = "Android".to_string();
        let version = "2".to_string();
        let mut api_template = HashMap::new();

        api_template.insert(ApiName::LoginMethod, Template::new(URL_GET_LOGIN_METHOD));
        api_template.insert(
            ApiName::TpsLoginMethod,
            Template::new(URL_GET_TPS_LOGIN_METHOD),
        );
        api_template.insert(
            ApiName::TpsTokenCheck,
            Template::new(URL_GET_TPS_TOKEN_CHECK),
        );
        api_template.insert(
            ApiName::CorplinkLoginMethod,
            Template::new(URL_GET_CORPLINK_LOGIN_METHOD),
        );
        api_template.insert(ApiName::RequestEmailCode, Template::new(URL_REQUEST_CODE));
        api_template.insert(ApiName::LoginEmail, Template::new(URL_VERIFY_CODE));
        api_template.insert(ApiName::LoginPassword, Template::new(URL_LOGIN_PASSWORD));
        api_template.insert(
            ApiName::LoginPasswordV1,
            Template::new(URL_LOGIN_PASSWORD_V1),
        );
        api_template.insert(ApiName::ListVPN, Template::new(URL_LIST_VPN));
        api_template.insert(ApiName::PingVPN, Template::new(URL_PING_VPN_HOST));
        api_template.insert(ApiName::ConnectVPN, Template::new(URL_FETCH_PEER_INFO));
        api_template.insert(ApiName::KeepAliveVPN, Template::new(URL_OPERATE_VPN));
        api_template.insert(ApiName::DisconnectVPN, Template::new(URL_OPERATE_VPN));
        api_template.insert(ApiName::Otp, Template::new(URL_OTP));
        api_template.insert(ApiName::Logout, Template::new(URL_LOGOUT));

        Ok(ApiUrl {
            user_param: UserUrlParam {
                app_version: CORPLINK_APP_VERSION.to_string(),
                url: conf
                    .server
                    .clone()
                    .context("server url missing in config")?,
                os: os.clone(),
                version: version.clone(),
            },
            vpn_param: VpnUrlParam {
                url: "".to_string(),
                os,
                version,
            },
            api_template,
        })
    }

    pub fn get_api_url(&self, name: &ApiName) -> String {
        let user_param = &self.user_param;
        let vpn_param = &self.vpn_param;
        match name {
            ApiName::LoginMethod => self.api_template[name].render(user_param),
            ApiName::TpsLoginMethod => self.api_template[name].render(user_param),
            ApiName::TpsTokenCheck => self.api_template[name].render(user_param),
            ApiName::CorplinkLoginMethod => self.api_template[name].render(user_param),
            ApiName::RequestEmailCode => self.api_template[name].render(user_param),
            ApiName::LoginEmail => self.api_template[name].render(user_param),
            ApiName::LoginPassword => self.api_template[name].render(user_param),
            ApiName::LoginPasswordV1 => self.api_template[name].render(user_param),
            ApiName::ListVPN => self.api_template[name].render(user_param),
            ApiName::Otp => self.api_template[name].render(user_param),
            ApiName::Logout => self.api_template[name].render(user_param),

            ApiName::PingVPN => self.api_template[name].render(vpn_param),
            ApiName::ConnectVPN => self.api_template[name].render(vpn_param),
            ApiName::KeepAliveVPN => self.api_template[name].render(vpn_param),
            ApiName::DisconnectVPN => self.api_template[name].render(vpn_param),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_classification_preserves_safe_http_context() {
        let failure = ClientFailure::http("list_vpn", 429);

        assert_eq!(failure.kind(), FailureKind::RateLimited);
        assert!(failure.kind().is_retryable());
        assert_eq!(failure.status(), Some(429));
        assert!(failure.to_string().contains("rate limited"));
        assert!(failure.to_string().contains("list_vpn"));
        assert!(!failure.to_string().contains("secret"));
    }

    #[test]
    fn classify_error_returns_interaction_without_parsing_message() {
        let error = anyhow::Error::new(ClientFailure::interaction("otp"));

        assert_eq!(classify_error(&error), FailureKind::InteractionRequired);
        assert!(!classify_error(&error).is_retryable());
    }

    #[test]
    fn server_and_rate_limit_failures_do_not_require_login() {
        let server = ClientFailure::http("connect_vpn", 503);
        let rate_limit = ClientFailure::http("connect_vpn", 429);

        assert_eq!(server.kind(), FailureKind::Server);
        assert_eq!(rate_limit.kind(), FailureKind::RateLimited);
        assert!(!server.kind().requires_login());
        assert!(!rate_limit.kind().requires_login());
        assert!(!server.to_string().contains("logout"));
    }

    #[test]
    fn authentication_api_code_requires_login_without_server_message() {
        let failure = ClientFailure::api("list_vpn", 101);

        assert_eq!(failure.kind(), FailureKind::AuthenticationExpired);
        assert!(failure.kind().requires_login());
        assert_eq!(failure.code(), Some(101));
        assert!(!failure.to_string().contains("server supplied message"));
    }
}

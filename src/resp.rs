#[derive(serde::Deserialize, Debug)]
pub struct Resp<T> {
    pub code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespCompany {
    pub name: String,
    pub zh_name: String,
    pub en_name: String,
    pub domain: String,
    pub enable_self_signed: bool,
    pub self_signed_cert: String,
    pub enable_public_key: bool,
    pub public_key: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespLoginMethod {
    pub login_enable_ldap: bool,
    pub login_enable: bool,
    pub login_orders: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespTpsLoginMethod {
    pub alias: String,
    pub login_url: String,
    pub token: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespCorplinkLoginMethod {
    pub mfa: bool,
    pub auth: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespLogin {
    #[serde(default)]
    pub url: String,
}

// response of the v1 login endpoint (/api/v1/login), e.g.
// {"result":"success","next":{"action":"GoToLink","can_skip":false}}
#[derive(serde::Deserialize, Debug)]
pub struct RespLoginV1 {
    #[serde(default)]
    pub result: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespOtp {
    pub url: String,
    pub code: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespVpnInfo {
    pub api_port: u16,
    pub vpn_port: u16,
    pub ip: String,
    // 1 for TCP, 2 for UDP.
    pub protocol_mode: i32,
    pub name: String,
    pub en_name: String,
    pub icon: String,
    pub id: i32,
    pub timeout: i32,
}

impl RespVpnInfo {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.en_name
        } else {
            &self.name
        }
    }

    pub fn matches_name(&self, configured: Option<&str>) -> bool {
        match configured.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => self.name == name || self.en_name == name,
            None => true,
        }
    }
}

#[derive(serde::Deserialize, Debug)]
pub struct RespWgExtraInfo {
    pub vpn_mtu: u32,
    pub vpn_dns: String,
    pub vpn_dns_backup: String,
    pub vpn_dns_domain_split: Option<Vec<String>>,
    pub vpn_route_full: Vec<String>,
    pub vpn_route_split: Vec<String>,
    pub v6_route_full: Option<Vec<String>>,
    pub v6_route_split: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Debug)]
pub struct RespWgInfo {
    pub ip: String,
    pub ipv6: String,
    pub ip_mask: String,
    pub public_key: String,
    pub setting: RespWgExtraInfo,
    pub mode: u32,
}

#[cfg(test)]
mod tests {
    use super::RespVpnInfo;

    #[test]
    fn node_names_match_display_or_english_name_and_blank_means_automatic() {
        for (name, en_name) in [("Office", ""), ("云节点", "Cloud VPN"), ("", "Cloud VPN")] {
            let vpn: RespVpnInfo = serde_json::from_value(serde_json::json!({
                "api_port": 443, "vpn_port": 51820, "ip": "127.0.0.1",
                "protocol_mode": 2, "name": name, "en_name": en_name,
                "icon": "", "id": 1, "timeout": 10
            }))
            .unwrap();
            for automatic in [None, Some(""), Some("  ")] {
                assert!(vpn.matches_name(automatic));
            }
            for alias in [name, en_name]
                .into_iter()
                .filter(|value| !value.is_empty())
            {
                assert!(vpn.matches_name(Some(alias)));
                assert!(vpn.matches_name(Some(&format!(" {alias} "))));
            }
            assert!(!vpn.matches_name(Some("unavailable node")));
            assert_eq!(
                vpn.display_name(),
                if name.is_empty() { en_name } else { name }
            );
        }
    }
}

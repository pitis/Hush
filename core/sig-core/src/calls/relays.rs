//! TURN/STUN relays for calls, from the chat server (`GET /v2/calling/relays`).

use presage::libsignal_service::prelude::PushService;
use ringrtc::webrtc::peer_connection_factory::IceServer;

/// One relay group from the server (Signal Desktop's shape).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct RelayGroup {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    #[serde(rename = "urlsWithIps", default)]
    pub urls_with_ips: Option<Vec<String>>,
}

/// Parses the server's JSON (`{"relays": [...]}`); junk answers give no relays, never an error.
pub(crate) fn parse_relays(body: &serde_json::Value) -> Vec<RelayGroup> {
    body.get("relays")
        .and_then(|r| r.as_array())
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| serde_json::from_value(g.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Each group becomes up to two RingRTC IceServers, exactly as Desktop builds them: the
/// ip-pinned urls with the hostname for TLS, and the plain urls with no hostname.
pub(crate) fn to_ice_servers(groups: &[RelayGroup]) -> Vec<IceServer> {
    let mut servers = Vec::new();
    for g in groups {
        if let Some(urls) = g.urls_with_ips.as_ref().filter(|u| !u.is_empty()) {
            servers.push(IceServer::new(
                g.username.clone(),
                g.password.clone(),
                g.hostname.clone().unwrap_or_default(),
                urls.clone(),
            ));
        }
        if let Some(urls) = g.urls.as_ref().filter(|u| !u.is_empty()) {
            servers.push(IceServer::new(
                g.username.clone(),
                g.password.clone(),
                String::new(),
                urls.clone(),
            ));
        }
    }
    servers
}

/// Fetches and converts. An error or empty answer gives no relays with a warning — a call
/// still tries direct ICE.
pub(crate) async fn fetch(push: &PushService) -> Vec<IceServer> {
    use presage::libsignal_service::configuration::Endpoint;
    use presage::libsignal_service::push_service::HttpAuthOverride;
    let answer = async {
        let resp = push
            .request(
                reqwest::Method::GET,
                Endpoint::service("/v2/calling/relays"),
                HttpAuthOverride::NoOverride,
            )
            .map_err(|e| e.to_string())?
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !resp.status().is_success() {
            return Err(format!("calling relays: HTTP {}", resp.status().as_u16()));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| e.without_url().to_string())
    }
    .await;
    match answer {
        Ok(v) => to_ice_servers(&parse_relays(&v)),
        Err(e) => {
            tracing::warn!("fetching calling relays failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_shaped_json_parses() {
        let v: serde_json::Value = serde_json::json!({
            "relays": [{
                "username": "u", "password": "p", "hostname": "turn.example.org",
                "urls": ["turn:example.org"], "urlsWithIps": ["turn:1.2.3.4"]
            }]
        });
        let groups = parse_relays(&v);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].urls_with_ips.as_deref(),
            Some(&["turn:1.2.3.4".to_string()][..])
        );
        // one group → two IceServers (ip-pinned with hostname, plain with empty hostname)
        assert_eq!(to_ice_servers(&groups).len(), 2);
    }

    #[test]
    fn empty_or_junk_json_means_no_relays() {
        assert!(parse_relays(&serde_json::json!({})).is_empty());
        assert!(parse_relays(&serde_json::json!({"relays": "nope"})).is_empty());
        let only_creds = vec![RelayGroup {
            username: "u".into(),
            password: "p".into(),
            hostname: None,
            urls: None,
            urls_with_ips: None,
        }];
        assert!(to_ice_servers(&only_creds).is_empty());
    }
}

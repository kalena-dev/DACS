//! Minimal client for the Headscale v0.29.x REST API (grpc-gateway, `/api/v1`).
//!
//! Pinned against juanfont/headscale v0.29.7 (`proto/headscale/v1/*.proto`).
//! grpc-gateway emits lowerCamelCase field names and encodes uint64 as JSON
//! strings; both spellings and encodings are accepted here.

use dacs_net_contract::Secret;
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

pub const PINNED_HEADSCALE_VERSION: &str = "v0.29.7";

#[derive(Debug, thiserror::Error)]
pub enum HeadscaleError {
    #[error("headscale request failed: {0}")]
    Transport(String),
    #[error("headscale returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected headscale response: {0}")]
    Decode(String),
}

impl HeadscaleError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, HeadscaleError::Status { status: 404, .. })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreAuthKey {
    #[serde(deserialize_with = "de_u64")]
    pub id: u64,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub reusable: bool,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub used: bool,
    #[serde(default, deserialize_with = "de_opt_ts")]
    pub expiration: Option<OffsetDateTime>,
    #[serde(default, alias = "acl_tags")]
    pub acl_tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(deserialize_with = "de_u64")]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "given_name")]
    pub given_name: String,
    #[serde(default, alias = "ip_addresses")]
    pub ip_addresses: Vec<String>,
    #[serde(default)]
    pub online: bool,
    #[serde(default, alias = "last_seen", deserialize_with = "de_opt_ts")]
    pub last_seen: Option<OffsetDateTime>,
    #[serde(default, alias = "pre_auth_key")]
    pub pre_auth_key: Option<PreAuthKey>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Operations the adapter needs. Implemented by [`HeadscaleClient`] and by an
/// in-memory fake in tests.
pub trait HeadscaleApi: Send + Sync {
    fn health(&self) -> impl Future<Output = Result<(), HeadscaleError>> + Send;
    fn create_tagged_key(
        &self,
        tags: &[String],
        expiration: OffsetDateTime,
    ) -> impl Future<Output = Result<PreAuthKey, HeadscaleError>> + Send;
    fn expire_key(&self, id: u64) -> impl Future<Output = Result<(), HeadscaleError>> + Send;
    fn list_keys(&self) -> impl Future<Output = Result<Vec<PreAuthKey>, HeadscaleError>> + Send;
    fn list_nodes(&self) -> impl Future<Output = Result<Vec<Node>, HeadscaleError>> + Send;
    fn delete_node(&self, id: u64) -> impl Future<Output = Result<(), HeadscaleError>> + Send;
    fn rename_node(
        &self,
        id: u64,
        name: &str,
    ) -> impl Future<Output = Result<Node, HeadscaleError>> + Send;
}

pub struct HeadscaleClient {
    base: String,
    api_key: Secret,
    http: reqwest::Client,
}

impl HeadscaleClient {
    /// `base` is the Headscale listen URL, e.g. `http://127.0.0.1:8080`.
    pub fn new(base: &str, api_key: Secret) -> Result<Self, HeadscaleError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| HeadscaleError::Transport(e.to_string()))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            api_key,
            http,
        })
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<serde_json::Value, HeadscaleError> {
        let resp = req
            .bearer_auth(self.api_key.expose())
            .send()
            .await
            .map_err(|e| HeadscaleError::Transport(self.redact(&e.without_url().to_string())))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| HeadscaleError::Transport(self.redact(&e.to_string())))?;
        if !status.is_success() {
            // Redact before truncating so no partial key survives the cut.
            return Err(HeadscaleError::Status {
                status: status.as_u16(),
                body: truncate(&self.redact(&body), 500),
            });
        }
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| HeadscaleError::Decode(self.redact(&e.to_string())))
    }

    /// Upstream text ends up in API errors and logs: strip the API key (a
    /// proxy or debug front end may echo the `Authorization` header) and
    /// anything shaped like a Headscale key.
    fn redact(&self, text: &str) -> String {
        let key = self.api_key.expose();
        if key.is_empty() {
            redact_keys(text)
        } else {
            redact_keys(&text.replace(key, "[redacted]"))
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{}", self.base, path)
    }
}

fn field<T: for<'de> Deserialize<'de>>(
    v: serde_json::Value,
    camel: &str,
    snake: &str,
) -> Result<T, HeadscaleError> {
    let inner = v
        .get(camel)
        .or_else(|| v.get(snake))
        .cloned()
        .ok_or_else(|| HeadscaleError::Decode(format!("missing field {camel}")))?;
    serde_json::from_value(inner)
        .map_err(|e| HeadscaleError::Decode(redact_keys(&format!("{camel}: {e}"))))
}

/// serde type errors quote the offending value; Headscale API keys and
/// pre-auth keys share the `hskey-` prefix, so mask whatever follows it.
fn redact_keys(text: &str) -> String {
    const PREFIX: &str = "hskey-";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(PREFIX) {
        out.push_str(&rest[..i]);
        out.push_str("hskey-[redacted]");
        let tail = &rest[i + PREFIX.len()..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn list<T: for<'de> Deserialize<'de>>(
    v: serde_json::Value,
    camel: &str,
    snake: &str,
) -> Result<Vec<T>, HeadscaleError> {
    match v.get(camel).or_else(|| v.get(snake)) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(inner) => serde_json::from_value(inner.clone())
            .map_err(|e| HeadscaleError::Decode(redact_keys(&format!("{camel}: {e}")))),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateKeyBody<'a> {
    reusable: bool,
    ephemeral: bool,
    expiration: String,
    acl_tags: &'a [String],
}

impl HeadscaleApi for HeadscaleClient {
    async fn health(&self) -> Result<(), HeadscaleError> {
        let v = self.send(self.http.get(self.url("health"))).await?;
        let ok = v
            .get("databaseConnectivity")
            .or_else(|| v.get("database_connectivity"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(HeadscaleError::Decode("database connectivity false".into()))
        }
    }

    async fn create_tagged_key(
        &self,
        tags: &[String],
        expiration: OffsetDateTime,
    ) -> Result<PreAuthKey, HeadscaleError> {
        // user = 0 (omitted): tags-only key; the node becomes a tagged node owned
        // by `tagged-devices`. Single use, not ephemeral.
        let body = CreateKeyBody {
            reusable: false,
            ephemeral: false,
            expiration: rfc3339(expiration),
            acl_tags: tags,
        };
        let v = self
            .send(self.http.post(self.url("preauthkey")).json(&body))
            .await?;
        field(v, "preAuthKey", "pre_auth_key")
    }

    async fn expire_key(&self, id: u64) -> Result<(), HeadscaleError> {
        let body = serde_json::json!({ "id": id.to_string() });
        self.send(self.http.post(self.url("preauthkey/expire")).json(&body))
            .await?;
        Ok(())
    }

    async fn list_keys(&self) -> Result<Vec<PreAuthKey>, HeadscaleError> {
        let v = self.send(self.http.get(self.url("preauthkey"))).await?;
        list(v, "preAuthKeys", "pre_auth_keys")
    }

    async fn list_nodes(&self) -> Result<Vec<Node>, HeadscaleError> {
        let v = self.send(self.http.get(self.url("node"))).await?;
        list(v, "nodes", "nodes")
    }

    async fn delete_node(&self, id: u64) -> Result<(), HeadscaleError> {
        self.send(self.http.delete(self.url(&format!("node/{id}"))))
            .await?;
        Ok(())
    }

    async fn rename_node(&self, id: u64, name: &str) -> Result<Node, HeadscaleError> {
        let v = self
            .send(
                self.http
                    .post(self.url(&format!("node/{id}/rename/{name}"))),
            )
            .await?;
        field(v, "node", "node")
    }
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("rfc3339 formatting")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn de_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        S(String),
        N(u64),
    }
    match N::deserialize(d)? {
        N::N(n) => Ok(n),
        N::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

/// grpc-gateway emits RFC 3339 timestamps; unset values may be `null` or the
/// zero time `0001-01-01T00:00:00Z`.
fn de_opt_ts<'de, D: Deserializer<'de>>(d: D) -> Result<Option<OffsetDateTime>, D::Error> {
    let s: Option<String> = Option::deserialize(d)?;
    let Some(s) = s else { return Ok(None) };
    if s.is_empty() || s.starts_with("0001-01-01") {
        return Ok(None);
    }
    OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_grpc_gateway_node() {
        let v = serde_json::json!({
            "node": {
                "id": "7",
                "machineKey": "mkey:x",
                "ipAddresses": ["100.64.0.7", "fd7a:115c:a1e0::7"],
                "name": "alice-pc",
                "givenName": "alice-pc",
                "lastSeen": "2026-11-14T10:00:00Z",
                "expiry": "0001-01-01T00:00:00Z",
                "preAuthKey": {"id": "12", "key": "", "used": true, "expiration": "2026-11-14T10:15:00Z", "aclTags": ["tag:dacs-worker"]},
                "online": true,
                "tags": ["tag:dacs-worker"]
            }
        });
        let n: Node = field(v, "node", "node").unwrap();
        assert_eq!(n.id, 7);
        assert_eq!(n.ip_addresses.len(), 2);
        assert_eq!(n.pre_auth_key.as_ref().unwrap().id, 12);
        assert!(n.online);
        assert!(n.last_seen.is_some());
    }

    #[test]
    fn decodes_snake_case_and_numeric_ids() {
        let v = serde_json::json!({"pre_auth_keys": [{"id": 3, "used": false, "expiration": null, "acl_tags": ["tag:a"]}]});
        let keys: Vec<PreAuthKey> = list(v, "preAuthKeys", "pre_auth_keys").unwrap();
        assert_eq!(keys[0].id, 3);
        assert_eq!(keys[0].acl_tags, vec!["tag:a"]);
        assert!(keys[0].expiration.is_none());
    }

    #[test]
    fn create_body_uses_camel_case_tags() {
        let tags = vec!["tag:dacs-worker".to_owned()];
        let body = CreateKeyBody {
            reusable: false,
            ephemeral: false,
            expiration: rfc3339(time::macros::datetime!(2026-11-14 10:15 UTC)),
            acl_tags: &tags,
        };
        let v = serde_json::to_value(body).unwrap();
        assert_eq!(v["aclTags"][0], "tag:dacs-worker");
        assert_eq!(v["expiration"], "2026-11-14T10:15:00Z");
        assert!(v.get("user").is_none());
    }
}

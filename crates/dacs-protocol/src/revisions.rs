//! Pinned revisions and content references.
use serde::{Deserialize, Serialize};

/// Full Git object id (40 or 64 lowercase hex chars).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitOid(pub String);

impl GitOid {
    pub fn parse(s: &str) -> Result<Self, String> {
        let ok = (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok { Ok(Self(s.to_owned())) } else { Err(format!("not a full lowercase git oid: `{s}`")) }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for GitOid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

/// Content address of an immutable artifact: `sha256:<64 lowercase hex>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactDigest(pub String);

impl ArtifactDigest {
    pub fn from_sha256_hex(hex: &str) -> Self { Self(format!("sha256:{hex}")) }
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.strip_prefix("sha256:") {
            Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => Ok(Self(s.to_owned())),
            _ => Err(format!("not a sha256 artifact digest: `{s}`")),
        }
    }
    /// The 64-char hex part. Only meaningful for values that passed [`ArtifactDigest::parse`].
    pub fn hex(&self) -> &str { self.0.strip_prefix("sha256:").unwrap_or(&self.0) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for ArtifactDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

/// Everything an attempt is pinned to. A result produced under different pins is
/// not publishable without re-validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pins {
    /// Shared contract revision the coordinator ran ([`crate::PROTOCOL_VERSION`]).
    pub protocol: String,
    /// Source commit the attempt starts from.
    pub source_commit: GitOid,
    /// Revision of the task's own contract (objective/acceptance), see [`crate::TaskContract`].
    pub task_contract_rev: u64,
    /// Revision of the project policy (limits, nonwaivable checks).
    pub policy_rev: u64,
}

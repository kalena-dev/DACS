//! DACS shared contract.
//!
//! Owner: Person 1 (coordinator track). Every change to this crate is a contract
//! revision: bump [`PROTOCOL_VERSION`], record it in `docs/contract.md` and notify
//! every affected track. Other tracks request changes; they do not edit this crate.
//!
//! Wire format is JSON over HTTP (see [`api::paths`]). Timestamps are Unix epoch
//! milliseconds (`*_ms: i64`). Enum values are `snake_case` strings.

/// Generates a string-backed enum whose serde names, `as_str`, `FromStr` and
/// `Display` agree, so the same spelling is used on the wire and in SQLite.
macro_rules! str_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $variant:ident = $s:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        pub enum $name { $($(#[$vmeta])* #[serde(rename = $s)] $variant,)+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];
            pub fn as_str(self) -> &'static str { match self { $($name::$variant => $s,)+ } }
        }
        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                match s { $($s => Ok($name::$variant),)+ other => Err(format!("unknown {} `{}`", stringify!($name), other)) }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

pub mod adapters;
pub mod api;
pub mod capacity;
pub mod enrollment;
pub mod envelope;
pub mod ids;
pub mod integration;
pub mod plan;
pub mod refusal;
pub mod revisions;
pub mod task;

pub use capacity::*;
pub use enrollment::*;
pub use envelope::*;
pub use ids::*;
pub use integration::*;
pub use plan::*;
pub use refusal::*;
pub use revisions::*;
pub use task::*;

/// Shared contract revision. Owned by Person 1. `-draft.N` until every owner has
/// confirmed its fields; compatibility is decided on `major.minor` only.
pub const PROTOCOL_VERSION: &str = "0.1.0-draft.1";

/// True when `other` shares this contract's `major.minor`.
pub fn is_compatible(other: &str) -> bool {
    fn major_minor(v: &str) -> Option<(&str, &str)> {
        let mut it = v.split(['.', '-']);
        Some((it.next()?, it.next()?))
    }
    matches!((major_minor(PROTOCOL_VERSION), major_minor(other)), (Some(a), Some(b)) if a == b)
}

/// Current Unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_is_major_minor() {
        assert!(is_compatible("0.1.0"));
        assert!(is_compatible("0.1.7-draft.3"));
        assert!(!is_compatible("0.2.0"));
        assert!(!is_compatible("garbage"));
    }

    #[test]
    fn str_enum_round_trips_on_wire_and_db() {
        for s in TaskState::ALL {
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(s.as_str().parse::<TaskState>().unwrap(), *s);
        }
    }

    #[test]
    fn work_kind_map_keys_serialize_as_strings() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(WorkKind::CompileUnit, 4u32);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"compile_unit":4}"#);
    }
}

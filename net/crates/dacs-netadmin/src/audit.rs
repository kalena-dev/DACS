//! Append-only JSON-lines audit of network operations. Never records secrets.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use time::OffsetDateTime;

pub struct Audit {
    file: Option<Mutex<File>>,
}

impl Audit {
    pub fn disabled() -> Self {
        Self { file: None }
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Some(Mutex::new(file)),
        })
    }

    pub fn record(&self, op: &str, detail: serde_json::Value) {
        let at = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let line = serde_json::json!({ "at": at, "op": op, "detail": detail });
        tracing::info!(target: "dacs_netadmin::audit", %line);
        if let Some(file) = &self.file {
            let mut f = file.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = writeln!(f, "{line}").and_then(|()| f.flush()) {
                tracing::error!(error = %e, "audit log write failed");
            }
        }
    }
}

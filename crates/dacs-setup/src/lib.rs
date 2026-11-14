//! Local-owner setup engine. Callers must restrict mutation endpoints to authenticated local
//! control and obtain explicit UI/CLI action; invitation display alone never invokes apply.
//! Package commands are reconstructed from this catalogue, never supplied by AI or invitations.
//! App updates trust a locally configured release key, not a key received with a release.
mod catalogue;
mod engine;
mod scan;
mod update;
mod util;

pub use catalogue::{catalog, detect, plan};
pub use engine::apply;
pub use scan::scan_project;
pub use update::{
    SignedUpdate, UpdateMetadata, apply_staged_update, reconcile_update, rollback_update, stage_update,
    update_signing_bytes, verify_update,
};

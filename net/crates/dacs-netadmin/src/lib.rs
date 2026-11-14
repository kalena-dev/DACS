//! DACS network enrollment adapter (Person 3).
//!
//! Runs on the infrastructure host beside Headscale and holds the Headscale
//! API key there. Person 1's coordinator calls it with a scoped operation
//! token after project/device authorization and the owner's consent.

pub mod audit;
pub mod config;
pub mod headscale;
pub mod http;
pub mod service;

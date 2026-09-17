//! The official ACP Registry adapter.
//!
//! The registry is an outside input, so this module keeps three rules:
//! parsing is explicit and reports what it rejects, the last good catalog
//! stays on disk, and an installed agent launches from a snapshot rather than
//! from the live registry.
pub mod archive;
pub mod client;
pub mod install;
pub mod manifest;
pub mod platform;

pub use archive::{sha256_hex, verify_sha256, ArchiveKind};
pub use client::{
    default_fetch, CacheMetadata, CachedCatalog, HttpFetch, HttpsFetch, RegistryClient,
    UnavailableFetch, DEFAULT_REGISTRY_URL,
};
pub use install::{
    ensure_pinned, install_directory, prepare, prepare_with_progress, select, split_package,
    InstallPlan, PreparedInstall,
};
pub use manifest::{
    parse_catalog, BinaryTarget, DistributionKind, PackageDistribution, RegistryAgent,
    RegistryCatalog, RegistryDistribution, RegistryRejection,
};
pub use platform::{PlatformTarget, ALL_PLATFORM_TARGETS};

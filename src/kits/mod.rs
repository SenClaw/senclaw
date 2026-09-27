//! Zen Kits — installable bundles of the daemon's own building blocks.
//!
//! A kit is "a Space App bundle without an app": one JSON manifest that
//! declares personas, skills, workflows, hooks and scheduled jobs, installed
//! in one call and removable in one call.
//!
//! ```text
//! manifest ──parse──▶ KitManifest ──install_kit──▶ files + rows + receipt
//!                                  ──uninstall_kit──▶ (reads the receipt back)
//! ```
//!
//! Where things go and what is deliberately left to a client (MCP servers,
//! apps) is documented on [`installer`]; why kit hooks live in their own file
//! and can only ever be prompt hooks is on [`hooks`].

pub mod builtin;
pub mod bundle;
pub mod hooks;
pub mod installer;
pub mod manifest;
pub mod params;
pub mod receipt;

pub use bundle::{BundleApp, BundleFile, KitBundle, KitBundleError};
pub use hooks::{kit_hook_files, kit_hook_path, KitHookOutcome};
pub use installer::{
    install_bundle_with_params, install_kit, kit_app_ids, uninstall_kit, uninstall_kit_with_extra,
    KitContext, KitInstallReport, KitItemStatus, KitRemoveOutcome, KitRemoveStatus,
    KitUninstallReport,
};
pub use manifest::{KitManifest, KitManifestError, KitWarning, KIT_MANIFEST_VERSION};
pub use params::{
    resolve_values, KitParam, KitParamError, KitParamType, KitParamValues,
};
pub use receipt::{KitItemKind, KitReceipt, KitReceiptStore};

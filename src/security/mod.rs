//! Security controls that run before untrusted third-party code is trusted,
//! and runtime controls that contain a compromised agent.
//!
//! [`scan`] is the pre-install scanner: it inspects a marketplace plugin
//! directory or an extracted Space App *before* the daemon does anything that
//! executes its contents.
//!
//! [`egress`] and [`replication`] are the runtime half — they assume injection
//! has already succeeded and cut the worm's propagation path. Threat model and
//! evidence: [`docs/agent-security-hooks.md`](../../docs/agent-security-hooks.md).
//!
//! ## Nguyên tắc chung
//!
//! Enforcement nằm ở Rust, không nằm ở prompt. Hook có thể *phát hiện* và *báo cáo*,
//! nhưng ranh giới chặn thật sự phải là code — bất cứ thứ gì nằm trong prompt đều có thể
//! bị nói vòng. Cùng mô hình với `apps/crm/src/guardrail.rs`.

pub mod egress;
pub mod replication;
pub mod scan;

pub use egress::{gate, guard, record_inbound, EgressGuard, GuardConfig};
pub use replication::{is_replicating, score, Scores};
// `Verdict` is deliberately not re-exported: `scan::Verdict` (install decision)
// and `egress::Verdict` (send decision) are different judgements, and a single
// unqualified name at this level would blur them. Import them qualified.
pub use scan::{scan_plugin_dir, scan_space_app, Finding, ScanPolicy, ScanReport, Severity};

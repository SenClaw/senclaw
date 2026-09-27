//! Typed decisions — questions answered with probabilities, not generated text.
//!
//! A caller hands over one `state` (a string, or JSON the model reads as
//! text) and a map of typed questions: `choice` (pick one label), `score`
//! (place the state on an ordered rubric) or `noul` (how likely a statement
//! holds). The answer to each is a distribution, never prose, so it can be
//! branched on directly. This is the `/v1/systemone` shape TypeSafe defined
//! for Jev and the open Laya checkpoints reuse.
//!
//! The engine — a local Laya checkpoint or a hosted `/v1/systemone` backend
//! (TypeSafe Jev, Cloudflare, any compatible URL) — is no longer in the
//! daemon: it is the `sen-sysone` runtime, installed and supervised like every
//! other engine (`crate::runtime`, `docs/runtime-protocol.md` §4.3). [`client`]
//! is the daemon's HTTP client to it. [`settings`] keeps only what stays
//! daemon-side: the tool-call [`gate`] and the pre-turn [`skill_route`]
//! router, which may approve an agent's shell command before it prompts, or
//! pick a skill before the main turn.

pub mod client;
pub mod gate;
pub mod json;
pub mod settings;
pub mod skill_route;
pub mod types;

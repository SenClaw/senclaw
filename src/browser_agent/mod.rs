//! The browser agent's decision loop — the *brain* half of SenBrowser.
//!
//! The `sen-browser` runtime observes pages and executes guarded actions; this
//! module decides what to do next, in the control plane's order:
//! **rule → Jev → LLM → person**. One request to the decision runtime asks
//! for the operation *and* every operation's target at once ([`encoder`]); a
//! confident answer acts, an unsure one goes to the LLM tier, a risky action
//! waits for the person ([`policy`]), and a DONE only counts once the page
//! proves it ([`run`]).

pub mod budget;
pub mod decide;
pub mod encoder;
pub mod extension;
pub mod llm;
pub mod policy;
pub mod ports;
pub mod prompts;
pub mod rest;
pub mod run;
pub mod settings;

//! Interactive permission setup at startup. Mirrors `src-old/setup.ts`.
//!
//! Called before `run_daemon()`:
//!   - First launch (no permission config): show default policy, ask whether to configure
//!   - Existing config: show current status, ask whether to reconfigure
//!   - Non-TTY environment (CI/background process): skip silently
//!   - No input for 2 minutes: continue with current/default policy

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::gateway::group_manager::{
    get_admin_permissions_config, save_admin_permissions_config, AdminPermissions,
};

const SETUP_TIMEOUT_SECS: u64 = 120;

fn is_first_time(cfg: &AdminPermissions) -> bool {
    !cfg.skip_main_agent_permissions && !cfg.skip_all_agents_permissions
}

fn describe(cfg: &AdminPermissions) -> &'static str {
    if cfg.skip_all_agents_permissions {
        "All agents bypass approval (including dispatch subagents)"
    } else if cfg.skip_main_agent_permissions {
        "Main agent bypasses approval; other agents still require approval"
    } else {
        "All agents require permission approval (default, safest)"
    }
}

fn read_line_with_timeout(timeout_secs: u64) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut line = String::new();
        let _ = stdin.lock().read_line(&mut line);
        let _ = tx.send(line);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(line) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

fn ask_yes_no(prompt: &str, timeout_secs: u64) -> Option<bool> {
    print!("{prompt} (y/N): ");
    io::stdout().flush().ok();
    match read_line_with_timeout(timeout_secs) {
        None => {
            crate::safe_println!("\nTimed out.");
            None
        }
        Some(line) => {
            let lower = line.to_lowercase();
            if lower.starts_with('y') {
                Some(true)
            } else {
                Some(false)
            }
        }
    }
}

fn select_policy() -> Option<String> {
    crate::safe_println!("\nSelect permission policy:");
    crate::safe_println!("  [1] All agents require approval (safest, default)");
    crate::safe_println!("  [2] Main agent bypasses approval");
    crate::safe_println!("  [3] All agents bypass approval");
    print!("Choice (1/2/3): ");
    io::stdout().flush().ok();

    match read_line_with_timeout(SETUP_TIMEOUT_SECS) {
        None => {
            crate::safe_println!("\nTimed out.");
            None
        }
        Some(line) => match line.trim() {
            "1" => Some("strict".to_string()),
            "2" => Some("main".to_string()),
            "3" => Some("all".to_string()),
            _ => None,
        },
    }
}

/// Run the interactive setup wizard if needed. Safe to call in non-TTY
/// environments (returns immediately).
pub fn run_setup_if_needed(config_path: &Path) {
    // Skip in non-interactive environments
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return;
    }

    let cfg = get_admin_permissions_config(config_path);
    let first_time = is_first_time(&cfg);

    crate::safe_println!("\n=== SenClaw Setup ===\n");

    if first_time {
        crate::safe_println!(
            "No permission policy found. Default policy: all agents require approval."
        );
        match ask_yes_no(
            "Configure permission policy now? (Will auto-skip after 2 minutes of inactivity)",
            SETUP_TIMEOUT_SECS,
        ) {
            None => {
                crate::safe_println!("Timed out. Using default policy (approval required). Change it later in WebUI settings.");
                crate::safe_println!("Starting...");
                return;
            }
            Some(false) => {
                crate::safe_println!("Skipped setup. Using default policy (approval required).");
                crate::safe_println!("Starting...");
                return;
            }
            Some(true) => {} // continue to policy selection
        }
    } else {
        crate::safe_println!("Current permission policy: {}", describe(&cfg));
        crate::safe_println!("Starting...");
        return;
    }

    // Policy selection
    let choice = match select_policy() {
        Some(c) => c,
        None => {
            crate::safe_println!("Cancelled. Keeping existing settings.");
            crate::safe_println!("Starting...");
            return;
        }
    };

    let next = AdminPermissions {
        skip_main_agent_permissions: choice == "main" || choice == "all",
        skip_all_agents_permissions: choice == "all",
    };

    if let Err(e) = save_admin_permissions_config(config_path, &next) {
        crate::safe_eprintln!("Failed to save permission config: {e}");
    } else {
        crate::safe_println!("Saved: {}", describe(&next));
    }
    crate::safe_println!("Starting...");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_first_time() {
        let cfg = AdminPermissions {
            skip_main_agent_permissions: false,
            skip_all_agents_permissions: false,
        };
        assert!(is_first_time(&cfg));
    }

    #[test]
    fn test_is_not_first_time() {
        let cfg = AdminPermissions {
            skip_main_agent_permissions: true,
            skip_all_agents_permissions: false,
        };
        assert!(!is_first_time(&cfg));
    }

    #[test]
    fn test_describe() {
        assert_eq!(
            describe(&AdminPermissions {
                skip_main_agent_permissions: false,
                skip_all_agents_permissions: false,
            }),
            "All agents require permission approval (default, safest)"
        );
        assert_eq!(
            describe(&AdminPermissions {
                skip_main_agent_permissions: true,
                skip_all_agents_permissions: false,
            }),
            "Main agent bypasses approval; other agents still require approval"
        );
        assert_eq!(
            describe(&AdminPermissions {
                skip_main_agent_permissions: true,
                skip_all_agents_permissions: true,
            }),
            "All agents bypass approval (including dispatch subagents)"
        );
    }
}

//! Built-in virtual agent personas. Mirrors `src-old/subagents/builtin-personas.ts`.
//!
//! Auto-installed to the virtual-agents directory on startup.
//! Existing .md files are skipped to preserve user customizations.

use std::fs;
use std::path::Path;

use tracing;

struct BuiltinPersona {
    filename: &'static str, // without .md suffix
    content: &'static str,  // full .md content (frontmatter + body)
}

const BUILTIN_PERSONAS: &[BuiltinPersona] = &[
    BuiltinPersona {
        filename: "general-assistant",
        content: include_str!("../../assets/builtin-personas/general-assistant.md"),
    },
    BuiltinPersona {
        filename: "reflector",
        content: include_str!("../../assets/builtin-personas/reflector.md"),
    },
    BuiltinPersona {
        filename: "product-director",
        content: include_str!("../../assets/builtin-personas/product-director.md"),
    },
    BuiltinPersona {
        filename: "copywriter",
        content: include_str!("../../assets/builtin-personas/copywriter.md"),
    },
    BuiltinPersona {
        filename: "research-assistant",
        content: include_str!("../../assets/builtin-personas/research-assistant.md"),
    },
    BuiltinPersona {
        filename: "browser-agent",
        content: include_str!("../../assets/builtin-personas/browser-agent.md"),
    },
    BuiltinPersona {
        filename: "space-assistant",
        content: include_str!("../../assets/builtin-personas/space-assistant.md"),
    },
    BuiltinPersona {
        filename: "ocr-worker",
        content: include_str!("../../assets/builtin-personas/ocr-worker.md"),
    },
    BuiltinPersona {
        filename: "js-sandbox",
        content: include_str!("../../assets/builtin-personas/js-sandbox.md"),
    },
    BuiltinPersona {
        filename: "bash-sandbox",
        content: include_str!("../../assets/builtin-personas/bash-sandbox.md"),
    },
];

/// Install builtin personas to `virtual_agents_dir`.
/// Skips files that already exist (preserves user customizations).
pub fn install_builtin_personas(virtual_agents_dir: &Path) {
    if let Err(e) = fs::create_dir_all(virtual_agents_dir) {
        tracing::warn!(
            "[BuiltinPersonas] Failed to create dir {:?}: {e}",
            virtual_agents_dir
        );
        return;
    }

    for persona in BUILTIN_PERSONAS {
        let file_path = virtual_agents_dir.join(format!("{}.md", persona.filename));
        if file_path.exists() {
            continue;
        }
        match fs::write(&file_path, persona.content) {
            Ok(()) => {
                tracing::info!("[BuiltinPersonas] Installed: {}.md", persona.filename);
            }
            Err(e) => {
                tracing::warn!(
                    "[BuiltinPersonas] Failed to install {}.md: {e}",
                    persona.filename
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_personas_have_content() {
        for p in BUILTIN_PERSONAS {
            assert!(!p.content.is_empty(), "{} should have content", p.filename);
            assert!(
                p.content.contains("---"),
                "{} should have frontmatter",
                p.filename
            );
        }
    }

    #[test]
    fn test_install_creates_files() {
        let tmp = std::env::temp_dir().join(format!("test-personas-{}", uuid::Uuid::new_v4()));
        install_builtin_personas(&tmp);

        for p in BUILTIN_PERSONAS {
            let file_path = tmp.join(format!("{}.md", p.filename));
            assert!(file_path.exists(), "{} should exist", p.filename);
            let content = fs::read_to_string(&file_path).unwrap();
            assert_eq!(content, p.content);
        }

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_install_skips_existing() {
        let tmp = std::env::temp_dir().join(format!("test-personas-skip-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();

        // Pre-create a file with custom content
        let existing = tmp.join("general-assistant.md");
        let custom = "---\nname: custom\n---\n\nCustom content";
        fs::write(&existing, custom).unwrap();

        install_builtin_personas(&tmp);

        // Existing file should be untouched
        let content = fs::read_to_string(&existing).unwrap();
        assert_eq!(content, custom);

        // New files should be created
        assert!(tmp.join("reflector.md").exists());

        fs::remove_dir_all(&tmp).ok();
    }
}

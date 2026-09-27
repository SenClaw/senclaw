use serde::{Deserialize, Serialize};

// ===== AdminPermissionsConfig =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminPermissionsConfig {
    #[serde(rename = "skipMainAgentPermissions")]
    pub skip_main_agent_permissions: bool,
    #[serde(rename = "skipAllAgentsPermissions")]
    pub skip_all_agents_permissions: bool,
}

impl Default for AdminPermissionsConfig {
    fn default() -> Self {
        Self {
            skip_main_agent_permissions: false,
            skip_all_agents_permissions: false,
        }
    }
}

// ===== MIME types =====

pub(crate) const MIME: &[(&str, &str)] = &[
    (".html", "text/html; charset=utf-8"),
    (".js", "application/javascript"),
    (".mjs", "application/javascript"),
    (".css", "text/css"),
    (".svg", "image/svg+xml"),
    (".png", "image/png"),
    (".ico", "image/x-icon"),
    (".json", "application/json"),
    (".woff2", "font/woff2"),
    (".woff", "font/woff"),
    (".ttf", "font/ttf"),
    (".md", "text/markdown; charset=utf-8"),
    (".markdown", "text/markdown; charset=utf-8"),
    (".txt", "text/plain; charset=utf-8"),
    (".csv", "text/csv; charset=utf-8"),
    (".xml", "text/xml; charset=utf-8"),
    (".yml", "text/yaml; charset=utf-8"),
    (".yaml", "text/yaml; charset=utf-8"),
    (".ts", "application/typescript"),
    (".tsx", "application/typescript"),
    (".jsx", "application/javascript"),
    (".py", "text/x-python"),
    (".rs", "text/x-rust"),
    (".go", "text/x-go"),
    (".java", "text/x-java"),
    (".c", "text/x-c"),
    (".cpp", "text/x-c++"),
    (".h", "text/x-c-header"),
    (".sh", "text/x-shellscript"),
    (".bash", "text/x-shellscript"),
    (".zsh", "text/x-shellscript"),
    (".ps1", "text/x-powershell"),
    (".sql", "text/x-sql"),
    (".log", "text/plain; charset=utf-8"),
];

pub(crate) fn path_to_mime(path: &str) -> &'static str {
    if let Some(pos) = path.rfind('.') {
        let ext = &path[pos..];
        for (e, m) in MIME {
            if *e == ext {
                return m;
            }
        }
    }
    "application/octet-stream"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mime_lookup() {
        assert_eq!(path_to_mime("index.html"), "text/html; charset=utf-8");
        assert_eq!(path_to_mime("app.js"), "application/javascript");
        assert_eq!(path_to_mime("style.css"), "text/css");
        assert_eq!(path_to_mime("unknown.xyz"), "application/octet-stream");
    }

    #[test]
    fn test_admin_perms_default() {
        let cfg = AdminPermissionsConfig::default();
        assert!(!cfg.skip_main_agent_permissions);
        assert!(!cfg.skip_all_agents_permissions);
    }
}

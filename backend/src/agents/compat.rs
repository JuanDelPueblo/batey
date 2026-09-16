//! Headless authentication compatibility for known Registry agents.
//!
//! Upstream agents behave differently in a browserless container. This module
//! centralizes those defaults so generic ACP/auth/session code never checks a
//! provider name. Matching uses the official Registry id from the installed
//! snapshot, never a Batey catalog id or an agent name.
//!
//! Precedence: compatibility defaults lose to user-configured T131 per-agent
//! overrides. The auth service applies them before overrides, and terminal
//! method env applies after.

use std::collections::HashMap;

/// The kind of authentication process a default applies to.
///
/// `Auth` covers the authentication probe and protocol-auth processes.
/// `Terminal` covers a terminal-auth child. They differ because upstream
/// constraints differ: Codex needs `NO_BROWSER` only where it chooses an
/// authentication method, while Copilot needs `CI` for terminal auth too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthEnvScope {
    Auth,
    Terminal,
}

/// Official Registry id for the Codex ACP adapter.
pub const CODEX_REGISTRY_ID: &str = "codex-acp";
/// Official Registry id for the GitHub Copilot CLI adapter.
pub const COPILOT_REGISTRY_ID: &str = "github-copilot-cli";
/// Official Registry id for Google Antigravity.
pub const ANTIGRAVITY_REGISTRY_ID: &str = "antigravity-acp";
/// Official Registry id for OpenCode (binary install reference).
pub const OPENCODE_REGISTRY_ID: &str = "opencode";

/// Compatibility activated for a protocol auth flow. The plumbing is generic;
/// this module is the only place that decides which Registry id opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolAuthCompatibility {
    pub intercept_browser: bool,
    pub capture_auth_stderr_url: bool,
}

/// Returns the compatibility path for one official Registry id. A catalog
/// alias, display name, or custom agent never matches this function.
pub fn protocol_auth_compatibility(registry_id: Option<&str>) -> Option<ProtocolAuthCompatibility> {
    (registry_id == Some(ANTIGRAVITY_REGISTRY_ID)).then_some(ProtocolAuthCompatibility {
        intercept_browser: true,
        capture_auth_stderr_url: true,
    })
}

/// Headless environment defaults for one Registry id and process scope.
///
/// Only known ids return entries. Callers apply these before T131 user
/// overrides so an explicit user value wins.
pub fn auth_env_defaults(
    registry_id: Option<&str>,
    scope: AuthEnvScope,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    match registry_id {
        Some(id) if id == CODEX_REGISTRY_ID && scope == AuthEnvScope::Auth => {
            // Makes Codex advertise/use headless-suitable methods in the
            // browserless backend. Probe and protocol-auth processes only;
            // ordinary chat sessions and terminal auth never receive this.
            out.insert("NO_BROWSER".to_string(), "1".to_string());
        }
        Some(id) if id == COPILOT_REGISTRY_ID => {
            // Makes upstream Copilot choose its headless/device-code path
            // instead of a loopback-browser callback in containers. Applies
            // to the auth and terminal-auth processes only, never to ordinary
            // chat sessions. Never appends --device-code; upstream args stay
            // as advertised.
            out.insert("CI".to_string(), "true".to_string());
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_gets_no_browser_for_auth_only() {
        let auth = auth_env_defaults(Some(CODEX_REGISTRY_ID), AuthEnvScope::Auth);
        assert_eq!(auth.get("NO_BROWSER").map(String::as_str), Some("1"));
        assert_eq!(auth.len(), 1);
        // A terminal-auth child never receives the Codex default.
        assert!(auth_env_defaults(Some(CODEX_REGISTRY_ID), AuthEnvScope::Terminal).is_empty());
    }

    #[test]
    fn copilot_gets_ci_for_auth_and_terminal() {
        for scope in [AuthEnvScope::Auth, AuthEnvScope::Terminal] {
            let defaults = auth_env_defaults(Some(COPILOT_REGISTRY_ID), scope);
            assert_eq!(defaults.get("CI").map(String::as_str), Some("true"));
            assert_eq!(defaults.len(), 1);
        }
    }

    #[test]
    fn unknown_agents_get_no_defaults() {
        for scope in [AuthEnvScope::Auth, AuthEnvScope::Terminal] {
            assert!(auth_env_defaults(None, scope).is_empty());
            assert!(auth_env_defaults(Some("other"), scope).is_empty());
            // Catalog ids never match; only Registry ids do.
            assert!(auth_env_defaults(Some("codex"), scope).is_empty());
        }
    }

    #[test]
    fn antigravity_protocol_compatibility_is_registry_scoped() {
        assert_eq!(
            protocol_auth_compatibility(Some(ANTIGRAVITY_REGISTRY_ID)),
            Some(ProtocolAuthCompatibility {
                intercept_browser: true,
                capture_auth_stderr_url: true,
            })
        );
        assert!(protocol_auth_compatibility(Some("Antigravity")).is_none());
        assert!(protocol_auth_compatibility(None).is_none());
    }

    #[test]
    fn other_agents_have_no_protocol_compatibility() {
        assert!(protocol_auth_compatibility(Some(CODEX_REGISTRY_ID)).is_none());
        assert!(protocol_auth_compatibility(Some(COPILOT_REGISTRY_ID)).is_none());
        assert!(protocol_auth_compatibility(None).is_none());
    }
}

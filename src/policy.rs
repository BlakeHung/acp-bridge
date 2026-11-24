//! Tool policy — capability-based allow / confirm / deny (P1/E12 first slice).
//!
//! Every tool call passes through [`Policy::decide`] before executing:
//!
//! - `allow` — run it.
//! - `confirm` — run it, but emit a visible in-session notice first.
//!   This is the documented fallback while OpenAB exposes no
//!   interactive `session/request_permission` channel: confirm can
//!   never mean "silently proceed" nor can it block forever on an
//!   approval nobody can give.
//! - `deny` — do not run; return a structured `Denied` tool error the
//!   model can read (so it can pick a different approach rather than
//!   retry blindly).
//!
//! On top of the per-capability rules there is a path-level deny list
//! (`deny_path_globs`) that applies to file tools' `path` argument —
//! credential-shaped files (`.env`, `*.pem`, `.ssh/**`) are refused even
//! inside an otherwise-writable workspace.

use serde::Deserialize;
use serde_json::Value;

use crate::llm::ConfigError;

/// A tool's privilege class. Mapped from the tool name — the model
/// cannot name a capability directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Inspect workspace files (read_file, list_dir, search_code).
    Read,
    /// Modify workspace files (write_file, edit).
    Write,
    /// Run commands (bash, git_*).
    Shell,
    /// Reach the network (web_fetch).
    Web,
}

/// What the policy lets happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    /// Execute after emitting a visible notice naming what will run.
    /// With no interactive permission channel this is the honest
    /// middle ground — it never silently proceeds and never waits
    /// forever on an answer that cannot arrive.
    Confirm,
    /// Refuse to execute; the model gets a Denied tool error.
    Deny,
}

impl Action {
    fn parse(s: &str, source: &'static str) -> Result<Self, ConfigError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "confirm" => Ok(Self::Confirm),
            "deny" => Ok(Self::Deny),
            other => Err(ConfigError::InvalidEnv {
                var: source,
                value: other.into(),
                reason: "expected allow | confirm | deny".into(),
            }),
        }
    }
}

/// Outcome of evaluating a tool call.
#[derive(Debug)]
pub enum Decision {
    Allow,
    /// Proceed, but surface this notice to the Client first.
    Confirm(String),
    /// Do not execute; return this reason as the tool error.
    Deny(String),
}

/// `[policy]` config-file section — strict like the rest of the
/// config: an unknown field is a startup error, not a silent default.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    /// Action for read tools (default allow).
    pub read: Option<String>,
    /// Action for write tools (default allow — file writes are the
    /// coding agent's bread and butter; path denies still apply).
    pub write: Option<String>,
    /// Action for shell tools (default confirm — arbitrary commands
    /// get a visible notice).
    pub shell: Option<String>,
    /// Action for network tools (default confirm — outbound traffic
    /// is an exfiltration surface even with the allowlist).
    pub web: Option<String>,
    /// Glob patterns for workspace-relative paths the file tools must
    /// refuse even when writes are allowed. `*` does not cross `/`,
    /// `**` crosses directories, `?` is one char.
    #[serde(default)]
    pub deny_path_globs: Option<Vec<String>>,
}

/// Resolved policy, checked on every tool dispatch.
#[derive(Debug, Clone)]
pub struct Policy {
    pub read: Action,
    pub write: Action,
    pub shell: Action,
    pub web: Action,
    pub deny_path_globs: Vec<String>,
}

/// Default deny list — credential-shaped files refuse inside the
/// sandbox even for the permissive daily-driver profile.
pub const DEFAULT_DENY_PATH_GLOBS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    ".ssh/**",
    ".aws/**",
    ".gnupg/**",
];

impl Policy {
    /// Default for tests and for `AppState::new` — read/write allow,
    /// shell/web confirm, the default credential deny list.
    pub fn default_policy() -> Self {
        Self {
            read: Action::Allow,
            write: Action::Allow,
            shell: Action::Confirm,
            web: Action::Confirm,
            deny_path_globs: DEFAULT_DENY_PATH_GLOBS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// Merge `[policy]` section + `LLM_POLICY_*` env vars.
    /// Env wins over file; a present-but-unparseable value is a
    /// startup error, same contract as the `LLM_*` vars.
    pub fn from_section_and_env(file: &PolicySection) -> Result<Self, ConfigError> {
        let pick = |env_name: &'static str,
                    file_val: &Option<String>,
                    default: Action|
         -> Result<Action, ConfigError> {
            if let Ok(raw) = std::env::var(env_name) {
                return Action::parse(&raw, env_name);
            }
            if let Some(raw) = file_val {
                return Action::parse(raw, "[policy] file field");
            }
            Ok(default)
        };

        let defaults = Self::default_policy();
        let deny_path_globs = std::env::var("LLM_POLICY_DENY_PATHS")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .or_else(|| file.deny_path_globs.clone())
            .unwrap_or(defaults.deny_path_globs);

        Ok(Self {
            read: pick("LLM_POLICY_READ", &file.read, defaults.read)?,
            write: pick("LLM_POLICY_WRITE", &file.write, defaults.write)?,
            shell: pick("LLM_POLICY_SHELL", &file.shell, defaults.shell)?,
            web: pick("LLM_POLICY_WEB", &file.web, defaults.web)?,
            deny_path_globs,
        })
    }

    /// Which capability a tool name maps to. Unknown tools are
    /// denied outright — an unmapped tool is a surface nobody
    /// classified, not an implicit allow.
    fn capability_of(name: &str) -> Option<Capability> {
        match name {
            "read_file" | "list_dir" | "search_code" => Some(Capability::Read),
            "write_file" | "edit" => Some(Capability::Write),
            "bash" | "git_status" | "git_diff" | "git_log" | "git_commit" => {
                Some(Capability::Shell)
            }
            "web_fetch" => Some(Capability::Web),
            _ => None,
        }
    }

    /// Evaluate a tool call before dispatch.
    pub fn decide(&self, name: &str, args: &Value) -> Decision {
        let Some(cap) = Self::capability_of(name) else {
            return Decision::Deny(format!("tool '{name}' is not a registered tool"));
        };

        // Path-level denies apply to file tools before the capability
        // action is even consulted — a denied path can never be
        // reached by an allow-ed tool either.
        if matches!(cap, Capability::Read | Capability::Write) {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                let rel = path.trim_start_matches('/');
                if self.deny_path_globs.iter().any(|g| glob_matches(g, rel)) {
                    return Decision::Deny(format!(
                        "path '{rel}' is denied by policy (deny_path_globs)"
                    ));
                }
            }
        }

        match cap {
            Capability::Read => match self.read {
                Action::Allow => Decision::Allow,
                Action::Confirm => Decision::Confirm(format!(
                    "policy requires confirmation for read tool '{name}'"
                )),
                Action::Deny => Decision::Deny(format!("policy denies read tool '{name}'")),
            },
            Capability::Write => match self.write {
                Action::Allow => Decision::Allow,
                Action::Confirm => Decision::Confirm(format!(
                    "policy requires confirmation for write tool '{name}'"
                )),
                Action::Deny => Decision::Deny(format!("policy denies write tool '{name}'")),
            },
            Capability::Shell => match self.shell {
                Action::Allow => Decision::Allow,
                Action::Confirm => Decision::Confirm(format!(
                    "policy requires confirmation for shell tool '{name}'"
                )),
                Action::Deny => Decision::Deny(format!("policy denies shell tool '{name}'")),
            },
            Capability::Web => match self.web {
                Action::Allow => Decision::Allow,
                Action::Confirm => Decision::Confirm(format!(
                    "policy requires confirmation for network tool '{name}'"
                )),
                Action::Deny => Decision::Deny(format!("policy denies network tool '{name}'")),
            },
        }
    }
}

/// Minimal glob matcher: `*` = any run except `/`, `**` = any run
/// including `/`, `?` = exactly one char, everything else literal.
/// A pattern with no `/` matches the path's basename at any depth —
/// gitignore semantics, so `*.pem` covers `secrets/a.pem` too.
/// Enough for the deny list without pulling in a glob crate.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let pth: Vec<char> = path.chars().collect();
    if glob_match(&pat, &pth) {
        return true;
    }
    if !pattern.contains('/') {
        if let Some(pos) = path.rfind('/') {
            return glob_match(&pat, &pth[pos + 1..]);
        }
    }
    false
}

fn glob_match(pat: &[char], pth: &[char]) -> bool {
    if pat.is_empty() {
        return pth.is_empty();
    }
    match pat[0] {
        '*' if pat.len() > 1 && pat[1] == '*' => {
            // `**` matches zero or more chars including `/`.
            let rest = &pat[2..];
            // skip optional leading `/` after `**`
            let rest = if rest.first() == Some(&'/') {
                &rest[1..]
            } else {
                rest
            };
            for i in 0..=pth.len() {
                if glob_match(rest, &pth[i..]) {
                    return true;
                }
            }
            false
        }
        '*' => {
            let rest = &pat[1..];
            for i in 0..=pth.len() {
                // `*` does not cross `/`
                if pth[..i].contains(&'/') {
                    break;
                }
                if glob_match(rest, &pth[i..]) {
                    return true;
                }
            }
            false
        }
        '?' => !pth.is_empty() && glob_match(&pat[1..], &pth[1..]),
        c => !pth.is_empty() && pth[0] == c && glob_match(&pat[1..], &pth[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_shape() {
        let p = Policy::default_policy();
        assert_eq!(p.read, Action::Allow);
        assert_eq!(p.write, Action::Allow);
        assert_eq!(p.shell, Action::Confirm);
        assert_eq!(p.web, Action::Confirm);
    }

    #[test]
    fn capability_mapping() {
        assert_eq!(Policy::capability_of("read_file"), Some(Capability::Read));
        assert_eq!(Policy::capability_of("bash"), Some(Capability::Shell));
        assert_eq!(Policy::capability_of("web_fetch"), Some(Capability::Web));
        assert_eq!(Policy::capability_of("not_a_tool"), None);
    }

    #[test]
    fn deny_glob_catches_credential_files() {
        let p = Policy::default_policy();
        for path in [".env", ".env.local", "cert.pem", "key.key", ".ssh/id_rsa"] {
            let d = p.decide("read_file", &serde_json::json!({"path": path}));
            assert!(
                matches!(d, Decision::Deny(_)),
                "expected deny for {path}, got {d:?}"
            );
        }
        // Ordinary source files pass through.
        let d = p.decide("read_file", &serde_json::json!({"path": "src/main.rs"}));
        assert!(matches!(d, Decision::Allow), "got {d:?}");
    }

    #[test]
    fn deny_glob_only_applies_to_file_tools() {
        let p = Policy::default_policy();
        // bash isn't path-checked (its `command` arg isn't a path arg)
        let d = p.decide("bash", &serde_json::json!({"command": "cat .env"}));
        assert!(
            matches!(d, Decision::Confirm(_)),
            "bash should hit shell confirm, got {d:?}"
        );
    }

    #[test]
    fn glob_basics() {
        assert!(glob_matches("*.pem", "a.pem"));
        assert!(glob_matches("*.pem", "dir/a.pem"));
        assert!(!glob_matches("*.pem", "a.pem.bak"));
        assert!(glob_matches(".ssh/**", ".ssh/id_rsa"));
        assert!(glob_matches(".env", ".env"));
        assert!(!glob_matches(".env", ".envx"));
        assert!(glob_matches(".env.*", ".env.local"));
    }

    #[test]
    fn deny_action_blocks_tool() {
        let mut p = Policy::default_policy();
        p.shell = Action::Deny;
        let d = p.decide("bash", &serde_json::json!({"command": "ls"}));
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }
}

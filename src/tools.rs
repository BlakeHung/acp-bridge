//! Built-in tools for acp-bridge — file reading, directory listing, code search.
//! All tools are sandboxed to the working directory.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Maximum file size to read (1 MB).
const MAX_FILE_SIZE: u64 = 1024 * 1024;
/// Maximum directory listing depth.
const MAX_LIST_DEPTH: usize = 3;
/// Maximum entries in directory listing.
const MAX_LIST_ENTRIES: usize = 200;
/// Maximum chars of combined tool output returned to the model.
const MAX_TOOL_OUTPUT_CHARS: usize = 50_000;

/// Error classification for a tool result. The `text` field on
/// [`ToolResult`] stays model-facing prose; `kind` is for the engine —
/// budgeting, streak detection, policy decisions — without parsing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Missing or malformed arguments.
    InvalidArgs,
    /// Path resolves outside the working directory (or via a symlink).
    OutOfSandbox,
    /// Target does not exist / has the wrong type.
    NotFound,
    /// Input or output exceeded a configured limit.
    TooLarge,
    /// Policy or allowlist denied the operation.
    Denied,
    /// The operation exceeded its time budget and was terminated.
    Timeout,
    /// The command ran and exited non-zero / the OS call failed.
    ExecFailed,
    /// Network or remote-side failure (HTTP error, connect error).
    Network,
    /// Tool name is not registered.
    UnknownTool,
}

/// Structured tool result (P1/E10 first slice). `text` remains the
/// model-facing content stored in history; the structured fields let the
/// engine reason about failures without string matching.
pub struct ToolResult {
    /// Model-facing result text (error text included when `error_kind`
    /// is `Some`).
    pub text: String,
    /// Process exit code when the tool ran a subprocess.
    pub exit_code: Option<i32>,
    /// Structured error classification; `None` on success.
    pub error_kind: Option<ToolErrorKind>,
    /// `text` was cut to the output cap — the model sees a marker and can
    /// ask for a narrower read instead of silently missing the tail.
    pub truncated: bool,
}

impl ToolResult {
    fn ok(text: String) -> Self {
        Self {
            text,
            exit_code: None,
            error_kind: None,
            truncated: false,
        }
    }

    pub(crate) fn err(kind: ToolErrorKind, text: String) -> Self {
        Self {
            text,
            exit_code: None,
            error_kind: Some(kind),
            truncated: false,
        }
    }
}

impl From<ToolResult> for String {
    fn from(r: ToolResult) -> String {
        r.text
    }
}

/// Truncate a string to at most `max` bytes without splitting a UTF-8
/// code point. `String::truncate` panics when the cut lands mid-char —
/// tool output in CJK hits that panic path.
fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Truncate tool output to the shared cap, UTF-8-safe, and flag it.
fn cap_output(s: &mut String) -> bool {
    if s.len() <= MAX_TOOL_OUTPUT_CHARS {
        return false;
    }
    truncate_utf8(s, MAX_TOOL_OUTPUT_CHARS);
    s.push_str("\n... (truncated)");
    true
}

fn is_ignored_entry(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "__pycache__")
}

/// Tool definitions in OpenAI/Ollama function calling format.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file. Returns the file content as text. Use this to examine source code, configuration files, or any text file in the project.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file from the working directory (e.g. 'src/main.rs', 'package.json')"
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files and directories at a given path. Returns a tree-like structure showing the directory contents. Use this to understand project structure.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the directory from the working directory (e.g. 'src', '.')"
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Search for a pattern in files within the working directory. Returns matching lines with file paths and line numbers. Use this to find function definitions, usages, or specific code patterns.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Text pattern to search for (plain text, not regex)"
                        },
                        "file_glob": {
                            "type": "string",
                            "description": "Optional file glob pattern to filter files (e.g. '*.rs', '*.py'). If omitted, searches all text files."
                        }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a bash command and return its output. Use this for running scripts, git commands, or any shell operation. The command runs in the working directory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute (e.g. 'ls -la', '/workspace/gbrain-cli.sh list', 'gh pr list')"
                        }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file with the given content. Use this to scaffold new files, replace entire file contents, or rewrite after a major refactor. For surgical edits to existing files prefer the `edit` tool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file from the working directory (e.g. 'src/lib.rs')"
                        },
                        "content": {
                            "type": "string",
                            "description": "The complete new content of the file"
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Apply a surgical patch to an existing file by replacing one exact occurrence of `old_text` with `new_text`. Fails if `old_text` appears zero or more than one time. Use this for small, targeted changes; for whole-file rewrites use `write_file`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file from the working directory"
                        },
                        "old_text": {
                            "type": "string",
                            "description": "The exact substring to replace (must appear exactly once in the file)"
                        },
                        "new_text": {
                            "type": "string",
                            "description": "The replacement substring"
                        }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch the contents of a URL over HTTP/HTTPS and return the response body as text. HTML responses are reduced to readable text (scripts/styles stripped, whitespace collapsed). 5 MB body limit, 30 s timeout. Use this to look up API docs, recent news, package versions, or anything that needs the live web. Configure allowed hosts via `LLM_WEB_ALLOWLIST` (comma-separated, default empty = block all) to limit exfiltration.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Absolute http:// or https:// URL to fetch"
                        }
                    },
                    "required": ["url"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Run `git status --short --branch` in the working directory. Returns a compact view of modified, added, deleted, and untracked files plus the current branch.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Run `git diff` (optionally limited to a specific path) and return the unified diff. Use this to review uncommitted changes before committing, or to see what changed in a file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Optional relative path to limit the diff to a single file"
                        },
                        "staged": {
                            "type": "boolean",
                            "description": "If true, show staged changes (`--cached`) instead of unstaged"
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_log",
                "description": "Run `git log` with a compact one-line-per-commit format. Returns the last 20 commits by default.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "max_count": {
                            "type": "integer",
                            "description": "Maximum number of commits to return (default 20, max 200)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional relative path to limit log to commits touching a specific file"
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_commit",
                "description": "Stage the listed paths and create a git commit with the supplied message. Failures from `git commit` (e.g. nothing staged, pre-commit hook failure) are returned verbatim so the model can react.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "Commit message"
                        },
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Relative paths to stage before committing. If omitted, stages all modified tracked files (`git add -u`)."
                        }
                    },
                    "required": ["message"]
                }
            }
        }),
    ]
}

/// Resolve and validate a path within the sandbox.
/// Returns None if the path escapes the working directory.
fn resolve_sandboxed_path(working_dir: &Path, relative_path: &str) -> Option<PathBuf> {
    // Normalize: strip leading slashes to force relative
    let cleaned = relative_path.trim_start_matches('/');
    let full = working_dir.join(cleaned);

    // Canonicalize to resolve .. and symlinks
    let canonical = match full.canonicalize() {
        Ok(p) => p,
        Err(_) => return None,
    };

    let canonical_wd = match working_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return None,
    };

    // Must be within working directory
    if canonical.starts_with(&canonical_wd) {
        Some(canonical)
    } else {
        warn!(
            path = %relative_path,
            "Path escapes sandbox, rejected"
        );
        None
    }
}

/// Execute a tool call and return a structured [`ToolResult`].
///
/// Subprocess tools (bash, git_*, web_fetch) are async and time-boxed;
/// file tools stay synchronous because they are bounded and fast.
/// `timeout_secs` applies per tool invocation and comes from the
/// `LLM_TOOL_TIMEOUT` config.
/// `deny_globs` is the policy's deny-path set (gitignore-like
/// workspace-relative globs). Engine passes `policy.deny_path_globs`;
/// the file tools apply it two ways:
/// - path-argument tools (`read_file`, `write_file`, `edit`) refuse a
///   denied path — same check the engine makes, repeated here so a
///   direct `execute_tool` caller can't bypass it;
/// - enumeration tools (`list_dir`, `search_code`) filter every visited
///   entry — otherwise `.env` contents would reach the model through a
///   search even though `read_file .env` is denied (G1a round-2).
///
/// An empty slice means "no path denies" — shell tools ignore it
/// entirely: a command string can't be path-filtered honestly.
pub async fn execute_tool(
    working_dir: &Path,
    name: &str,
    arguments: &Value,
    timeout_secs: u64,
    deny_globs: &[String],
) -> ToolResult {
    match name {
        "read_file" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(e) = path_denied(path, deny_globs) {
                return e;
            }
            execute_read_file(working_dir, path)
        }
        "list_dir" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            execute_list_dir(working_dir, path, deny_globs)
        }
        "search_code" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let file_glob = arguments.get("file_glob").and_then(|v| v.as_str());
            execute_search_code(working_dir, pattern, file_glob, deny_globs)
        }
        "bash" => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            execute_bash(working_dir, command, timeout_secs).await
        }
        "write_file" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(e) = path_denied(path, deny_globs) {
                return e;
            }
            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            execute_write_file(working_dir, path, content)
        }
        "edit" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(e) = path_denied(path, deny_globs) {
                return e;
            }
            let old_text = arguments
                .get("old_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_text = arguments
                .get("new_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            execute_edit(working_dir, path, old_text, new_text)
        }
        "web_fetch" => {
            let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
            execute_web_fetch(url, timeout_secs).await
        }
        "git_status" => {
            execute_git(
                working_dir,
                &["status", "--short", "--branch"],
                timeout_secs,
            )
            .await
        }
        "git_diff" => {
            let path = arguments.get("path").and_then(|v| v.as_str());
            let staged = arguments
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut args: Vec<&str> = vec!["diff"];
            if staged {
                args.push("--cached");
            }
            let path_owned;
            if let Some(p) = path {
                args.push("--");
                args.push(p);
                path_owned = p.to_string();
                let _ = path_owned; // suppress unused warning if branch not taken
            }
            execute_git(working_dir, &args, timeout_secs).await
        }
        "git_log" => {
            let max_count = arguments
                .get("max_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .clamp(1, 200);
            let path = arguments.get("path").and_then(|v| v.as_str());
            let count_str = format!("-{max_count}");
            let mut args: Vec<String> = vec![
                "log".into(),
                "--oneline".into(),
                "--decorate".into(),
                count_str,
            ];
            if let Some(p) = path {
                args.push("--".into());
                args.push(p.into());
            }
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            execute_git(working_dir, &arg_refs, timeout_secs).await
        }
        "git_commit" => {
            let message = arguments
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let paths: Vec<String> = arguments
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let mut output = String::new();
            let add_args: Vec<&str> = if paths.is_empty() {
                vec!["add", "-u"]
            } else {
                let mut v = vec!["add", "--"];
                let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                v.extend(path_refs);
                v
            };
            // `git add` first; surface its output before the commit so the
            // model sees if staging failed.
            let staged = execute_git(working_dir, &add_args, timeout_secs).await;
            if !staged.text.is_empty() && !staged.text.starts_with("(no output)") {
                output.push_str(&staged.text);
            }

            if message.is_empty() {
                output.push_str("Error: commit message is empty\n");
                return ToolResult::err(ToolErrorKind::InvalidArgs, output);
            }

            let commit = execute_git(working_dir, &["commit", "-m", message], timeout_secs).await;
            output.push_str(&commit.text);
            let mut result = ToolResult::ok(output);
            result.error_kind = commit.error_kind;
            result.truncated = staged.truncated || commit.truncated;
            result
        }
        _ => ToolResult::err(ToolErrorKind::UnknownTool, format!("Unknown tool: {name}")),
    }
}

fn execute_read_file(working_dir: &Path, relative_path: &str) -> ToolResult {
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return ToolResult::err(
            ToolErrorKind::OutOfSandbox,
            format!(
                "Error: path '{}' is outside the working directory or does not exist",
                relative_path
            ),
        );
    };

    if !path.is_file() {
        return ToolResult::err(
            ToolErrorKind::NotFound,
            format!("Error: '{}' is not a file", relative_path),
        );
    }

    // Check file size
    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > MAX_FILE_SIZE => {
            return ToolResult::err(
                ToolErrorKind::TooLarge,
                format!(
                    "Error: file is too large ({} bytes, max {} bytes)",
                    meta.len(),
                    MAX_FILE_SIZE
                ),
            );
        }
        Err(e) => {
            return ToolResult::err(
                ToolErrorKind::ExecFailed,
                format!("Error reading file metadata: {e}"),
            )
        }
        _ => {}
    }

    match std::fs::read_to_string(&path) {
        Ok(mut content) => {
            debug!(path = %relative_path, bytes = content.len(), "read_file");
            let truncated = cap_output(&mut content);
            let mut r = ToolResult::ok(content);
            r.truncated = truncated;
            r
        }
        Err(e) => ToolResult::err(
            ToolErrorKind::ExecFailed,
            format!("Error reading file: {e}"),
        ),
    }
}

/// Deny check for path-argument tools: `path` is matched against the
/// policy globs the same way `policy::decide` does it — repeated here
/// so the rule holds even when a caller bypasses the engine.
fn path_denied(relative_path: &str, deny_globs: &[String]) -> Option<ToolResult> {
    let rel = relative_path.trim_start_matches('/');
    for pattern in deny_globs {
        if crate::policy::glob_matches(pattern, rel) {
            return Some(ToolResult::err(
                ToolErrorKind::Denied,
                format!("Error: path '{relative_path}' is denied by policy ('{pattern}')"),
            ));
        }
    }
    None
}

/// True when the working-dir-relative path matches a deny glob.
fn is_denied(rel_path: &str, deny_globs: &[String]) -> bool {
    deny_globs
        .iter()
        .any(|p| crate::policy::glob_matches(p, rel_path))
}

fn execute_list_dir(working_dir: &Path, relative_path: &str, deny_globs: &[String]) -> ToolResult {
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return ToolResult::err(
            ToolErrorKind::OutOfSandbox,
            format!(
                "Error: path '{}' is outside the working directory or does not exist",
                relative_path
            ),
        );
    };

    if !path.is_dir() {
        return ToolResult::err(
            ToolErrorKind::NotFound,
            format!("Error: '{}' is not a directory", relative_path),
        );
    }

    let mut output = String::new();
    let mut count = 0;
    let mut denied = 0usize;
    list_dir_recursive(
        working_dir,
        &path,
        "",
        0,
        &mut output,
        &mut count,
        deny_globs,
        &mut denied,
    );
    if denied > 0 {
        output.push_str(&format!("\n[{denied} path(s) hidden by policy]\n"));
    }

    if count >= MAX_LIST_ENTRIES {
        output.push_str(&format!(
            "\n... truncated ({MAX_LIST_ENTRIES} entries shown)\n"
        ));
    }

    debug!(path = %relative_path, entries = count, "list_dir");
    let mut r = ToolResult::ok(output);
    r.truncated = count >= MAX_LIST_ENTRIES;
    r
}

#[allow(clippy::too_many_arguments)]
fn list_dir_recursive(
    working_dir: &Path,
    dir: &Path,
    prefix: &str,
    depth: usize,
    output: &mut String,
    count: &mut usize,
    deny_globs: &[String],
    denied: &mut usize,
) {
    if depth > MAX_LIST_DEPTH || *count >= MAX_LIST_ENTRIES {
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if *count >= MAX_LIST_ENTRIES {
            return;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        // Policy deny globs apply to every enumerated path BEFORE the
        // hidden-file filter — a denied `.env` would otherwise be
        // silently skipped by is_ignored_entry and never counted as a
        // policy skip.
        let rel = entry
            .path()
            .strip_prefix(working_dir)
            .unwrap_or(&entry.path())
            .to_string_lossy()
            .to_string();
        if is_denied(&rel, deny_globs) {
            *denied += 1;
            continue;
        }

        // Skip hidden files and common noise
        if is_ignored_entry(&name) {
            continue;
        }

        // Determine whether the entry is a directory. `file_type()` can fail
        // for e.g. broken symlinks; fall back to `metadata()` (follows links)
        // and skip the entry entirely if that also fails rather than panicking.
        let is_dir = match entry.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(_) => match std::fs::metadata(entry.path()) {
                Ok(meta) => meta.is_dir(),
                Err(e) => {
                    warn!(entry = %name, error = %e, "Skipping entry: cannot stat");
                    continue;
                }
            },
        };
        if is_dir {
            output.push_str(&format!("{prefix}{name}/\n"));
            *count += 1;
            list_dir_recursive(
                working_dir,
                &entry.path(),
                &format!("{prefix}  "),
                depth + 1,
                output,
                count,
                deny_globs,
                denied,
            );
        } else {
            output.push_str(&format!("{prefix}{name}\n"));
            *count += 1;
        }
    }
}

fn execute_search_code(
    working_dir: &Path,
    pattern: &str,
    file_glob: Option<&str>,
    deny_globs: &[String],
) -> ToolResult {
    if pattern.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: search pattern is empty".to_string(),
        );
    }

    let mut results = String::new();
    let mut match_count = 0;
    let mut denied = 0usize;
    const MAX_MATCHES: usize = 50;

    search_dir(
        working_dir,
        working_dir,
        pattern,
        file_glob,
        &mut results,
        &mut match_count,
        MAX_MATCHES,
        0,
        deny_globs,
        &mut denied,
    );
    if denied > 0 {
        results.push_str(&format!("\n[{denied} path(s) hidden by policy]\n"));
    }

    if match_count == 0 {
        return ToolResult::ok(format!("No matches found for '{pattern}'"));
    }

    if match_count >= MAX_MATCHES {
        results.push_str(&format!("\n... truncated ({MAX_MATCHES} matches shown)\n"));
    }

    debug!(pattern, matches = match_count, "search_code");
    let mut r = ToolResult::ok(results);
    r.truncated = match_count >= MAX_MATCHES;
    r
}

#[allow(clippy::too_many_arguments)]
fn search_dir(
    dir: &Path,
    working_dir: &Path,
    pattern: &str,
    file_glob: Option<&str>,
    results: &mut String,
    match_count: &mut usize,
    max_matches: usize,
    depth: usize,
    deny_globs: &[String],
    denied: &mut usize,
) {
    if *match_count >= max_matches || depth > MAX_LIST_DEPTH * 4 {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if *match_count >= max_matches {
            return;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Policy deny globs apply BEFORE the hidden-file filter — a
        // denied `.env` would otherwise be silently skipped by
        // is_ignored_entry and never counted as a policy skip.
        let rel = path
            .strip_prefix(working_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if is_denied(&rel, deny_globs) {
            *denied += 1;
            continue;
        }

        // Skip hidden/noise directories
        if is_ignored_entry(&name) {
            continue;
        }

        // Skip symlinks — both directories and files. `path.is_dir()`
        // follows symlinks which means a hostile symlink under the
        // sandbox can read the workspace file outside it (review §3.1).
        // We use `symlink_metadata` which does NOT follow symlinks; if
        // the entry itself is a symlink we skip it. The canonical
        // prefix check below then catches any remaining path-traversal
        // via regular paths or hardlinks.
        if entry
            .path()
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            warn!(path = %path.display(), "Skipping symlink in search_code");
            continue;
        }

        // Policy deny globs apply to every enumerated path — searching
        // must never return matches inside `.env`, `*.key`, etc. just
        // because the caller didn't name the file directly.
        let rel = path
            .strip_prefix(working_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if is_denied(&rel, deny_globs) {
            *denied += 1;
            continue;
        }

        // Sanity-check the path is actually inside the working dir. This
        // is defense in depth — `resolve_sandboxed_path` already does
        // this for read / write_file, but search_dir recurses through
        // directory entries and could theoretically walk into a path
        // created by a TOCTOU between the canonicalize above and the
        // read below.
        let canonical_wd = match working_dir.canonicalize() {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Ok(canonical_path) = path.canonicalize() {
            if !canonical_path.starts_with(&canonical_wd) {
                warn!(path = %path.display(), "search_code walked outside sandbox, skipping");
                continue;
            }
        }

        if path.is_dir() {
            search_dir(
                &path,
                working_dir,
                pattern,
                file_glob,
                results,
                match_count,
                max_matches,
                depth + 1,
                deny_globs,
                denied,
            );
        } else if path.is_file() {
            // Check glob filter
            if let Some(glob) = file_glob {
                let ext_pattern = glob.trim_start_matches('*');
                if !name.ends_with(ext_pattern) {
                    continue;
                }
            }

            // Skip binary/large files
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() > MAX_FILE_SIZE {
                    continue;
                }
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                let relative = path.strip_prefix(working_dir).unwrap_or(&path);
                for (line_num, line) in content.lines().enumerate() {
                    if *match_count >= max_matches {
                        return;
                    }
                    if line.contains(pattern) {
                        results.push_str(&format!(
                            "{}:{}:{}\n",
                            relative.display(),
                            line_num + 1,
                            line.trim()
                        ));
                        *match_count += 1;
                    }
                }
            }
        }
    }
}

/// Kills the child process's process group on drop (Unix). `bash -c`
/// commands commonly spawn grandchildren (pipes, command substitution);
/// SIGKILLing only the direct child would orphan them. The guard fires on
/// timeout, on success, and when the surrounding future is dropped by a
/// session cancel — which is what makes `session/cancel` actually stop
/// work instead of waiting for the sync call to return.
fn kill_process_group(pgid: Option<u32>) {
    if let Some(pgid) = pgid {
        // `kill -KILL -<pgid>` targets the whole group; the child is
        // its own group leader via process_group(0), so pgid == pid.
        // Synchronous spawn is fine — `kill` execs and exits
        // immediately; we intentionally don't wait for it.
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", &format!("-{pgid}")])
            .spawn();
    }
}

struct ProcessGroupGuard {
    pgid: Option<u32>,
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.pgid.take());
    }
}

/// Bytes of process output kept in memory per stream. `wait_with_output`
/// collects the child's full stdout+stderr unboundedly — a `yes` or a
/// verbose `find /` would fill RAM before the timeout could fire. We
/// keep the first N bytes and keep draining (discarding) so the child
/// never blocks on a full pipe; the retained prefix carries a
/// truncation note.
const MAX_PROC_OUTPUT_BYTES: usize = 1_000_000;

/// Read a stream to end, retaining only the first
/// `MAX_PROC_OUTPUT_BYTES`. Draining matters: stopping early leaves
/// the child blocked on a full pipe until the timeout kills it.
/// Returns (retained_bytes, total_bytes_seen).
async fn drain_bounded(mut stream: impl tokio::io::AsyncRead + Unpin) -> (Vec<u8>, usize) {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n;
                let remaining = MAX_PROC_OUTPUT_BYTES.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
        }
    }
    (kept, total)
}

/// Spawn a subprocess, capture stdout/stderr with a timeout, and kill the
/// process group when the future completes, times out, or is dropped
/// (session cancel). Nothing spawned by a tool call survives the call —
/// a `sleep &` background job would otherwise hold the output pipes open
/// and hang the reader until timeout. Output per stream is memory-bounded
/// via [`drain_bounded`]. Returns (exit_code, combined_output, timed_out).
async fn run_process(
    program: &str,
    args: &[&str],
    working_dir: &Path,
    timeout_secs: u64,
) -> Result<(Option<i32>, String, bool), std::io::Error> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn()?;
    let _guard = ProcessGroupGuard {
        #[cfg(unix)]
        pgid: child.id(),
        #[cfg(not(unix))]
        pgid: None,
    };

    // Drain both pipes concurrently so a stderr-heavy command can't
    // deadlock on a full stdout buffer (and vice versa).
    let stdout_reader = child.stdout.take().map(|s| tokio::spawn(drain_bounded(s)));
    let stderr_reader = child.stderr.take().map(|s| tokio::spawn(drain_bounded(s)));

    // Clamp to a 1s floor: timeout(0s) would expire before the child
    // could run at all, which reads as a spawn failure rather than a
    // configured zero-second budget.
    let wait = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs.max(1)),
        child.wait(),
    )
    .await;

    let (status, timed_out) = match wait {
        Ok(r) => (Some(r), false),
        Err(_) => (None, true),
    };

    // Kill the whole group NOW — before joining the readers — on every
    // path: a timeout kill obviously needs it, but so does the success
    // path: a `cmd &`-style grandchild that outlives the child still
    // holds the pipes open, and joining the readers first would hang
    // forever on those pipes (the guard's drop-kill only runs after
    // this function returns — exactly the deadlock the wait was for).
    kill_process_group(_guard.pgid);
    if timed_out {
        // Bounded wait: the group kill above is asynchronous, give the
        // child a short grace to reap rather than awaiting forever.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    }

    // Join the readers with a grace deadline — pipes close when the
    // group dies, but we never wait on them unboundedly.
    let collect = |h: Option<tokio::task::JoinHandle<(Vec<u8>, usize)>>| async move {
        match h {
            Some(h) => match tokio::time::timeout(std::time::Duration::from_secs(3), h).await {
                Ok(Ok((kept, total))) => (kept, total),
                _ => (Vec::new(), 0),
            },
            None => (Vec::new(), 0),
        }
    };
    let (out_bytes, out_total) = collect(stdout_reader).await;
    let (err_bytes, err_total) = collect(stderr_reader).await;

    // Truncation note goes FIRST: the downstream 50KB `cap_output`
    // cuts the tail, so a note appended at the end would be invisible.
    let stream_note = |total: usize, kept: usize| -> String {
        if total > kept {
            format!("[note: stream truncated — kept first {kept}/{total} bytes]\n")
        } else {
            String::new()
        }
    };
    let stdout = String::from_utf8_lossy(&out_bytes);
    let stderr = String::from_utf8_lossy(&err_bytes);
    let mut result = String::new();
    result.push_str(&stream_note(out_total, out_bytes.len()));
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() || err_total > err_bytes.len() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr] ");
        result.push_str(&stream_note(err_total, err_bytes.len()));
        result.push_str(&stderr);
    }

    let exit_code = match status {
        Some(Ok(s)) => s.code(),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    Ok((exit_code, result, timed_out))
}

/// Execute a bash command in the working directory with a real timeout
/// and cancellation support (P1/E11 first slice).
async fn execute_bash(working_dir: &Path, command: &str, timeout_secs: u64) -> ToolResult {
    if command.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: command is empty".to_string(),
        );
    }

    debug!(command, "bash");

    match run_process("bash", &["-c", command], working_dir, timeout_secs).await {
        Ok((exit_code, mut output, timed_out)) => {
            if timed_out {
                return ToolResult::err(
                    ToolErrorKind::Timeout,
                    format!("Error: command timed out after {timeout_secs}s and was terminated"),
                );
            }
            if output.is_empty() {
                output = "(no output)".to_string();
            }
            let truncated = cap_output(&mut output);
            let mut r = ToolResult {
                text: output,
                exit_code,
                error_kind: if exit_code.is_some_and(|c| c != 0) {
                    Some(ToolErrorKind::ExecFailed)
                } else {
                    None
                },
                truncated,
            };
            // Non-zero exit is still a valid tool result (the model reads
            // the failure text); keep it but surface the classification.
            if r.error_kind.is_some() && !r.text.starts_with("Error") {
                r.text = format!("(exit {})\n{}", exit_code.unwrap_or_default(), r.text);
            }
            r
        }
        Err(e) => ToolResult::err(
            ToolErrorKind::ExecFailed,
            format!("Error executing command: {e}"),
        ),
    }
}

/// Maximum size of a file that `write_file` / `edit` will accept.
const MAX_WRITE_FILE_SIZE: u64 = 5 * 1024 * 1024;

fn execute_write_file(working_dir: &Path, relative_path: &str, content: &str) -> ToolResult {
    if relative_path.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: path is empty".to_string(),
        );
    }
    if content.len() as u64 > MAX_WRITE_FILE_SIZE {
        return ToolResult::err(
            ToolErrorKind::TooLarge,
            format!(
                "Error: content too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_FILE_SIZE
            ),
        );
    }

    let cleaned = relative_path.trim_start_matches('/');
    let target = working_dir.join(cleaned);

    // Reject obvious `..` escapes: if any path component is `..`, refuse.
    for component in Path::new(cleaned).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return ToolResult::err(
                ToolErrorKind::OutOfSandbox,
                format!(
                    "Error: path '{}' escapes the working directory",
                    relative_path
                ),
            );
        }
    }

    let canonical_wd = match working_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return ToolResult::err(
                ToolErrorKind::ExecFailed,
                format!("Error resolving working directory: {e}"),
            )
        }
    };

    // Resolved write target — set to the canonical parent + basename so
    // the open()/rename() path contains no symlink components at all.
    let target_real;
    if let Some(parent) = target.parent() {
        // Validate the deepest EXISTING ancestor before creating
        // anything. A blind `create_dir_all` first would happily make
        // real directories outside the sandbox through a symlinked
        // component — the side effect must not happen even when the
        // write is ultimately rejected (G1a-FS1).
        let mut ancestor = parent;
        let existing = loop {
            match ancestor.symlink_metadata() {
                Ok(_) => break Some(ancestor),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => match ancestor.parent() {
                    Some(up) => ancestor = up,
                    None => break None,
                },
                Err(e) => {
                    return ToolResult::err(
                        ToolErrorKind::ExecFailed,
                        format!("Error inspecting '{}': {e}", ancestor.display()),
                    );
                }
            }
        };
        match existing.and_then(|a| a.canonicalize().ok()) {
            Some(canon_anc) if canon_anc.starts_with(&canonical_wd) => {}
            _ => {
                return ToolResult::err(
                    ToolErrorKind::OutOfSandbox,
                    format!(
                        "Error: path '{}' escapes the working directory via a symlinked ancestor",
                        relative_path
                    ),
                );
            }
        }

        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolResult::err(
                ToolErrorKind::ExecFailed,
                format!("Error creating parent directory: {e}"),
            );
        }
        // Re-verify after creation (TOCTOU-lite): a directory swapped
        // in for a symlink between the checks above and now would
        // still land the write outside the sandbox.
        match parent.canonicalize() {
            Ok(canon_parent) if canon_parent.starts_with(&canonical_wd) => {
                // Write through the CANONICAL path — every component is
                // a verified real directory, so no symlink component can
                // be swapped in on the open() path.
                target_real = canon_parent.join(target.file_name().unwrap());
            }
            _ => {
                return ToolResult::err(
                    ToolErrorKind::OutOfSandbox,
                    format!(
                        "Error: path '{}' escapes the working directory via a symlinked parent",
                        relative_path
                    ),
                );
            }
        }
    } else {
        target_real = canonical_wd.join(target.file_name().unwrap());
    }

    // An existing target that is itself a symlink: the tmp-write+rename
    // below REPLACES the dentry instead of following it, so a swapped-in
    // link at write time can no longer redirect bytes outside (G1a-FS2).
    // Still refuse outright for a clear error — and refuse when it
    // resolves inside but the model clearly meant to write through it.
    if let Ok(meta) = target.symlink_metadata() {
        if meta.file_type().is_symlink() {
            match target.canonicalize() {
                Ok(canon_target) if canon_target.starts_with(&canonical_wd) => {}
                _ => {
                    return ToolResult::err(
                        ToolErrorKind::OutOfSandbox,
                        format!(
                            "Error: path '{}' is a symlink outside the working directory",
                            relative_path
                        ),
                    );
                }
            }
        }
    }

    // Atomic publish: write the payload to a same-dir temp name, then
    // rename() over the target. rename() replaces the dentry — it can
    // never follow a swapped-in symlink — and readers see either the
    // old file or the new one, never a partial write.
    let tmp_name = format!(
        ".{}.acp-tmp-{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id()
    );
    let tmp_target = target_real
        .parent()
        .map(|p| p.join(&tmp_name))
        .unwrap_or_else(|| target_real.with_file_name(&tmp_name));
    match std::fs::write(&tmp_target, content) {
        Ok(()) => {
            // Post-write re-verify: the parent must STILL be in-sandbox
            // (a component swap between our checks and now is refused
            // and its temp file cleaned up).
            match target_real.parent().and_then(|p| p.canonicalize().ok()) {
                Some(p) if p.starts_with(&canonical_wd) => {}
                _ => {
                    let _ = std::fs::remove_file(&tmp_target);
                    return ToolResult::err(
                        ToolErrorKind::OutOfSandbox,
                        format!(
                            "Error: path '{}' escaped the working directory during write",
                            relative_path
                        ),
                    );
                }
            }
            if let Err(e) = std::fs::rename(&tmp_target, &target_real) {
                let _ = std::fs::remove_file(&tmp_target);
                return ToolResult::err(
                    ToolErrorKind::ExecFailed,
                    format!("Error publishing file: {e}"),
                );
            }
            debug!(
                path = %relative_path,
                bytes = content.len(),
                "write_file"
            );
            ToolResult::ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                relative_path
            ))
        }
        Err(e) => ToolResult::err(
            ToolErrorKind::ExecFailed,
            format!("Error writing file: {e}"),
        ),
    }
}

/// Replace exactly one occurrence of `old_text` with `new_text` in the
/// file at `relative_path`. Fails clearly if `old_text` is missing or
/// appears more than once — the model should re-read the file and try
/// again rather than guess.
fn execute_edit(
    working_dir: &Path,
    relative_path: &str,
    old_text: &str,
    new_text: &str,
) -> ToolResult {
    if relative_path.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: path is empty".to_string(),
        );
    }
    if old_text.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: old_text is empty; refusing to do an open-ended replace".to_string(),
        );
    }

    // Use the same sandbox resolver as read_file so we cannot escape.
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return ToolResult::err(
            ToolErrorKind::OutOfSandbox,
            format!(
                "Error: path '{}' is outside the working directory or does not exist",
                relative_path
            ),
        );
    };
    if !path.is_file() {
        return ToolResult::err(
            ToolErrorKind::NotFound,
            format!("Error: '{}' is not a file", relative_path),
        );
    }

    let original = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return ToolResult::err(
                ToolErrorKind::ExecFailed,
                format!("Error reading file: {e}"),
            )
        }
    };

    let occurrences = original.matches(old_text).count();
    if occurrences == 0 {
        return ToolResult::err(
            ToolErrorKind::NotFound,
            format!(
                "Error: old_text not found in {}. Re-read the file and provide the exact current text.",
                relative_path
            ),
        );
    }
    if occurrences > 1 {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            format!(
                "Error: old_text appears {} times in {}. Provide a more specific (longer) old_text that matches exactly once.",
                occurrences, relative_path
            ),
        );
    }

    let patched = original.replacen(old_text, new_text, 1);
    if let Err(e) = std::fs::write(&path, &patched) {
        return ToolResult::err(
            ToolErrorKind::ExecFailed,
            format!("Error writing file: {e}"),
        );
    }
    debug!(
        path = %relative_path,
        before = original.len(),
        after = patched.len(),
        "edit"
    );
    ToolResult::ok(format!(
        "patched {} ({} bytes -> {} bytes)",
        relative_path,
        original.len(),
        patched.len()
    ))
}

// ----------------------------------------------------------------------------
// Web fetch
// ----------------------------------------------------------------------------

/// 5 MB body cap for web_fetch responses.
const MAX_WEB_BODY_BYTES: usize = 5 * 1024 * 1024;
/// 30 second total timeout for web_fetch requests.
const WEB_FETCH_TIMEOUT_SECS: u64 = 30;

/// Comma-separated allowlist of host suffixes read from `LLM_WEB_ALLOWLIST`.
/// Empty (default) blocks every host; set this to give the model web
/// access. Use this to prevent the model from making requests to
/// internal infrastructure even if the sandbox is misconfigured.
fn web_fetch_allowed_hosts() -> Vec<String> {
    std::env::var("LLM_WEB_ALLOWLIST")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch `url` and return the body as text. HTML responses get a light
/// reduction (script/style stripped, tags removed, whitespace
/// collapsed) so the model sees readable content; everything else is
/// passed through (text/plain, application/json, …).
///
/// The body is read in chunks with a hard cap enforced *during* the
/// download — a slow or oversized response stops feeding memory at
/// [`MAX_WEB_BODY_BYTES`], keeps the bounded prefix, and flags
/// `truncated` so the model knows it saw a partial document.
async fn execute_web_fetch(url: &str, timeout_secs: u64) -> ToolResult {
    if url.is_empty() {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            "Error: url is empty".to_string(),
        );
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            format!("Error: '{url}' is not a valid URL"),
        );
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return ToolResult::err(
            ToolErrorKind::InvalidArgs,
            format!(
                "Error: scheme '{}' is not allowed (only http and https)",
                parsed.scheme()
            ),
        );
    }

    let allow = web_fetch_allowed_hosts();
    if allow.is_empty() {
        return ToolResult::err(
            ToolErrorKind::Denied,
            "Error: web_fetch is disabled (LLM_WEB_ALLOWLIST is empty)".to_string(),
        );
    }
    let host = parsed
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    if !allow
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    {
        return ToolResult::err(
            ToolErrorKind::Denied,
            format!(
                "Error: host '{host}' is not in LLM_WEB_ALLOWLIST ({})",
                allow.join(", ")
            ),
        );
    }

    let allow_for_redirect_check = allow.clone();
    let timeout = timeout_secs.max(1).min(WEB_FETCH_TIMEOUT_SECS.max(1));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .user_agent(concat!("acp-bridge/", env!("CARGO_PKG_VERSION")))
        // Limit redirects to a small number and re-validate the host
        // on every hop. Without this, a server on an allowlisted
        // domain can 302 to an internal host and the body fetch would
        // succeed — review §3.4.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.stop();
            }
            // Re-check the next hop's host against the allowlist. The
            // cloned allowlist is shared (Arc-like via the closure
            // environment) so this captures the original set at
            // request time.
            let next = attempt.url();
            if let Some(next_host) = next.host_str() {
                let next_host = next_host.to_ascii_lowercase();
                let allowed = allow_for_redirect_check.iter().any(|suffix| {
                    next_host == *suffix || next_host.ends_with(&format!(".{suffix}"))
                });
                if !allowed {
                    return attempt.stop();
                }
            } else {
                return attempt.stop();
            }
            attempt.follow()
        }))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ToolResult::err(
                ToolErrorKind::Network,
                format!("Error building HTTP client: {e}"),
            )
        }
    };

    let mut resp = match client.get(parsed).send().await {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_timeout() {
                ToolErrorKind::Timeout
            } else {
                ToolErrorKind::Network
            };
            return ToolResult::err(kind, format!("Error fetching URL: {e}"));
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Read the body chunk by chunk with the cap enforced while
    // downloading — not after the fact. A server streaming an endless
    // body cannot grow memory beyond MAX_WEB_BODY_BYTES.
    let mut bytes: Vec<u8> = Vec::new();
    let mut body_truncated = false;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_WEB_BODY_BYTES.saturating_sub(bytes.len());
                if room == 0 {
                    body_truncated = true;
                    break;
                }
                if chunk.len() > room {
                    bytes.extend_from_slice(&chunk[..room]);
                    body_truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return ToolResult::err(
                    ToolErrorKind::Network,
                    format!("Error reading response body: {e}"),
                )
            }
        }
    }

    if !status.is_success() {
        return ToolResult::err(
            ToolErrorKind::Network,
            format!(
                "Error: HTTP {} {} (response body {} bytes)",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                bytes.len()
            ),
        );
    }

    debug!(url, status = %status, bytes = bytes.len(), "web_fetch");

    let body = if content_type.contains("html") {
        reduce_html(&String::from_utf8_lossy(&bytes))
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };

    let mut out = format!(
        "HTTP {} {}\nContent-Type: {}\n\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        content_type
    );
    if body_truncated {
        out.push_str("[note: body truncated at 5 MB during download]\n\n");
    }
    out.push_str(&body);
    let truncated = cap_output(&mut out);
    let mut r = ToolResult::ok(out);
    r.truncated = truncated || body_truncated;
    r
}

/// Very small HTML-to-text reducer. Strips `<script>` / `<style>`
/// entirely, removes all other tags, collapses whitespace. We deliberately
/// keep this minimal — no entity decoding beyond what `&amp; &lt; &gt;
/// &quot; &apos; &nbsp;` covers, no DOM — the goal is "readable for the
/// LLM", not "faithful HTML rendering".
fn reduce_html(input: &str) -> String {
    // Drop script and style blocks first.
    let mut s = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('<') {
        s.push_str(&rest[..open]);
        let lower = rest[open..].to_ascii_lowercase();
        if lower.starts_with("<script") || lower.starts_with("<style") {
            // Find matching close tag.
            let tag = if lower.starts_with("<script") {
                "script"
            } else {
                "style"
            };
            let close = format!("</{tag}>");
            if let Some(end) = rest[open..].find(&close) {
                rest = &rest[open + end + close.len()..];
                continue;
            } else {
                // No close tag — drop everything to the end.
                rest = "";
                break;
            }
        }
        // Skip this tag.
        if let Some(close) = rest[open..].find('>') {
            rest = &rest[open + close + 1..];
        } else {
            rest = "";
            break;
        }
    }
    s.push_str(rest);

    // Decode a small set of named entities the model sees frequently.
    s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");

    // Strip all remaining tags (we already removed script/style).
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    // Collapse whitespace.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

// ----------------------------------------------------------------------------
// Git helpers
// ----------------------------------------------------------------------------

/// Run `git` in the working directory with the supplied args under the
/// same timeout/process-group discipline as `bash` — a `git` that hangs
/// (e.g. blocked on a lock or credential helper) cannot stall the turn.
async fn execute_git(working_dir: &Path, args: &[&str], timeout_secs: u64) -> ToolResult {
    debug!(args = ?args, "git");

    match run_process("git", args, working_dir, timeout_secs).await {
        Ok((exit_code, mut result, timed_out)) => {
            if timed_out {
                return ToolResult::err(
                    ToolErrorKind::Timeout,
                    format!(
                        "Error: git {} timed out after {timeout_secs}s and was terminated",
                        args.first().copied().unwrap_or("")
                    ),
                );
            }
            let code = exit_code.unwrap_or(0);
            if code != 0 {
                let text = format!(
                    "git {} failed (exit {code:?}):\n{}",
                    args.first().copied().unwrap_or(""),
                    result
                );
                let mut r = ToolResult::err(ToolErrorKind::ExecFailed, text);
                r.exit_code = Some(code);
                return r;
            }
            if result.is_empty() {
                result = "(no output)".to_string();
            }
            let truncated = cap_output(&mut result);
            ToolResult {
                text: result,
                exit_code: Some(code),
                error_kind: None,
                truncated,
            }
        }
        Err(e) => {
            let text = if e.kind() == std::io::ErrorKind::NotFound {
                "Error: git binary not found on PATH".to_string()
            } else {
                format!("Error running git: {e}")
            };
            ToolResult::err(ToolErrorKind::ExecFailed, text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpwd() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-bridge-tools-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_file_creates_and_overwrites() {
        let wd = tmpwd();
        let result = execute_write_file(&wd, "hello.txt", "hi\n");
        assert!(result.text.contains("wrote"), "got: {}", result.text);
        assert_eq!(fs::read_to_string(wd.join("hello.txt")).unwrap(), "hi\n");

        let result = execute_write_file(&wd, "hello.txt", "there\n");
        assert!(result.text.contains("wrote"));
        assert_eq!(fs::read_to_string(wd.join("hello.txt")).unwrap(), "there\n");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn write_file_rejects_parent_dir_traversal() {
        let wd = tmpwd();
        let result = execute_write_file(&wd, "../escape.txt", "x");
        assert!(result.text.starts_with("Error"), "got: {}", result.text);
        assert_eq!(result.error_kind, Some(ToolErrorKind::OutOfSandbox));
        assert!(!wd.parent().unwrap().join("escape.txt").exists());

        let _ = fs::remove_dir_all(&wd);
    }

    /// A symlinked parent directory must not smuggle writes outside the
    /// sandbox — the `..` component check cannot see through symlinks
    /// (G1a-FS1).
    #[test]
    fn write_file_rejects_symlinked_parent_escape() {
        let wd = tmpwd();
        let outside = tmpwd();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, wd.join("linkdir")).unwrap();
            let result = execute_write_file(&wd, "linkdir/evil.txt", "x");
            assert_eq!(result.error_kind, Some(ToolErrorKind::OutOfSandbox));
            assert!(
                !outside.join("evil.txt").exists(),
                "write escaped via symlinked parent"
            );

            // A normal sibling path still works next to the link.
            let ok = execute_write_file(&wd, "fine.txt", "y");
            assert!(ok.error_kind.is_none(), "got: {}", ok.text);
        }
        let _ = fs::remove_dir_all(&wd);
        let _ = fs::remove_dir_all(&outside);
    }

    /// G1a round-2: a write under a symlinked parent that needs NEW
    /// directories must be rejected BEFORE `create_dir_all` runs —
    /// the reject must not itself create directories outside the
    /// sandbox (the side effect was the bug).
    #[test]
    fn write_file_reject_creates_nothing_outside() {
        let wd = tmpwd();
        let outside = tmpwd();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, wd.join("linkdir")).unwrap();
            let result = execute_write_file(&wd, "linkdir/newsub/evil.txt", "x");
            assert_eq!(result.error_kind, Some(ToolErrorKind::OutOfSandbox));
            assert!(
                !outside.join("newsub").exists(),
                "rejected write still created directories outside the sandbox"
            );
        }
        let _ = fs::remove_dir_all(&wd);
        let _ = fs::remove_dir_all(&outside);
    }

    /// G1a round-2: process output is memory-bounded — a command
    /// printing megabytes keeps ~1MB and says so, rather than the
    /// backend buffering the whole stream.
    #[tokio::test]
    async fn bash_output_is_bounded() {
        let wd = tmpwd();
        // ~2MB of 'a' — well past the 1MB stream cap.
        let result = execute_bash(&wd, "head -c 2000000 /dev/zero | tr '\\0' 'a'", 30).await;
        assert!(
            result.text.len() < 1_100_000,
            "output not bounded: {} bytes",
            result.text.len()
        );
        assert!(
            result.text.contains("stream truncated"),
            "missing truncation note: {}",
            &result.text[..result.text.len().min(200)]
        );
        let _ = fs::remove_dir_all(&wd);
    }

    /// G1a round-2: denied files must not leak through search results —
    /// `read_file .env` is denied, so `search_code` finding `.env`
    /// contents anyway would be a trivial bypass.
    #[test]
    fn search_code_hides_denied_files() {
        let wd = tmpwd();
        fs::write(wd.join(".env"), "SECRET=hunter2\n").unwrap();
        fs::write(
            wd.join("allowed.rs"),
            "fn main() { println!(\"hunter2\"); }",
        )
        .unwrap();
        let denies: Vec<String> = vec![".env".into()];
        let result = execute_search_code(&wd, "hunter2", None, &denies);
        assert!(result.text.contains("allowed.rs"), "got: {}", result.text);
        assert!(
            !result.text.contains("SECRET"),
            ".env leaked through search_code: {}",
            result.text
        );
        assert!(
            result.text.contains("hidden by policy"),
            "missing skip note: {}",
            result.text
        );
        let _ = fs::remove_dir_all(&wd);
    }

    /// list_dir hides denied entries the same way.
    #[test]
    fn list_dir_hides_denied_files() {
        let wd = tmpwd();
        fs::write(wd.join(".env"), "SECRET=x").unwrap();
        fs::write(wd.join("visible.txt"), "x").unwrap();
        let denies: Vec<String> = vec![".env".into()];
        let result = execute_list_dir(&wd, ".", &denies);
        assert!(result.text.contains("visible.txt"), "got: {}", result.text);
        assert!(
            !result.text.contains(".env\n") && !result.text.contains("SECRET"),
            ".env listed: {}",
            result.text
        );
        let _ = fs::remove_dir_all(&wd);
    }

    /// An existing file that is itself a symlink to an outside target
    /// must not be followed by write_file.
    #[test]
    fn write_file_rejects_symlink_target_outside() {
        let wd = tmpwd();
        let outside = tmpwd();
        #[cfg(unix)]
        {
            fs::write(outside.join("victim.txt"), "keep").unwrap();
            std::os::unix::fs::symlink(outside.join("victim.txt"), wd.join("link.txt")).unwrap();
            let result = execute_write_file(&wd, "link.txt", "EVIL");
            assert_eq!(result.error_kind, Some(ToolErrorKind::OutOfSandbox));
            assert_eq!(
                fs::read_to_string(outside.join("victim.txt")).unwrap(),
                "keep"
            );
        }
        let _ = fs::remove_dir_all(&wd);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn edit_replaces_unique_match() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "alpha beta gamma").unwrap();
        let result = execute_edit(&wd, "f.txt", "beta", "BETA");
        assert!(result.text.contains("patched"), "got: {}", result.text);
        assert_eq!(
            fs::read_to_string(wd.join("f.txt")).unwrap(),
            "alpha BETA gamma"
        );

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_fails_when_old_text_missing() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "alpha beta").unwrap();
        let result = execute_edit(&wd, "f.txt", "gamma", "GAMMA");
        assert!(result.text.contains("not found"), "got: {}", result.text);
        assert_eq!(fs::read_to_string(wd.join("f.txt")).unwrap(), "alpha beta");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_fails_when_old_text_ambiguous() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "x x x").unwrap();
        let result = execute_edit(&wd, "f.txt", "x", "y");
        assert!(
            result.text.contains("appears 3 times"),
            "got: {}",
            result.text
        );
        assert_eq!(fs::read_to_string(wd.join("f.txt")).unwrap(), "x x x");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_rejects_empty_old_text() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "x").unwrap();
        let result = execute_edit(&wd, "f.txt", "", "y");
        assert!(result.text.contains("empty"), "got: {}", result.text);

        let _ = fs::remove_dir_all(&wd);
    }

    #[tokio::test]
    async fn web_fetch_blocks_when_allowlist_empty() {
        // Default: no allowlist → every request is blocked.
        std::env::remove_var("LLM_WEB_ALLOWLIST");
        let result = execute_web_fetch("https://example.com/", 30).await;
        assert!(
            result.text.contains("LLM_WEB_ALLOWLIST"),
            "expected empty-allowlist block message, got: {}",
            result.text
        );
        assert_eq!(result.error_kind, Some(ToolErrorKind::Denied));
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_schemes() {
        std::env::set_var("LLM_WEB_ALLOWLIST", "example.com");
        let result = execute_web_fetch("file:///etc/passwd", 30).await;
        assert!(result.text.contains("not allowed"), "got: {}", result.text);
        std::env::remove_var("LLM_WEB_ALLOWLIST");
    }

    #[test]
    fn reduce_html_strips_scripts_and_tags() {
        let html = "<html><head><script>alert(1)</script><style>body{}</style></head><body><h1>Title</h1><p>Hello&nbsp;world &amp; goodbye</p></body></html>";
        let text = reduce_html(html);
        assert!(!text.contains("alert"), "script leaked: {text}");
        assert!(!text.contains("body{}"), "style leaked: {text}");
        assert!(!text.contains('<'), "tag leaked: {text}");
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world & goodbye"), "got: {text}");
    }

    #[test]
    fn reduce_html_collapses_whitespace() {
        let html = "<p>   line 1\n\n\nline   2   </p>";
        let text = reduce_html(html);
        assert!(text.contains("line 1 line 2"), "got: {text}");
    }

    #[tokio::test]
    async fn git_helper_runs_status() {
        // Init a throwaway repo and verify `git status` works.
        let wd = tmpwd();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&wd)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&wd)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&wd)
            .output()
            .unwrap();
        std::fs::write(wd.join("a.txt"), "hello").unwrap();

        let out = execute_git(&wd, &["status", "--short", "--branch"], 30).await;
        assert!(
            !out.text.contains("failed"),
            "git status failed: {}",
            out.text
        );
        assert!(
            out.text.contains("a.txt"),
            "expected a.txt in output: {}",
            out.text
        );

        let _ = fs::remove_dir_all(&wd);
    }

    #[tokio::test]
    async fn bash_timeout_kills_process() {
        // A command that sleeps longer than the tool timeout must return
        // a Timeout error quickly, not block the turn.
        let wd = tmpwd();
        let started = std::time::Instant::now();
        let result = execute_bash(&wd, "sleep 30", 1).await;
        let elapsed = started.elapsed();
        assert_eq!(
            result.error_kind,
            Some(ToolErrorKind::Timeout),
            "got: {}",
            result.text
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "timeout did not take effect promptly: {elapsed:?}"
        );
        let _ = fs::remove_dir_all(&wd);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_reports_exit_code() {
        let wd = tmpwd();
        let result = execute_bash(&wd, "echo out; echo err >&2; exit 7", 30).await;
        assert_eq!(result.exit_code, Some(7));
        assert!(result.text.contains("out"), "got: {}", result.text);
        assert!(result.text.contains("err"), "got: {}", result.text);
        let _ = fs::remove_dir_all(&wd);
    }

    /// UTF-8-heavy output must truncate on a char boundary — a mid-char
    /// `String::truncate` panics and would crash the tool loop (plan's
    /// 「巨量 UTF-8」 boundary case).
    #[tokio::test]
    async fn bash_output_truncates_on_utf8_boundary() {
        let wd = tmpwd();
        // 30_000 CJK chars = 90_000 bytes — beyond the 50 KB cap, and the
        // 50_000th byte lands mid-char.
        let result = execute_bash(&wd, "printf '中%.0s' {1..30000}", 30).await;
        assert!(result.error_kind.is_none(), "got: {}", result.text);
        assert!(result.truncated, "expected truncated flag");
        assert!(
            result.text.len() <= MAX_TOOL_OUTPUT_CHARS + 32,
            "output was not capped: {} bytes",
            result.text.len()
        );
        let _ = fs::remove_dir_all(&wd);
    }
}

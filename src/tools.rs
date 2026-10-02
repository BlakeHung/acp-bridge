//! Built-in tools for acp-bridge — file reading, directory listing, code search.
//! All tools are sandboxed to the working directory.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, warn};

/// Maximum file size to read (1 MB).
const MAX_FILE_SIZE: u64 = 1024 * 1024;
/// Maximum directory listing depth.
const MAX_LIST_DEPTH: usize = 3;
/// Maximum entries in directory listing.
const MAX_LIST_ENTRIES: usize = 200;

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

/// Execute a tool call and return the result as a string.
pub fn execute_tool(working_dir: &Path, name: &str, arguments: &Value) -> String {
    match name {
        "read_file" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            execute_read_file(working_dir, path)
        }
        "list_dir" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            execute_list_dir(working_dir, path)
        }
        "search_code" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let file_glob = arguments.get("file_glob").and_then(|v| v.as_str());
            execute_search_code(working_dir, pattern, file_glob)
        }
        "bash" => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            execute_bash(working_dir, command)
        }
        "write_file" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            execute_write_file(working_dir, path, content)
        }
        "edit" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
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
            execute_web_fetch(url)
        }
        "git_status" => execute_git(working_dir, &["status", "--short", "--branch"]),
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
            execute_git(working_dir, &args)
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
            execute_git(working_dir, &arg_refs)
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
            let staged = execute_git(working_dir, &add_args);
            if !staged.is_empty() && !staged.starts_with("(no output)") {
                output.push_str(&staged);
            }

            if message.is_empty() {
                output.push_str("Error: commit message is empty\n");
                return output;
            }

            let commit = execute_git(working_dir, &["commit", "-m", message]);
            output.push_str(&commit);
            output
        }
        _ => format!("Unknown tool: {name}"),
    }
}

fn execute_read_file(working_dir: &Path, relative_path: &str) -> String {
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return format!(
            "Error: path '{}' is outside the working directory or does not exist",
            relative_path
        );
    };

    if !path.is_file() {
        return format!("Error: '{}' is not a file", relative_path);
    }

    // Check file size
    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > MAX_FILE_SIZE => {
            return format!(
                "Error: file is too large ({} bytes, max {} bytes)",
                meta.len(),
                MAX_FILE_SIZE
            );
        }
        Err(e) => return format!("Error reading file metadata: {e}"),
        _ => {}
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            debug!(path = %relative_path, bytes = content.len(), "read_file");
            content
        }
        Err(e) => format!("Error reading file: {e}"),
    }
}

fn execute_list_dir(working_dir: &Path, relative_path: &str) -> String {
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return format!(
            "Error: path '{}' is outside the working directory or does not exist",
            relative_path
        );
    };

    if !path.is_dir() {
        return format!("Error: '{}' is not a directory", relative_path);
    }

    let mut output = String::new();
    let mut count = 0;
    list_dir_recursive(&path, "", 0, &mut output, &mut count);

    if count >= MAX_LIST_ENTRIES {
        output.push_str(&format!(
            "\n... truncated ({MAX_LIST_ENTRIES} entries shown)\n"
        ));
    }

    debug!(path = %relative_path, entries = count, "list_dir");
    output
}

fn list_dir_recursive(
    dir: &Path,
    prefix: &str,
    depth: usize,
    output: &mut String,
    count: &mut usize,
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
                &entry.path(),
                &format!("{prefix}  "),
                depth + 1,
                output,
                count,
            );
        } else {
            output.push_str(&format!("{prefix}{name}\n"));
            *count += 1;
        }
    }
}

fn execute_search_code(working_dir: &Path, pattern: &str, file_glob: Option<&str>) -> String {
    if pattern.is_empty() {
        return "Error: search pattern is empty".to_string();
    }

    let mut results = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 50;

    search_dir(
        working_dir,
        working_dir,
        pattern,
        file_glob,
        &mut results,
        &mut match_count,
        MAX_MATCHES,
    );

    if match_count == 0 {
        return format!("No matches found for '{pattern}'");
    }

    if match_count >= MAX_MATCHES {
        results.push_str(&format!("\n... truncated ({MAX_MATCHES} matches shown)\n"));
    }

    debug!(pattern, matches = match_count, "search_code");
    results
}

fn search_dir(
    dir: &Path,
    working_dir: &Path,
    pattern: &str,
    file_glob: Option<&str>,
    results: &mut String,
    match_count: &mut usize,
    max_matches: usize,
) {
    if *match_count >= max_matches {
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

        // Skip hidden/noise directories
        if is_ignored_entry(&name) {
            continue;
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

/// Execute a bash command in the working directory with a timeout.
fn execute_bash(working_dir: &Path, command: &str) -> String {
    if command.is_empty() {
        return "Error: command is empty".to_string();
    }

    debug!(command, "bash");

    match Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("[stderr] ");
                result.push_str(&stderr);
            }
            if result.is_empty() {
                "(no output)".to_string()
            } else {
                // Truncate very long output
                if result.len() > 50_000 {
                    result.truncate(50_000);
                    result.push_str("\n... (truncated)");
                }
                result
            }
        }
        Err(e) => format!("Error executing command: {e}"),
    }
}

/// Maximum size of a file that `write_file` / `edit` will accept.
const MAX_WRITE_FILE_SIZE: u64 = 5 * 1024 * 1024;

fn execute_write_file(working_dir: &Path, relative_path: &str, content: &str) -> String {
    if relative_path.is_empty() {
        return "Error: path is empty".to_string();
    }
    if content.len() as u64 > MAX_WRITE_FILE_SIZE {
        return format!(
            "Error: content too large ({} bytes, max {} bytes)",
            content.len(),
            MAX_WRITE_FILE_SIZE
        );
    }

    // Resolve relative path against working_dir. Note: unlike read/list,
    // write_file does NOT require the file to exist — it must be allowed
    // to create new files. We still reject absolute paths and `..`
    // traversal by canonicalizing the parent directory after the write
    // would-be target.
    let cleaned = relative_path.trim_start_matches('/');
    let target = working_dir.join(cleaned);

    // Reject obvious `..` escapes: if any path component is `..`, refuse.
    for component in Path::new(cleaned).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return format!(
                "Error: path '{}' escapes the working directory",
                relative_path
            );
        }
    }

    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("Error creating parent directory: {e}");
        }
    }

    match std::fs::write(&target, content) {
        Ok(()) => {
            debug!(
                path = %relative_path,
                bytes = content.len(),
                "write_file"
            );
            format!("wrote {} bytes to {}", content.len(), relative_path)
        }
        Err(e) => format!("Error writing file: {e}"),
    }
}

/// Replace exactly one occurrence of `old_text` with `new_text` in the
/// file at `relative_path`. Fails clearly if `old_text` is missing or
/// appears more than once — the model should re-read the file and try
/// again rather than guess.
fn execute_edit(working_dir: &Path, relative_path: &str, old_text: &str, new_text: &str) -> String {
    if relative_path.is_empty() {
        return "Error: path is empty".to_string();
    }
    if old_text.is_empty() {
        return "Error: old_text is empty; refusing to do an open-ended replace".to_string();
    }

    // Use the same sandbox resolver as read_file so we cannot escape.
    let Some(path) = resolve_sandboxed_path(working_dir, relative_path) else {
        return format!(
            "Error: path '{}' is outside the working directory or does not exist",
            relative_path
        );
    };
    if !path.is_file() {
        return format!("Error: '{}' is not a file", relative_path);
    }

    let original = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return format!("Error reading file: {e}"),
    };

    let occurrences = original.matches(old_text).count();
    if occurrences == 0 {
        return format!(
            "Error: old_text not found in {}. Re-read the file and provide the exact current text.",
            relative_path
        );
    }
    if occurrences > 1 {
        return format!(
            "Error: old_text appears {} times in {}. Provide a more specific (longer) old_text that matches exactly once.",
            occurrences, relative_path
        );
    }

    let patched = original.replacen(old_text, new_text, 1);
    if let Err(e) = std::fs::write(&path, &patched) {
        return format!("Error writing file: {e}");
    }
    debug!(
        path = %relative_path,
        before = original.len(),
        after = patched.len(),
        "edit"
    );
    format!(
        "patched {} ({} bytes -> {} bytes)",
        relative_path,
        original.len(),
        patched.len()
    )
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
fn execute_web_fetch(url: &str) -> String {
    if url.is_empty() {
        return "Error: url is empty".to_string();
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return format!("Error: '{url}' is not a valid URL");
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return format!(
            "Error: scheme '{}' is not allowed (only http and https)",
            parsed.scheme()
        );
    }

    let allow = web_fetch_allowed_hosts();
    if allow.is_empty() {
        return "Error: web_fetch is disabled (LLM_WEB_ALLOWLIST is empty)".to_string();
    }
    let host = parsed
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    if !allow
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    {
        return format!(
            "Error: host '{host}' is not in LLM_WEB_ALLOWLIST ({})",
            allow.join(", ")
        );
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
        .user_agent(concat!("acp-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("Error building HTTP client: {e}"),
    };

    let resp = match client.get(parsed).send() {
        Ok(r) => r,
        Err(e) => return format!("Error fetching URL: {e}"),
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Read body with a hard cap so a malicious server can't OOM us.
    let bytes = match resp.bytes() {
        Ok(b) if b.len() > MAX_WEB_BODY_BYTES => {
            return format!(
                "Error: response body too large ({} bytes, max {} bytes)",
                b.len(),
                MAX_WEB_BODY_BYTES
            );
        }
        Ok(b) => b,
        Err(e) => return format!("Error reading response body: {e}"),
    };

    if !status.is_success() {
        return format!(
            "Error: HTTP {} {} (response body {} bytes)",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            bytes.len()
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
    out.push_str(&body);
    if out.len() > 50_000 {
        out.truncate(50_000);
        out.push_str("\n... (truncated)");
    }
    out
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

/// Run `git` in the working directory with the supplied args. Uses
/// `execute_bash` style output formatting (stdout + `[stderr]` line)
/// but with `git` as the binary directly so we don't shell-escape.
fn execute_git(working_dir: &Path, args: &[&str]) -> String {
    debug!(args = ?args, "git");

    let output = match Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return if e.kind() == std::io::ErrorKind::NotFound {
                "Error: git binary not found on PATH".to_string()
            } else {
                format!("Error running git: {e}")
            }
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return format!(
            "git {} failed (exit {:?}):\n{}\n{}",
            args.first().copied().unwrap_or(""),
            output.status.code(),
            stdout,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&stderr);
    }
    if result.is_empty() {
        "(no output)".to_string()
    } else if result.len() > 50_000 {
        result.truncate(50_000);
        result.push_str("\n... (truncated)");
        result
    } else {
        result
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
        assert!(result.contains("wrote"), "got: {result}");
        assert_eq!(fs::read_to_string(wd.join("hello.txt")).unwrap(), "hi\n");

        let result = execute_write_file(&wd, "hello.txt", "there\n");
        assert!(result.contains("wrote"));
        assert_eq!(fs::read_to_string(wd.join("hello.txt")).unwrap(), "there\n");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn write_file_rejects_parent_dir_traversal() {
        let wd = tmpwd();
        let result = execute_write_file(&wd, "../escape.txt", "x");
        assert!(result.starts_with("Error"), "got: {result}");
        assert!(!wd.parent().unwrap().join("escape.txt").exists());

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_replaces_unique_match() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "alpha beta gamma").unwrap();
        let result = execute_edit(&wd, "f.txt", "beta", "BETA");
        assert!(result.contains("patched"), "got: {result}");
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
        assert!(result.contains("not found"), "got: {result}");
        assert_eq!(fs::read_to_string(wd.join("f.txt")).unwrap(), "alpha beta");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_fails_when_old_text_ambiguous() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "x x x").unwrap();
        let result = execute_edit(&wd, "f.txt", "x", "y");
        assert!(result.contains("appears 3 times"), "got: {result}");
        assert_eq!(fs::read_to_string(wd.join("f.txt")).unwrap(), "x x x");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn edit_rejects_empty_old_text() {
        let wd = tmpwd();
        fs::write(wd.join("f.txt"), "x").unwrap();
        let result = execute_edit(&wd, "f.txt", "", "y");
        assert!(result.contains("empty"), "got: {result}");

        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn web_fetch_blocks_when_allowlist_empty() {
        // Default: no allowlist → every request is blocked.
        std::env::remove_var("LLM_WEB_ALLOWLIST");
        let result = execute_web_fetch("https://example.com/");
        assert!(
            result.contains("LLM_WEB_ALLOWLIST"),
            "expected empty-allowlist block message, got: {result}"
        );
    }

    #[test]
    fn web_fetch_rejects_non_http_schemes() {
        std::env::set_var("LLM_WEB_ALLOWLIST", "example.com");
        let result = execute_web_fetch("file:///etc/passwd");
        assert!(result.contains("not allowed"), "got: {result}");
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

    #[test]
    fn git_helper_runs_status() {
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

        let out = execute_git(&wd, &["status", "--short", "--branch"]);
        assert!(!out.contains("failed"), "git status failed: {out}");
        assert!(out.contains("a.txt"), "expected a.txt in output: {out}");

        let _ = fs::remove_dir_all(&wd);
    }
}

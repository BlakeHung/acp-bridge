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
        if name.starts_with('.')
            || name == "node_modules"
            || name == "target"
            || name == "__pycache__"
        {
            continue;
        }

        let file_type = entry
            .file_type()
            .unwrap_or_else(|_| std::fs::metadata(entry.path()).unwrap().file_type());
        if file_type.is_dir() {
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
        if name.starts_with('.')
            || name == "node_modules"
            || name == "target"
            || name == "__pycache__"
        {
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

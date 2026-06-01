//! Minimal benchmark harness for acp-bridge against any configured LLM backend.
//!
//! Triggered by the `--bench` CLI flag. Runs a fixed set of fixture prompts
//! against the user's LLM endpoint and reports wall-clock time plus token
//! counts from the backend's own usage / eval stats when available.
//!
//! Goal: give operators a reproducible local-AI benchmark they can run on
//! their own hardware (e.g. M4 Pro Mac Mini, NVIDIA workstation, AMD
//! workstation) so they can compare against cloud agents on real tasks
//! without inventing the harness themselves.

use crate::llm::{self, LlmConfig};
use serde_json::{json, Value};
use std::time::Instant;

/// A single benchmark fixture — a name + the user message text, plus an
/// optional per-fixture system prompt. `None` means no system message is
/// sent; the model's natural answer length is preserved so decode-heavy
/// fixtures (summarize, explain_concept) produce enough tokens for the
/// timing to be meaningful.
pub struct Fixture {
    pub name: &'static str,
    pub user_text: &'static str,
    pub system_prompt: Option<&'static str>,
}

pub fn default_fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "hello",
            user_text: "Hello! Respond with a one-line greeting.",
            system_prompt: Some("You are a concise assistant. Answer briefly."),
        },
        Fixture {
            name: "short_code",
            user_text: "Write a Rust function `fn add(a: i32, b: i32) -> i32` that returns a + b. Only the function, no commentary.",
            system_prompt: Some("You are a concise coding assistant. Output only the code."),
        },
        Fixture {
            name: "explain_concept",
            user_text: "Explain in detail why Rust requires explicit lifetime annotations on some struct definitions. Cover the borrow checker rationale and give an example.",
            system_prompt: None,
        },
        Fixture {
            name: "refactor",
            user_text: "Given `fn f(v: Vec<String>) -> usize { v.len() }`, rewrite so the function does not take ownership of `v`. Show only the new signature and body.",
            system_prompt: Some("You are a concise coding assistant."),
        },
        Fixture {
            name: "summarize",
            user_text: "Summarize the trade-off between Ollama and vLLM for serving a 32B model on consumer hardware. Cover throughput, batching, memory footprint, ops complexity, and quantization support.",
            system_prompt: None,
        },
    ]
}

/// Aggregate stats for one run.
pub struct RunResult {
    pub name: &'static str,
    pub wall_ms: u128,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Tokens per second for decode, when the backend reports enough to compute it.
    pub decode_tps: Option<f64>,
    pub error: Option<String>,
}

/// Run all fixtures sequentially and return the per-fixture results.
///
/// A single warm-up request runs first and its result is discarded. This
/// pulls model weights into RAM/VRAM, primes the prompt cache, and
/// settles the LLM serving stack so the first measured fixture isn't
/// penalised by cold-start cost.
pub async fn run(config: &LlmConfig, fixtures: &[Fixture]) -> Vec<RunResult> {
    println!("Warm-up: priming model cache (result discarded)…");
    let warmup_messages = vec![
        json!({"role": "system", "content": "Respond with one word."}),
        json!({"role": "user", "content": "Say 'ready'."}),
    ];
    let _ = llm::chat(config, &warmup_messages, None, None).await;

    let mut results = Vec::with_capacity(fixtures.len());
    for fx in fixtures {
        let mut messages = Vec::with_capacity(2);
        if let Some(sys) = fx.system_prompt {
            messages.push(json!({"role": "system", "content": sys}));
        }
        messages.push(json!({"role": "user", "content": fx.user_text}));

        let start = Instant::now();
        let response = llm::chat(config, &messages, None, None).await;
        let wall_ms = start.elapsed().as_millis();

        match response {
            Ok(val) => results.push(parse_result(fx.name, &val, wall_ms)),
            Err(e) => results.push(RunResult {
                name: fx.name,
                wall_ms,
                prompt_tokens: None,
                completion_tokens: None,
                decode_tps: None,
                error: Some(e),
            }),
        }
    }
    results
}

/// Parse OpenAI-compatible `usage` or Ollama-native fields out of a response.
fn parse_result(name: &'static str, val: &Value, wall_ms: u128) -> RunResult {
    if let Some(usage) = val.get("usage") {
        let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
        let completion = usage.get("completion_tokens").and_then(|v| v.as_u64());
        let decode_tps = completion
            .filter(|c| *c > 0 && wall_ms > 0)
            .map(|c| c as f64 * 1000.0 / wall_ms as f64);
        return RunResult {
            name,
            wall_ms,
            prompt_tokens: prompt,
            completion_tokens: completion,
            decode_tps,
            error: None,
        };
    }

    let prompt = val.get("prompt_eval_count").and_then(|v| v.as_u64());
    let completion = val.get("eval_count").and_then(|v| v.as_u64());
    let eval_duration_ns = val.get("eval_duration").and_then(|v| v.as_u64());
    let decode_tps = match (completion, eval_duration_ns) {
        (Some(c), Some(d)) if d > 0 => Some(c as f64 * 1e9 / d as f64),
        _ => None,
    };

    RunResult {
        name,
        wall_ms,
        prompt_tokens: prompt,
        completion_tokens: completion,
        decode_tps,
        error: None,
    }
}

/// Print a human-readable report to stdout.
pub fn print_report(config: &LlmConfig, results: &[RunResult]) {
    println!();
    println!(
        "Bench results — {} (model={}, ollama_native={})",
        config.base_url,
        config.model,
        config.is_ollama_native()
    );
    let tps_label = if config.is_ollama_native() {
        "tok/s"
    } else {
        "tok/s*"
    };
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>10}",
        "fixture", "wall", "prompt_tok", "comp_tok", tps_label
    );
    println!("{}", "-".repeat(64));

    let mut total_wall: u128 = 0;
    let mut total_completion: u64 = 0;

    for r in results {
        let wall = format!("{:.2}s", r.wall_ms as f64 / 1000.0);
        let prompt = r
            .prompt_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let completion = r
            .completion_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let tps = r
            .decode_tps
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(|| "-".into());

        if let Some(err) = &r.error {
            println!("{:<20} {:>10} ERROR: {err}", r.name, wall);
            // Don't pollute the aggregate with error timings.
            continue;
        }
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>10}",
            r.name, wall, prompt, completion, tps
        );
        total_wall += r.wall_ms;
        if let Some(c) = r.completion_tokens {
            total_completion += c;
        }
    }

    println!("{}", "-".repeat(64));
    let agg_tps = if total_wall > 0 && total_completion > 0 {
        format!(
            "{:.1}",
            total_completion as f64 * 1000.0 / total_wall as f64
        )
    } else {
        "-".into()
    };
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>10}",
        "TOTAL",
        format!("{:.2}s", total_wall as f64 / 1000.0),
        "-",
        total_completion,
        agg_tps,
    );
    println!("(TOTAL tok/s is a wall-clock aggregate — not directly comparable to per-fixture decode tok/s.)");
    if !config.is_ollama_native() {
        println!(
            "(* OpenAI-compatible mode uses HTTP wall-clock, so per-fixture tok/s includes \
             prompt-eval (TTFT) and network transit; native Ollama uses eval_duration only.)"
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_usage() {
        let val = json!({
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 50,
                "total_tokens": 62
            }
        });
        let r = parse_result("t", &val, 1000);
        assert_eq!(r.prompt_tokens, Some(12));
        assert_eq!(r.completion_tokens, Some(50));
        assert!(r.decode_tps.unwrap() > 49.0 && r.decode_tps.unwrap() < 51.0);
    }

    #[test]
    fn parses_ollama_native_eval_stats() {
        let val = json!({
            "prompt_eval_count": 30,
            "eval_count": 100,
            "eval_duration": 2_000_000_000u64,
        });
        let r = parse_result("t", &val, 2100);
        assert_eq!(r.prompt_tokens, Some(30));
        assert_eq!(r.completion_tokens, Some(100));
        let tps = r.decode_tps.unwrap();
        assert!((49.0..51.0).contains(&tps), "tps = {tps}");
    }

    #[test]
    fn missing_usage_returns_dashes() {
        let val = json!({"choices": [{"message": {"content": "hi"}}]});
        let r = parse_result("t", &val, 500);
        assert_eq!(r.prompt_tokens, None);
        assert_eq!(r.completion_tokens, None);
        assert!(r.decode_tps.is_none());
    }
}

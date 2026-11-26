//! Bounded working context. All counts are conservative estimates, not tokenizer counts.
use serde_json::{json, Value};

/// Flat per-image charge for context estimation. The vision projector
/// converts an image into a few hundred to ~1k tokens depending on
/// resolution; the exact count lives server-side, so this carries
/// margin instead of pretending precision. Transport size is NOT
/// context cost: a 1 MB photo ships ~1.4 MB of base64 on the wire but
/// only its projected tokens count against the window — estimating
/// from the serialized payload overestimates by ~1000x and locally
/// rejects every image prompt as "protected and oversized".
pub const IMAGE_CONTEXT_ESTIMATE_TOKENS: u64 = 1536;

/// Clone `v` with image payloads removed, counting how many were
/// stripped. Covers the three encodings this bridge produces or
/// receives: Ollama `images: [b64, …]`, OpenAI `{"type":"image_url",
/// "image_url":{"url":"data:…"}}` parts, and inbound ACP `{"type":
/// "image","data":…}` blocks.
fn strip_image_payloads(v: &Value, count: &mut u64) -> Value {
    match v {
        Value::Object(map) => {
            let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "image_url" || (t == "image" && v.get("data").is_some()) {
                *count += 1;
                return Value::Null;
            }
            let mut m = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if k == "images" {
                    if let Some(arr) = val.as_array() {
                        *count += arr.len() as u64;
                    }
                    continue;
                }
                m.insert(k.clone(), strip_image_payloads(val, count));
            }
            Value::Object(m)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|e| strip_image_payloads(e, count)).collect())
        }
        _ => v.clone(),
    }
}

/// Include serialized metadata, tool arguments, roles and a template allowance.
/// Non-ASCII gets a larger allowance than prose in ASCII. Provider usage calibrates this.
pub fn estimate_value(value: &Value) -> u64 {
    let mut images = 0u64;
    let stripped = strip_image_payloads(value, &mut images);
    let text = stripped.to_string();
    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
    let unicode = text.chars().filter(|c| !c.is_ascii()).count() as u64;
    ascii.div_ceil(3) + unicode * 2 + 16 + images * IMAGE_CONTEXT_ESTIMATE_TOKENS
}

pub fn estimate_tokens(messages: &[Value]) -> u64 {
    messages.iter().map(estimate_value).sum::<u64>().max(1)
}

pub fn estimate_request(messages: &[Value], tools: Option<&[Value]>) -> u64 {
    estimate_tokens(messages)
        + tools
            .map(|t| t.iter().map(estimate_value).sum::<u64>())
            .unwrap_or(0)
        + 64
}

/// Prefix marking the folded working-memory summary the engine inserts.
/// Once the summary replaced older user turns, this message may be the
/// only live copy of their constraints — fallback must not drop it
/// silently. Its size is bounded at insertion, so protecting it cannot
/// create an unshrinkable floor; if it still cannot fit, the caller
/// reports the failure instead of losing the constraints.
pub const WORKING_MEMORY_PREFIX: &str = "[bridge working-memory summary";

/// A tool-call assistant and every following tool result are one indivisible group.
pub fn groups(messages: &[Value]) -> Vec<std::ops::Range<usize>> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let start = i;
        i += 1;
        if messages[start]
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty())
        {
            while i < messages.len() && messages[i]["role"] == "tool" {
                i += 1;
            }
        }
        result.push(start..i);
    }
    result
}

fn protected(messages: &[Value], group: &std::ops::Range<usize>) -> bool {
    group.end == messages.len()
        || group.clone().any(|i| {
            messages[i]["role"] == "system"
                || messages[i]["role"] == "user"
                || messages[i]["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(WORKING_MEMORY_PREFIX))
        })
}

/// Elision is idempotent and applies only to tool output, including the newest result.
/// Arguments and user instructions are never cut in half.
fn elide_tool(message: &mut Value) -> bool {
    if message["role"] != "tool" {
        return false;
    }
    let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return false;
    };
    const KEEP: usize = 1024;
    if text.len() <= KEEP * 2 + 128 || text.contains("[bridge: tool output elided;") {
        return false;
    }
    let mut head = KEEP;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - KEEP;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    message["content"] = json!(format!(
        "{}\n[bridge: tool output elided; re-run a bounded read for missing details]\n{}",
        &text[..head],
        &text[tail..]
    ));
    true
}

fn loss_marker(dropped: usize) -> Value {
    json!({"role":"assistant", "content":format!("[bridge: {dropped} earlier messages compacted away; details unavailable, re-read evidence before relying on it]")})
}

/// A fallback, not semantic summarization. Callers must check the returned size.
pub fn compact_for_context(mut messages: Vec<Value>, budget: u64) -> (Vec<Value>, usize, usize) {
    let mut elided = 0;
    for i in 0..messages.len() {
        if estimate_tokens(&messages) <= budget {
            break;
        }
        if elide_tool(&mut messages[i]) {
            elided += 1;
        }
    }
    let mut dropped = 0;
    // Include the loss marker's allowance before selecting groups to remove.
    while estimate_tokens(&messages)
        + if dropped > 0 {
            estimate_value(&loss_marker(dropped))
        } else {
            0
        }
        > budget
    {
        let candidate = groups(&messages)
            .into_iter()
            .find(|g| !protected(&messages, g));
        let Some(group) = candidate else {
            break;
        };
        dropped += group.len();
        messages.drain(group);
    }
    if dropped > 0 {
        let index = usize::from(messages.first().is_some_and(|m| m["role"] == "system"));
        messages.insert(index, loss_marker(dropped));
    }
    (messages, dropped, elided)
}

/// Summarize the old prefix, keeping two complete recent groups and both task anchors.
/// Returned transcript is data for a separate no-tools summarization request.
pub fn summary_parts(messages: &[Value]) -> Option<(Vec<Value>, Vec<Value>)> {
    let groups = groups(messages);
    if groups.len() < 5 {
        return None;
    }
    let cut = groups[groups.len() - 2].start;
    let first = messages.iter().position(|m| m["role"] == "user");
    let last = messages.iter().rposition(|m| m["role"] == "user");
    let mut old = Vec::new();
    let mut kept = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if i >= cut || msg["role"] == "system" || Some(i) == first || Some(i) == last {
            kept.push(msg.clone());
        } else {
            old.push(msg.clone());
        }
    }
    if old.is_empty() {
        return None;
    }
    Some((old, kept))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_elision_makes_no_false_progress() {
        let mut m = json!({"role":"tool","content":"漢".repeat(20000)});
        assert!(elide_tool(&mut m));
        let once = m.clone();
        assert!(!elide_tool(&mut m));
        assert_eq!(m, once);
    }
    #[test]
    fn includes_tool_schema_and_arguments() {
        let msgs = vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"a","function":{"arguments":"x".repeat(12000)}}]}),
        ];
        assert!(estimate_request(&msgs, None) > 4000);
        assert!(estimate_request(&msgs, Some(&[json!({"description":"x".repeat(6000)})])) > 6000);
    }
    #[test]
    fn dropping_tools_preserves_pairs_and_latest_correction() {
        let msgs = vec![
            json!({"role":"system","content":"sys"}),
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","tool_calls":[{"id":"a"},{"id":"b"}],"content":"x".repeat(4000)}),
            json!({"role":"tool","tool_call_id":"a","content":"a"}),
            json!({"role":"tool","tool_call_id":"b","content":"b"}),
            json!({"role":"user","content":"do not deploy"}),
        ];
        let (out, dropped, _) = compact_for_context(msgs, 300);
        assert_eq!(dropped, 3);
        assert!(!out.iter().any(|m| m["role"] == "tool"));
        assert_eq!(out.last().unwrap()["content"], "do not deploy");
    }
    #[test]
    fn newest_large_tool_can_shrink_without_losing_call() {
        let messages = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","tool_calls":[{"id":"a"}]}),
            json!({"role":"tool","tool_call_id":"a","content":"文".repeat(20000)}),
        ];
        let (out, dropped, elided) = compact_for_context(messages, 2500);
        assert_eq!((dropped, elided), (0, 1));
        assert!(estimate_tokens(&out) <= 2500);
        assert_eq!(out[2]["tool_calls"][0]["id"], out[3]["tool_call_id"]);
    }

    #[test]
    fn summary_preserves_task_latest_correction_and_complete_tail() {
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","content":"old reasoning"}),
            json!({"role":"user","content":"do not publish"}),
            json!({"role":"assistant","tool_calls":[{"id":"a"},{"id":"b"}]}),
            json!({"role":"tool","tool_call_id":"a","content":"a"}),
            json!({"role":"tool","tool_call_id":"b","content":"b"}),
            json!({"role":"assistant","content":"recent"}),
        ];
        let (old, kept) = summary_parts(&messages).unwrap();
        assert_eq!(old.len(), 1);
        assert!(kept.iter().any(|m| m["content"] == "do not publish"));
        assert_eq!(groups(&kept).last().unwrap().len(), 1);
        assert!(groups(&kept).iter().any(|g| g.len() == 3));
    }

    /// After semantic folding, the summary may be the only live copy of
    /// constraints from compacted-away user turns. Fallback must drop
    /// other groups first — losing the summary silently loses task limits.
    #[test]
    fn fallback_keeps_working_memory_summary_constraints() {
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","content":"[bridge working-memory summary; verify details against original files]\nConstraints: do not publish"}),
            json!({"role":"assistant","content":"x".repeat(6000)}),
            json!({"role":"assistant","content":"y".repeat(6000)}),
            json!({"role":"user","content":"continue"}),
        ];
        let (out, dropped, _) = compact_for_context(messages, 2500);
        assert!(dropped > 0, "budget must force a real drop");
        assert!(out.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|c| c.contains("do not publish"))));
    }

    #[test]
    fn oversized_instructions_remain_intact_for_caller_to_reject() {
        let messages = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"文".repeat(5000)}),
        ];
        let (out, _, _) = compact_for_context(messages.clone(), 1000);
        assert_eq!(out, messages);
        assert!(estimate_tokens(&out) > 1000);
    }

    // Image payloads are transport, not context. A photo's base64 must
    // not meter as prompt text — every image counts a flat projected-
    // token estimate instead. Regression for "image prompt rejected
    // locally as protected/oversized" with a real photo-sized payload.
    #[test]
    fn image_payloads_estimate_by_count_not_bytes() {
        let big_b64 = "A".repeat(1_400_000); // ~1 MB photo in base64
                                             // Ollama shape: `images` array on the user message.
        let single = json!({"role":"user","content":"描述這張圖","images":[big_b64]});
        let est = estimate_value(&single);
        assert!(
            est < IMAGE_CONTEXT_ESTIMATE_TOKENS * 2,
            "one image must not exceed ~2x the flat rate, got {est}"
        );
        // Multi-image scales linearly, not by payload size.
        let three = json!({"role":"user","content":"t","images":[big_b64, big_b64, big_b64]});
        let est3 = estimate_value(&three);
        assert!(est3 > IMAGE_CONTEXT_ESTIMATE_TOKENS * 3);
        assert!(est3 < IMAGE_CONTEXT_ESTIMATE_TOKENS * 4);
        // OpenAI shape: `image_url` part with a data URI.
        let openai = json!({"role":"user","content":[
            {"type":"text","text":"描述"},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{big_b64}")}}
        ]});
        let est_o = estimate_value(&openai);
        assert!(est_o > IMAGE_CONTEXT_ESTIMATE_TOKENS);
        assert!(est_o < IMAGE_CONTEXT_ESTIMATE_TOKENS * 2);
    }

    #[test]
    fn image_history_stays_shrinkable_under_budget() {
        // Image + tool history: estimates must stay bounded so the
        // tool results can still be compacted, instead of the whole
        // history looking unshrinkable from the image's base64.
        let b64 = "B".repeat(700_000);
        let messages = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"看圖","images":[b64]}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"c1"}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"x".repeat(20_000)}),
        ];
        let est = estimate_tokens(&messages);
        assert!(
            est < 20_000,
            "history with one image + tool output must stay bounded, got {est}"
        );
        // Tight budget → elision trims the tool output while the
        // (protected) image turn stays present.
        let (out, _, elided) = compact_for_context(messages, 3000);
        assert!(elided > 0, "oversized tool output must still be elidable");
        assert!(estimate_tokens(&out) <= 3000);
        assert!(out.iter().any(|m| m.get("images").is_some()));
    }
}

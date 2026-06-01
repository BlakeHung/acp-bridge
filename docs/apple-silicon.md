# acp-bridge on Apple Silicon

This page covers running `acp-bridge` against a local LLM on M-series Macs (M1, M2, M3, M4, including Pro / Max / Ultra tiers, plus the Mac mini and Mac Studio). The goal is "second-choice for OpenCode local-AI users who hit pain" — a setup that just works on consumer Apple hardware.

`acp-bridge` itself does not run the model. It talks to a local LLM endpoint (Ollama, llama.cpp, LM Studio, etc.) that does. This page recommends which endpoint to pair it with on Apple Silicon and which model sizes are realistic per memory tier.

## What `--bench` says on Apple Silicon

At startup `acp-bridge` prints a one-line detection report:

```
INFO acp_bridge: Platform: macos aarch64
INFO acp_bridge: GPU: Apple Silicon GPU + Neural Engine (unified memory) (Metal)
INFO acp_bridge: Hint: Apple Silicon: Ollama uses Metal automatically; for >7B models consider MLX backends.
```

Run `acp-bridge --bench` against your configured `LLM_BASE_URL` to get reproducible tokens/sec numbers on your specific machine. The numbers in this doc are rough — yours will differ depending on macOS version, thermal headroom, and model quantization.

## Memory tier → model size

Apple Silicon uses a unified memory architecture: the GPU shares RAM with the CPU. Practical limits as of macOS 15:

| Unified RAM | Realistic max model (Q4_K_M GGUF) | Context window |
|---|---|---|
| 16 GB | 7B (Qwen2.5-Coder 7B, Llama 3.1 8B) | 8K–16K |
| 24 GB (M4 Pro low) | 13B–14B (Phi-4 14B, Qwen2.5-Coder 14B) | 16K–32K |
| 32 GB | 22B–27B (Mistral Small 22B, Gemma 3 27B Q3) | 16K–32K |
| 48 GB (M4 Pro mid) | 32B (Qwen2.5-Coder 32B Q4) | 16K–32K |
| 64 GB (M4 Pro high) | 32B comfortably + system; 70B Q3 tight | 32K |
| 96 GB / 128 GB (M-Max / M-Ultra) | 70B Q4 with room | 32K+ |

Leave ~6–8 GB of RAM for the OS, your editor, and the model loading process. Pushing right to the limit triggers swap and tanks tokens/sec.

## Recommended backend: Ollama (default)

Ollama is the easiest path on Apple Silicon. It uses Metal automatically and handles model download + serving.

```bash
brew install ollama
ollama serve &                # default: http://localhost:11434
ollama pull qwen2.5-coder:32b-instruct-q4_K_M
```

Point `acp-bridge` at it:

```bash
export LLM_BASE_URL=http://localhost:11434       # Ollama native (recommended)
export LLM_MODEL=qwen2.5-coder:32b-instruct-q4_K_M
acp-bridge
```

URL **without** `/v1` triggers Ollama native mode (NDJSON streaming, eval-duration stats). URL **with** `/v1` triggers OpenAI-compatible mode — both work but native mode gives `acp-bridge --bench` richer numbers.

### Tuning

| Env / setting | Default | When to change |
|---|---|---|
| `OLLAMA_NUM_PARALLEL` | 1 | Increase only if your unified RAM has headroom for N copies of the model |
| `OLLAMA_KEEP_ALIVE` | 5m | Set to `-1` to keep model resident (avoids reload latency at the cost of RAM) |
| `OLLAMA_FLASH_ATTENTION` | 0 | Set `1` on M3/M4 — usually faster + lower memory |
| `OLLAMA_KV_CACHE_TYPE` | `f16` | `q8_0` halves KV cache size with minor quality cost; useful at 32K context on 24/32 GB |

## When to use MLX instead of Ollama

[MLX](https://github.com/ml-explore/mlx) is Apple's native ML framework. For inference on M-series, an MLX-based server (e.g. [`mlx-lm`](https://github.com/ml-explore/mlx-lm)) is typically 1.2–1.8x faster than llama.cpp Metal on the same model + quantization, especially for >13B models.

Trade-off:

- **Ollama**: easier to install, automatic model management, slightly slower
- **MLX**: faster decode, manual model conversion, requires `mlx_lm.server` setup

Use MLX when the model is >13B and benchmark numbers actually matter. Use Ollama otherwise — the throughput gap on 7B–13B is small and Ollama's UX is worth it.

MLX server pointed at OpenAI-compat endpoint:

```bash
pip install mlx-lm
mlx_lm.server --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit --port 8000
export LLM_BASE_URL=http://localhost:8000/v1     # OpenAI compat
export LLM_MODEL=mlx-community/Qwen2.5-Coder-32B-Instruct-4bit
acp-bridge
```

## Common pain points

- **First request after idle is slow** — model unload + reload. Set `OLLAMA_KEEP_ALIVE=-1` if you have RAM to spare.
- **Tokens/sec drops over time** — thermal throttling, especially in the fanless MacBook Air / Mac mini. Run `--bench` cold vs after 10 minutes of inference to see the curve.
- **Swap during inference** — model + your editor + browser exceeded RAM. Either drop to a smaller model, lower quantization (Q4 → Q3), or close some apps. Don't fight it with macOS memory pressure tooling; just use a smaller model.
- **Context length silently truncated** — Ollama defaults to ~2K context unless overridden. Set `num_ctx` in the model's Modelfile or via the API. `acp-bridge` doesn't override this — it's the LLM endpoint's job.

## Reference rig — Mac mini M4 Pro

Configuration that the project uses as a reference Apple Silicon target:

- Mac mini M4 Pro, unified memory in the 48–64 GB range
- Ollama with `qwen2.5-coder:32b-instruct-q4_K_M`
- `OLLAMA_FLASH_ATTENTION=1`, `OLLAMA_KEEP_ALIVE=-1`
- 16K context for typical coding sessions

Expect roughly 15–25 tokens/sec decode on this setup. Run `acp-bridge --bench` to confirm on yours.

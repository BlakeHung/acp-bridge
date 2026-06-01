# acp-bridge on NVIDIA GPUs

This page covers running `acp-bridge` against a local LLM hosted on NVIDIA hardware — single GPU laptops up to dual / quad GPU workstations. Same goal as the Apple Silicon page: when an OpenCode user hits local-AI setup friction, the answer here should be specific enough to pick a backend and a model size without guessing.

`acp-bridge` itself does not run the model. It talks to whatever LLM endpoint serves it (Ollama, llama.cpp, vLLM, LM Studio, etc.) over OpenAI-compatible HTTP or Ollama native. This page recommends which endpoint to pair it with on which kind of NVIDIA rig.

## What `--bench` says on NVIDIA

`acp-bridge` calls `nvidia-smi` at startup and prints what it sees:

```
INFO acp_bridge: Platform: linux x86_64
INFO acp_bridge: GPU: NVIDIA GeForce RTX 3090 Ti (Cuda, 24576 MB VRAM)
INFO acp_bridge: GPU: NVIDIA GeForce RTX 3090 Ti (Cuda, 24576 MB VRAM)
INFO acp_bridge: Hint: Multi-GPU NVIDIA (2× cards, 48 GB total VRAM): vLLM with --tensor-parallel-size 2, or Ollama with OLLAMA_SCHED_SPREAD=1.
```

Run `acp-bridge --bench` against your configured `LLM_BASE_URL` to get concrete decode tokens/sec. Numbers below are rough — exact figures depend on driver, CUDA version, model quantization, and context length.

## VRAM tier → model size (single GPU)

| VRAM | Realistic model (Q4_K_M GGUF, 4K context) | Notes |
|---|---|---|
| 8 GB (RTX 3060/4060) | 7B–8B | Q4 only, short context |
| 12 GB (RTX 3060/4070) | 13B–14B Q4 | 13B Q5 fits but tight |
| 16 GB (RTX 4060 Ti / 4080) | 20B–22B Q4, or 14B Q6 | Sweet spot for single-card |
| 24 GB (RTX 3090 / 3090 Ti / 4090) | 32B Q4, or 70B Q2 (low quality) | 32B is the practical cap |
| 48 GB (A6000 / 6000 Ada) | 70B Q4 single card | Workstation tier |

Account for KV cache: longer context = more VRAM. A 32B model at 4K context fits in 24 GB; the same model at 32K context overflows. Use `OLLAMA_KV_CACHE_TYPE=q8_0` or vLLM's quantized KV cache to reclaim VRAM.

## Multi-GPU strategies

When you have ≥2 GPUs, there are two distinct strategies:

### Strategy A: Tensor parallelism (one big model, split across cards)

This is the right call when you want to serve a model larger than any single card's VRAM. **vLLM** is the production-grade option here:

```bash
pip install vllm
python -m vllm.entrypoints.openai.api_server \
  --model Qwen/Qwen2.5-Coder-32B-Instruct-AWQ \
  --tensor-parallel-size 2 \
  --max-model-len 16384 \
  --port 8000

export LLM_BASE_URL=http://localhost:8000/v1
export LLM_MODEL=Qwen/Qwen2.5-Coder-32B-Instruct-AWQ
acp-bridge
```

`--tensor-parallel-size N` must match the number of GPUs you want to use. acp-bridge's startup hint suggests this when it detects multiple NVIDIA cards.

vLLM handles batching well, so this scales further if you eventually serve multiple concurrent sessions.

### Strategy B: Layer offloading (one model, layers split across cards)

Lower-friction alternative when you want to run one model but don't want to deal with vLLM. Both Ollama and llama.cpp natively support this.

Ollama with `OLLAMA_SCHED_SPREAD=1`:

```bash
export OLLAMA_SCHED_SPREAD=1
ollama serve &
ollama pull qwen2.5-coder:32b-instruct-q4_K_M

export LLM_BASE_URL=http://localhost:11434
export LLM_MODEL=qwen2.5-coder:32b-instruct-q4_K_M
acp-bridge
```

Expected throughput hierarchy on 2× RTX 3090 Ti (48 GB total VRAM) running a 32B Q4 model:

1. **vLLM tensor-parallel-2 + AWQ** — fastest decode (~50–80 tok/s), best batching
2. **vLLM tensor-parallel-2 + GGUF** — similar decode, larger memory footprint
3. **Ollama spread** — easier setup, ~25–40 tok/s decode
4. **llama.cpp `-ngl 99` with manual split** — comparable to Ollama, more knobs

### Strategy C: Independent models per card

Less common but worth flagging: run two completely separate model servers, one per card, pinned via `CUDA_VISIBLE_DEVICES=0` and `CUDA_VISIBLE_DEVICES=1`. Useful when you want one card running a small model for fast tool-call iterations and the other running a large model for final synthesis. Not directly supported by `acp-bridge` (one `LLM_BASE_URL` per process), but you can run two `acp-bridge` instances.

## When to use which backend

| Backend | Best for | Trade-off |
|---|---|---|
| **Ollama** | Easiest setup, single-GPU, casual multi-GPU | Lower decode than vLLM, fewer batching knobs |
| **llama.cpp server** | Maximum control, custom quantizations (Q3 / IQ4_XS), embedded use | Manual model conversion, more flags to tune |
| **vLLM** | Production throughput, multi-GPU tensor-parallel, concurrent sessions | Requires AWQ / GPTQ quantization for best speed; heavier install |
| **LM Studio** | Desktop GUI, model browsing | Single-process, less suitable for headless servers |

For the "OpenCode local-AI is broken, find a second option" scenario, **Ollama is almost always the answer** unless the user already has vLLM running.

## Tuning checklist

- Verify CUDA: `nvidia-smi` shows the driver; the LLM server logs should mention CUDA at startup.
- For ≥2 cards on Ollama, set `OLLAMA_SCHED_SPREAD=1` — otherwise it loads the whole model on one card.
- For 24 GB cards running 32B at long context: try `OLLAMA_KV_CACHE_TYPE=q8_0` or `q4_0`.
- For vLLM: AWQ / GPTQ quantizations are usually 2–3x faster than GGUF for decode.
- Disable Flash Attention only if you see numerical issues — it's a clear win otherwise.
- Persistent loading: `OLLAMA_KEEP_ALIVE=-1` avoids cold-start latency between sessions.

## Reference rig — 2× RTX 3090 Ti

Configuration that the project uses as a reference NVIDIA target:

- 2× RTX 3090 Ti (48 GB total VRAM)
- Linux + recent CUDA driver
- Either:
  - **vLLM** with `--tensor-parallel-size 2` serving Qwen2.5-Coder-32B AWQ → ~50–80 tok/s decode, supports concurrent sessions
  - **Ollama** with `OLLAMA_SCHED_SPREAD=1` serving `qwen2.5-coder:32b-instruct-q4_K_M` → ~25–40 tok/s decode, simpler ops

Run `acp-bridge --bench` to validate on your specific driver + model combination.

## Common pain points

- **VRAM full, but only on card 0** — you forgot `OLLAMA_SCHED_SPREAD=1` (Ollama) or `--tensor-parallel-size N` (vLLM).
- **Decode is 5 tok/s on a 24 GB card** — model spilled into CPU RAM. Check `nvidia-smi` for memory; if the model is fully loaded, the issue is likely context length pushing the KV cache out. Drop context or use a quantized KV cache.
- **First call returns gibberish** — wrong chat template. vLLM and llama.cpp sometimes need an explicit `--chat-template` for instruction-tuned models that don't follow the default.
- **NCCL / tensor-parallel hangs at startup** — vLLM on 2 GPUs with mismatched VRAM (e.g. 3090 + 3090 Ti) can hang. Force same memory cap with `--gpu-memory-utilization 0.85`.

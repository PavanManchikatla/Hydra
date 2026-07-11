# Federated Consumer-Device Inference for 70B–80B LLMs
## Landscape research, technical deep dive, and system design

*Prepared July 2026. Sources are linked inline and collected at the end. Performance claims are either sourced or explicitly labeled as estimates.*

---

## Executive summary (read this before anything else)

The idea — pool phones, laptops, and desktops into one cluster that runs a 70B model — is real and shipping in partial forms today, but three of your implicit assumptions need correction up front:

1. **Tensor parallelism over Wi-Fi is physically infeasible.** A 70B dense model has ~80 transformer layers, and TP requires roughly two all-reduce collectives per layer per token. At even 2 ms per collective over Wi-Fi, that's ~320 ms of pure network time per token before any compute happens. Every credible consumer-cluster system (prima.cpp, Petals, exo's original design, Parallax) uses **pipeline/layer sharding** across slow links, reserving TP for fast interconnects (Thunderbolt, RDMA, intra-machine). exo's 2025–26 rewrite added tensor parallelism precisely because it pivoted to RDMA-over-Thunderbolt-5 Mac clusters, not Wi-Fi fleets.

2. **iPhones cannot be reliable inference workers.** iOS gives an app roughly half the device RAM before jetsam kills it, forbids sustained background compute, throttles thermally within minutes, and offers no daemon model. Phones belong at the *edge* of the design (client, tokenizer/embedding host, redundant shard, speculative-draft device), not in the critical token path. Android via Termux is marginally viable (prima.cpp proves it) but is still your straggler.

3. **For this topology, an ~80B-class sparse MoE beats a 70B dense model.** MoE models (e.g., gpt-oss-120B with ~5B active parameters, Qwen3-30B-A3B class) are memory-hungry but compute-light — exactly matching a cluster that has lots of *pooled* memory and weak *individual* compute. Your fleet's problem is compute and bandwidth, not aggregate RAM.

Realistic expectations, grounded in published numbers: prima.cpp (ICLR 2026) achieves **674 ms/token (~1.5 tok/s) for a 70B model on four ordinary home devices over Wi-Fi**, and ~26 tok/s for a 32B with speculative decoding. Petals achieves up to ~6 tok/s for Llama-2-70B over the internet on pooled consumer GPUs. A well-engineered v1 of your system on a mixed fleet (1 CUDA desktop + 1–2 Macs + 1 Android, wired where possible) should target **2–6 tok/s decode for 70B dense Q4, 10–25 tok/s for a modern MoE**, with prefill as the dominant pain point. That is "audiobook speed," usable for chat, not for agentic workloads — unless you go MoE.

The recommended build: **fork/extend the ggml/llama.cpp compute layer (MIT) — following prima.cpp's pipelined-ring design — with a new Rust control plane (QUIC transport, mDNS + rendezvous discovery, Halda-style heterogeneity-aware placement), int8-quantized activations on the wire, GGUF Q4 weights, and TurboQuant-class 3–4-bit KV cache on memory-constrained devices.** Details and justification in Part 3.

---

# PART 1 — LANDSCAPE RESEARCH

## 1.1 Distributed / peer-to-peer LLM inference systems

### exo (exo-explore/exo)
**What it is now (important — the project pivoted).** The original exo ("run your own AI cluster at home with everyday devices 📱💻🖥️⌚") is **archived as `ex-exo`**. The current exo is a rewrite focused on Apple Silicon: it uses **MLX as the inference backend and MLX distributed for communication**, ships **day-0 RDMA over Thunderbolt 5** (claimed 99% latency reduction between devices), **topology-aware automatic parallel placement** based on a realtime view of device resources and per-link latency/bandwidth, and **tensor parallelism** with claimed speedups of ~1.8× on 2 devices and ~3.2× on 4. It exposes OpenAI/Anthropic/Ollama-compatible APIs and a dashboard, and supports coordinator-only nodes (`--no-worker`). Flagship demos are 4× M3 Ultra Mac Studios running DeepSeek-V3.1-671B and Kimi-K2 over TP+RDMA. ([repo](https://github.com/exo-explore/exo))
- **Parallelism:** originally pipeline (layer shards in a ring); current version emphasizes tensor parallelism over RDMA/Thunderbolt, with topology-aware placement choosing PP or TP.
- **Platforms:** macOS first-class (GPU via MLX; the macOS app requires macOS Tahoe 26.2+); Linux currently **CPU-only** per the README; iOS/Android support from the old exo is gone.
- **License:** Apache-2.0. ~45k stars, actively developed (commits through June 2026).
- **Maturity/limits:** a third-party October 2025 code review found experimental-grade security, fault tolerance, and ops tooling (UDP-broadcast discovery, monolithic node logic) and judged it not production-ready. exo's own transparent benchmarks showed that for *single* requests, adding devices under pipeline parallelism can *reduce* tok/s (49.3 → 39.7 TPS for a 3B model going 1→3 M4 Pro nodes) while multi-request throughput scales ~2.2× on 3 devices — network latency, not bandwidth, is the bottleneck. ([exo blog](https://blog.exolabs.net/day-1/))
- **Reuse for you:** the topology-aware placement ideas and the MLX/TB5 island concept are directly reusable; the codebase itself is now Mac-centric and won't cover phones or CUDA desktops the way you need.

### Petals (bigscience-workshop/petals)
- **What:** BitTorrent-style collaborative inference/fine-tuning over the public internet. Servers each host a contiguous block of transformer layers; a client holds embeddings locally, finds a **chain of servers covering all layers** via a hivemind DHT, and streams hidden states through the chain. Supports Llama-3.1-405B, Mixtral, Falcon, BLOOM. ([repo](https://github.com/bigscience-workshop/petals), [paper](https://arxiv.org/abs/2209.01188))
- **Parallelism:** pipeline (layer sharding) with client-driven routing; 8-bit weight compression (LLM.int8) and **dynamic blockwise int8 quantization of hidden states on the wire** — a trick you should steal.
- **Fault tolerance (its best idea):** servers hold per-session KV; **clients cache the inputs they sent to each server, so when a server drops, the client finds a replacement and replays those inputs to rebuild the KV** — the canonical solution to dropout mid-stream.
- **Performance:** up to ~6 tok/s single-batch for Llama-2-70B, ~4 tok/s for Falcon-180B; 3–25× lower latency than disk/RAM offloading in realistic network conditions.
- **Platforms:** Linux/macOS Python + PyTorch + CUDA-class GPUs; not mobile.
- **License:** MIT. **Maturity:** the code works but core development has been dormant since ~2023–24; the public swarm's health fluctuates; community forks (e.g., Kwaai's OpenAI-Petal) keep it alive.
- **Reuse:** the *protocols* (DHT discovery, chain formation, KV-rebuild-on-failure, activation quantization) are the single best-documented fault-tolerance design in this space. The PyTorch/CUDA server runtime is wrong for your device fleet.

### llama.cpp RPC backend (ggml-org/llama.cpp)
- **What:** `rpc-server` processes on worker nodes expose their ggml backend over TCP; a master node holds the GGUF and offloads tensors/graph splits to workers via `--rpc`.
- **Parallelism:** effectively memory pooling with sequential graph execution; it is **designed to fit models that don't fit on one machine, not to speed anything up**. A known scheduler limitation leaves worker CPUs badly underutilized, and if the model fits on one node, adding RPC workers slows you down. ([practical writeup](https://hpcwithus.discoverer.bg/?p=439))
- **Platforms:** everything llama.cpp compiles on (Linux/macOS/Windows/Android; CUDA, Metal, Vulkan, CPU). **License:** MIT.
- **Limits:** no auth/encryption, no compression, static assignment, master must see the whole model file, no fault tolerance.
- **Reuse:** the ggml backend abstraction underneath it is exactly the device-abstraction layer you need; the RPC transport itself should be replaced.

### prima.cpp (ICLR 2026) — the closest thing to your spec that exists
- **What:** a distributed fork of llama.cpp targeting exactly "low-resource everyday home clusters": mixed CPUs/GPUs, insufficient RAM/VRAM, slow disks, Wi-Fi, heterogeneous OSs. ([paper](https://arxiv.org/abs/2504.08791), [OpenReview](https://openreview.net/forum?id=h0LjpOG1jq))
- **Key techniques:** **pipelined-ring parallelism (PRP)** — devices form a ring and can take multiple passes per token, overlapping disk I/O with compute/communication; **mmap-based lazy weight loading** keeping memory pressure <6–10% (solving the "prefetch-release" conflict); **Halda**, a heterogeneity-aware scheduler that solves the NP-hard co-assignment of layers to each device's CPU *and* GPU under RAM/VRAM constraints (it will drop devices that would slow the ring — "No layer is assigned to me" is a feature).
- **Results:** 70B at **674 ms/token TPOT on four ordinary home devices**; 32B + speculative decoding at 26 tok/s; claims **5–17× lower TPOT than llama.cpp RPC, exo, and distributed-llama** on 30B+ models.
- **Platforms:** Linux, macOS, Android and HarmonyOS via Termux; Windows on the roadmap; **GPU support is CUDA-only today, Vulkan on the roadmap**; ZeroMQ transport; uses the HiGHS solver for placement.
- **License:** MIT (llama.cpp lineage). **Maturity:** research-grade but public and runnable; single-request oriented; no dynamic membership mid-stream; no iOS.
- **Reuse:** this is your strongest foundation or at minimum your reference design — PRP + Halda are the published state of the art for your exact scenario.

### distributed-llama (b4rtaz/distributed-llama)
- **What:** C++ tensor-parallel inference across home devices over TCP/Ethernet; one root node + workers; praised for near-linear scaling on *fast wired* LANs and Raspberry Pi clusters. ([repo](https://github.com/b4rtaz/distributed-llama))
- **Limits:** requires a power-of-two worker count (2^n), is TP-only (so it inherits the all-reduce-per-layer network sensitivity — it works on wired Ethernet, suffers on Wi-Fi), supports a restricted model/quantization set, and assumes relatively symmetric workers. License: MIT.
- **Reuse:** good evidence for what wired-LAN TP can do; wrong strategy for a Wi-Fi, unequal-device fleet (the prima.cpp paper benchmarks it as "dllama" and beats it substantially in that regime).

### Cake (evilsocket/cake) — *[corrected July 2026; the original version of this section was outdated]*
- **What:** a Rust **multimodal inference server** that runs models single-node or shards transformer blocks across a heterogeneous cluster — iOS, Android, macOS, Linux, Windows — with CUDA, Metal, Vulkan, and CPU backends. Current features: zero-config mDNS clustering with layers assigned proportionally to each device's VRAM/compute, **master-streamed weight shards (workers need no model files; zstd-compressed, CRC32-verified, cached locally)**, 15 text-model families plus image (SD/FLUX) and TTS, and an OpenAI-compatible API with web UI. ([repo](https://github.com/evilsocket/cake))
- **Status:** actively developed (1,100+ commits, CI); README still labels it "experimental code... changed very quickly." It remains the only project with a real iOS/Android worker story.
- **License — the landmine, restated correctly:** **FAIR License v1.0.0**, not GPL. Non-commercial use is free; commercial use requires visible attribution; **any business use requires a signed commercial agreement with the author**. It is source-available/business-restrictive, which still rules it out as a dependency for a permissively licensed project — but for different reasons than previously stated.
- **Reuse:** now highly relevant *prior art*: its shard-streaming/content-caching design and cross-platform worker packaging are exactly what Hydra's model service and mobile workers need to replicate (independently, given the license). Note also that "open-weight" model licenses are a separate compliance axis from code licenses — check both.

### Parallax (Gradient Network, 2025–26)
- **What:** a decentralized inference framework spanning data-center GPUs down to Apple Silicon Macs. Its planning layer decomposes into (i) **model allocation** — placing layers of each model replica across diverse GPUs to jointly optimize latency and throughput under memory and link-bandwidth constraints — and (ii) **request-time pipeline selection**, stitching layers from different replicas into end-to-end chains that balance load and adapt to current conditions; claims up to 3.1× gains over prior decentralized systems. ([paper](https://gradient.network/parallax.pdf), [OpenReview](https://openreview.net/forum?id=1PhUigVew4))
- **Platforms:** NVIDIA GPUs (SGLang-derived runtime) + Apple Silicon (MLX); open-source repo under the GradientHQ org. Phones are not workers.
- **Reuse:** the two-level scheduler (static allocation + dynamic per-request chain selection) is the right mental model for your control plane; the runtime is heavier than your fleet wants.

### Academic systems worth mining (research-only, not shipping)
- **TPI-LLM** (tensor parallelism on edge devices with on-demand weight loading and a sliding memory scheduler), **Galaxy** and **Hepti** (heterogeneity-aware TP partitioning for edge clusters), **AirInfer** (all-reduce via over-the-air analog superposition on wireless — exotic but clarifies that TP's all-reduce is *the* wireless bottleneck), **BalanceKV** (KV compression via discrepancy theory). The prima.cpp related-work section is a good index of this literature. One more relevant thread: a 2026 line of work on **prompt-reconstruction attacks against distributed inference** shows that intermediate activations leak input content — relevant if any shard runs on hardware you don't trust.

### Ranking for your use case (heterogeneous consumer fleet, Wi-Fi/LAN, phones included)

| System | Fit | Why |
|---|---|---|
| **prima.cpp** | ★★★★★ | Built for exactly this: heterogeneity-aware placement, Wi-Fi-tolerant PRP, Android support, mmap OOM-safety, best published 70B-on-home-devices numbers. Gaps: iOS, Vulkan, dynamic membership. |
| **Petals (design, not code)** | ★★★★ | Best fault-tolerance and WAN design ever shipped in this space; runtime is CUDA/PyTorch-only. |
| **llama.cpp + ggml (as substrate)** | ★★★★ | Not a distributed system, but the only compute layer that already targets Metal, CUDA, Vulkan, and CPU on every OS you listed. |
| **exo (current)** | ★★★ | Excellent for a Mac-island sub-cluster (TP over TB5); no phones, Linux CPU-only. |
| **Parallax** | ★★★ | Right scheduler ideas; heavier GPU-server runtime; no phones. |
| **distributed-llama** | ★★ | Wired-LAN TP only; 2^n symmetric workers; wrong regime. |
| **Cake** | ★★★ | Actively developed; only real iOS/Android worker + shard-streaming design worth studying; FAIR license (business use needs a commercial agreement) rules it out as a dependency. |
| **llama.cpp RPC (as-is)** | ★★ | Memory pooling only; utilization bug; no security or fault tolerance. |

## 1.2 Cross-platform on-device runtimes (your worker's engine options)

- **llama.cpp / ggml** (MIT). CPU (NEON/AVX), Metal, CUDA, Vulkan, HIP, SYCL, OpenCL backends behind one `ggml-backend` interface; GGUF is the de-facto quantized distribution format; builds on iOS, Android, macOS, Linux, Windows. Mature, huge community, continuously optimized. No ANE/NPU access (GPU/CPU only) — that's the main gap versus vendor stacks. **This is the natural worker engine.**
- **MLC-LLM / TVM** (Apache-2.0). Compiles models to Metal, CUDA, Vulkan, OpenCL, and WebGPU; first-class iOS and Android apps; strong performance from kernel autotuning. Costs: a compile-per-model-per-device pipeline, its own weight format, and a heavier toolchain; distributed serving exists (MLC serve, disco) but targets GPU servers. Great fallback where ggml kernels lag (e.g., some Android GPUs via OpenCL/Vulkan).
- **ExecuTorch** (BSD-3, PyTorch). Production mobile runtime with delegate backends: XNNPACK (CPU), Core ML/MPS (Apple, incl. some ANE paths), Qualcomm QNN, Vulkan, MediaTek. The 2026 consensus recommendation for *production mobile* apps. But it executes whole exported programs — slicing a model into net-transparent layer shards fights its design.
- **MediaPipe LLM Inference API** (Apache-2.0, Google). Easiest path to on-device LLMs on Android/iOS/Web, but a closed set of supported models and no partial-model execution. Not composable into a shard worker.
- **Core ML** (Apple, OS framework, proprietary but free to use). The only public route to the **Apple Neural Engine**. iOS 18+ stateful models make KV-cache-in-Core-ML viable, and Apple publishes ANE-optimized transformer guidance. Reality check: ANE shines for encoder-style and small decoder workloads; for shard-style decoding, Metal (via ggml/MLX) is simpler and usually comparable; conversion friction is high. Treat ANE as a later optimization, not a dependency.
- **ONNX Runtime (+ GenAI extension)** (MIT). Execution providers for QNN, Core ML, NNAPI/Vulkan, CUDA, DirectML. Strong for NPU access on Snapdragon (QNN EP). Cost: ONNX export pain for cutting-edge LLM architectures; GenAI extension covers popular models only.
- **MLX** (MIT, Apple). Best-in-class Apple Silicon performance with unified memory; `mlx-lm` covers most open models; **MLX distributed** gives you collectives over Thunderbolt/Ethernet (this is what exo builds on). macOS/iOS only.
- **Ollama** (MIT). Single-node UX wrapper over a ggml-based engine. Not a building block for sharding; useful as a UX benchmark and API-compatibility target.

| Runtime | Metal | ANE | CUDA | Vulkan/Android | CPU | Shardable? | Verdict for worker engine |
|---|---|---|---|---|---|---|---|
| llama.cpp/ggml | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ (layer ranges natively) | **Primary** |
| MLX | ✅ | ❌ | ❌ | ❌ | ✅(mac) | ✅ (mlx distributed) | Mac-island TP engine |
| MLC-LLM | ✅ | ❌ | ✅ | ✅ | ✅ | partial (disco) | Fallback for weird Android GPUs |
| ExecuTorch | via CoreML | partial | ❌ | ✅ | ✅ | ❌ (whole-program) | Not for shards |
| ONNX Runtime | via CoreML | partial | ✅ | ✅(QNN/NNAPI) | ✅ | ❌ practically | NPU experiments only |
| Core ML | ✅ | ✅ | ❌ | ❌ | ✅ | ❌ | ANE optimization, later |
| MediaPipe LLM | ❌ | ❌ | ❌ | ✅ | ✅ | ❌ | Not composable |

## 1.3 Server-grade parallelism to borrow ideas from

- **vLLM** (Apache-2.0): Megatron-style **tensor parallelism** (column/row-split linear layers, two all-reduces per transformer block), **pipeline parallelism** across nodes, **PagedAttention** (block-based KV allocation — steal this for fragmentation-free KV on every worker), continuous batching, and expert parallelism for MoE. Assumes NVLink/InfiniBand for TP; its own docs steer multi-node users toward PP when interconnect is slow — the same conclusion you'll reach.
- **TGI** (Apache-2.0 again since v2.0 — it was briefly under the restrictive HFOIL license in 2023, now resolved): TP within node, router/scheduler separation (a clean control-plane pattern), quantization integrations.
- **TensorRT-LLM** (Apache-2.0 code, NVIDIA-only runtime): the ceiling for single-vendor performance; its in-flight batching and KV reuse designs are instructive; nothing portable.
- **DeepSpeed-Inference** (permissive, MIT-family): pioneered kernel-injection TP and ZeRO-Inference (weight streaming from CPU/NVMe — conceptually the ancestor of prima.cpp's mmap trick).
- **Megatron-LM** (BSD-style NVIDIA license): the canonical TP/PP/sequence-parallel formulation; read it to understand *why* TP needs ~2 all-reduces per layer and why sequence parallelism only pays with fast interconnects.
- **Ray Serve** (Apache-2.0): not an inference engine; its actor-based placement groups and health-checking are a decent control-plane reference, but it's far too heavy to run on phones.

**Key takeaway table (parallelism strategies vs. your constraints):**

| Strategy | Per-token network cost (P devices, 80-layer 70B) | Wi-Fi viable? | Tolerates unequal devices? | Tolerates dropout? |
|---|---|---|---|---|
| Tensor parallel | ~160 collectives/token, all devices lockstep | ❌ (needs TB/10GbE+) | Poorly (slowest gates all) | No (any loss stalls) |
| Pipeline / layer shard | P−1 point-to-point hops/token, ~8–16 KB each | ✅ | Yes (assign layers ∝ speed) | Yes (reassign a stage) |
| Sequence parallel | Only helps prefill; TP-grade comms | ❌ | — | — |
| Expert parallel (MoE) | Router→expert scatter/gather per MoE layer | Marginal | Yes (experts ∝ memory) | Partially (drop experts ≈ quality loss) |

---

# PART 2 — TECHNICAL DEEP DIVE

## 2.1 Partitioning strategy: what actually fits unequal, unreliable devices

**Pipeline (layer) sharding wins, and it isn't close.** The arithmetic: a Llama-3-70B-class dense model has 80 transformer blocks, hidden size 8192. Between pipeline stages you transmit one hidden-state vector per token: 8192 × 2 bytes = **16 KB FP16 (8 KB at int8)**. Decode traffic for a 4-stage pipeline is therefore ~3 hops × 8–16 KB ≈ 24–48 KB per token — trivial bandwidth; what matters is per-hop *latency* (exo's own benchmarking reached the same conclusion: "network latency, not bandwidth, is typically the bottleneck").

Tensor parallelism instead splits every weight matrix and requires an all-reduce after attention and after the MLP in every block: ~2 × 80 = 160 synchronous collectives per generated token, each moving O(hidden × batch) and, worse, each paying full round-trip latency across *all* devices. **Estimate:** at an optimistic 2 ms per collective on good Wi-Fi, that's 320 ms/token of pure communication — a hard ceiling of ~3 tok/s with *zero* compute, degrading catastrophically with Wi-Fi jitter. This is why distributed-llama demands wired Ethernet and symmetric nodes, and why exo only embraced TP after moving to RDMA over Thunderbolt 5. TP also fails your heterogeneity requirement (every device does the same fraction of every layer, so the slowest device gates every layer) and your fault-tolerance requirement (losing one TP rank corrupts every layer).

The right refinement is **hierarchical parallelism**: pipeline sharding across slow links, with TP permitted *inside* fast islands — two Macs on Thunderbolt behave as one bigger pipeline stage (exo-style), a desktop's GPU+CPU pair behaves as one stage internally split (prima.cpp's Halda co-assigns each device's CPU and GPU workloads). prima.cpp's **pipelined-ring parallelism** adds one more idea worth adopting: devices form a ring and may take *multiple rounds* per token, which lets a device hold (and mmap-stream) more layers than fit in RAM, overlapping disk I/O with the ring's compute — this is what keeps memory pressure under 6% while still hitting 674 ms/token on 70B.

**Dense 70B vs. ~80B MoE changes the answer materially.** A dense 70B does ~70B MAC-weights of work per token; your weakest ring member must push its layer slice through its NPU-less CPU/GPU. An 80–120B-class MoE with ~5–13B active parameters (gpt-oss-120B, Mixtral-style, Qwen3-A3B lineage) needs the *memory* of its full parameter count but only the *compute* of its active count — a ~10× compute reduction per token that lands exactly on your fleet's weakness. Two viable mappings: (a) keep layer sharding — each stage holds all experts for its layers; routing stays local to the stage, so network cost is identical to dense PP (recommended); or (b) expert parallelism — experts scattered by memory capacity, but then every MoE layer does a cross-device scatter/gather per token, which Wi-Fi latency punishes. Verdict: **prefer an MoE model, still sharded by layers.** The one caution: MoE weight footprint is larger (gpt-oss-120B ≈ 60–65 GB at its native ~4-bit MXFP4), so your pooled-memory budget must be honest.

## 2.2 Heterogeneity and load balancing

The published state of the art is prima.cpp's **Halda** scheduler: model the cluster with per-device compute throughput (CPU and GPU separately), RAM/VRAM ceilings, disk read speed, OS memory-management behavior, and per-link latency/bandwidth; then solve the NP-hard assignment of (layer count per device, CPU/GPU split per device, device inclusion) to minimize token latency, using an ILP-style formulation (prima.cpp links HiGHS). Two of its behaviors are the correct design instincts: it **excludes devices** whose inclusion would raise ring latency (a slow phone is often net-negative), and it treats **layers ∝ effective speed** rather than ∝ memory, using mmap streaming to break the memory constraint. Parallax adds the second level you'll eventually want: separate the slow **allocation** problem (re-solved on membership change) from the fast per-request **path selection** problem.

Practical measurement plan: on worker startup, run a ~10-second self-benchmark (a few transformer blocks at target quantization on each local backend) to get tok-throughput per layer; measure link RTT/bandwidth with a UDP echo + bulk-transfer probe to each peer; re-probe cheaply in the background. Rebalance policy: rebalance **between requests only** (moving a layer means moving 100s of MB of weights plus that layer's KV — never mid-token); hysteresis so thermal wobble doesn't trigger thrash; keep a "shadow plan" precomputed for the most likely failure (strongest device leaving, weakest device leaving).

## 2.3 Networking: the real bottleneck (correctly identified)

**Budgets.** Decode: per token, PP traffic is (stages − 1) × hidden × bytes. For 70B/8192-hidden at int8, a 4-stage ring moves ~24 KB/token — even 50 Mbit/s Wi-Fi supplies that in <4 ms. The killer is latency: home Wi-Fi RTTs of 2–10 ms with 50–100 ms tail spikes (bufferbloat, channel contention) directly add (stages − 1) × RTT to every token. **Estimates:** 4 stages × 3 ms typical = ~9–12 ms/token network floor (fine); tail spikes stall the whole ring (this is why you want small socket buffers, no Nagle, and QUIC's loss recovery rather than TCP head-of-line blocking). Prefill is different: you send hidden states for *all* prompt tokens per hop — a 4,000-token prompt at int8 is ~32 MB per hop, i.e., ~5–6 s per hop on 50 Mbit/s Wi-Fi. Prefill, not decode, is where bandwidth hurts; mitigations below.

**Activation compression.** Petals established that dynamic blockwise **int8 quantization of hidden states is quality-safe** in production; that halves FP16 traffic. Research variants push activations to 4–6 bits or top-k sparsification, but int8 is the shipping-today answer. Do not confuse this with KV-cache quantization (KV never crosses the wire in PP — each stage owns the KV for its layers).

**Protocol.** gRPC is fine for control but adds latency/framing overhead in the token hot path. Recommended: **QUIC** (quinn in Rust) for both planes — 0-RTT reconnects, stream multiplexing without head-of-line blocking, built-in TLS, datagram support for probes — with a zero-copy framing format (FlatBuffers or Cap'n Proto) for activation frames: {request_id, token_idx, layer_range, dtype, tensor bytes}. prima.cpp uses ZeroMQ, which is a pragmatic LAN alternative but gives you no security story.

**NAT traversal / WAN.** If devices span networks: embed or reuse a WebRTC-style ICE stack (STUN hole-punching, TURN relay fallback) or simply document **Tailscale/WireGuard** as the supported WAN mode for v1 (this is exactly the workaround exo users apply today; note exo's UDP-broadcast discovery famously ignored Tailscale's 100.x addresses — design your discovery to enumerate all interfaces). Petals/hivemind's libp2p-based DHT with relays is the fully-decentralized reference if you outgrow that. Be honest with users: WAN adds 20–80 ms per hop; a WAN hop belongs at *one* pipeline boundary at most, and prefill over WAN for long prompts is minutes.

## 2.4 Memory fitting: weights + KV cache

**Weights.** For your fleet the GGUF k-quant/i-quant family is the practical choice (works on every backend, streams via mmap): 70B at Q4_K_M ≈ **~42–43 GB**, IQ4_XS ≈ ~38 GB, Q3 ≈ ~34 GB with visible quality cost. GPTQ and AWQ (both ~4-bit, calibration-based) are excellent on CUDA (AWQ generally more robust at 4-bit) but their optimized kernels don't span Metal/Vulkan/CPU the way ggml does — in a mixed cluster, format uniformity beats a point-optimal CUDA kernel. Realistic pooled budget check (estimate): desktop RTX 4090 (24 GB VRAM + host RAM) + MacBook M-series 32 GB (usable ~24 GB) + Android 16 GB (usable ~8 GB via Termux) + iPhone (usable ~3–4 GB, if used at all) ⇒ ~60 GB of *comfortable* accelerator-adjacent memory: Q4 70B fits with little slack; mmap streaming (prima.cpp-style) is your OOM safety valve, and an MoE's MXFP4 native quantization changes the arithmetic in your favor.

**KV cache pressure — the numbers.** Llama-3-70B: 80 layers, GQA with 8 KV heads × head-dim 128 ⇒ 2 × 8 × 128 = 2,048 values/token/layer ⇒ FP16 = 4 KB/layer/token = **320 KB per token across the model**; a 32k context is ~10 GB, 128k is ~40 GB FP16 — spread across stages ∝ layers held. A phone holding even 8 layers at 32k context needs ~1 GB of KV at FP16. This is where KV quantization is not optional.

### TurboQuant, evaluated precisely (as requested)

**What it is:** TurboQuant (Zandieh, Daliri, Hadian, Mirrokni — Google Research/DeepMind/NYU; ICLR 2026; [arXiv:2504.19874](https://arxiv.org/abs/2504.19874)) is a **training-free, calibration-free ("data-oblivious"), online vector-quantization algorithm** with near-optimal distortion guarantees across bit-widths. Mechanism: apply a random rotation (fast Walsh–Hadamard transform) to each vector so coordinates become approximately Gaussian, then apply optimal Lloyd–Max scalar quantizers per rotated coordinate; a second variant targets unbiased **inner-product** estimation by adding a 1-bit Quantized-JL correction on the residual. Reported results: **~3.5 bits/channel with absolute quality neutrality, ~2.5 bits with marginal degradation** for KV-cache quantization; it also beats product quantization for nearest-neighbor search with near-zero indexing time. You are right and the framing matters: **it is a KV-cache/vector compression method, not a distributed-inference framework** — anyone citing it as a way to "distribute" a model has misread it.

**Where it helps you:** exactly where your design hurts — long-context KV on memory-constrained stages. At ~3.5 bits it cuts the KV numbers above by ~4.5× vs FP16 (32k context: ~10 GB → ~2.3 GB model-wide), which is the difference between a phone/8GB-Mac being able to hold a useful layer range at real context lengths or not. Because it's data-oblivious and online, it fits streaming decode with no calibration pass — a genuine advantage over calibration-dependent schemes on a fleet of unpredictable devices.

**How it composes:** it is orthogonal to (i) **weight quantization** — weights and KV are separate tensors; GGUF Q4 weights + TurboQuant KV stack cleanly (the QVAC integration explicitly targets "any standard transformer loading from a GGUF file"); and (ii) **paged attention** — PagedAttention changes KV *allocation/layout*, TurboQuant changes KV *numeric storage*; a paged block simply stores TQ-encoded vectors (the Wikipedia article makes the same distinction). The inner-product variant matters because attention scores are q·k inner products — you can compute attention against quantized keys with unbiased estimates rather than fully dequantizing.

**Integration status (mid-2026):** not yet merged in mainline llama.cpp, but a working implementation exists and is under review — llama.cpp discussion #20969 documents CPU + CUDA kernels with new GGML cache types (TQ3/TQ4), MSE matching the paper within 1%, ~4.9× compression at 3-bit; a parallel submission targets ik_llama.cpp; a ROCm fork reports **72–78% KV VRAM reduction at <10% throughput overhead with perplexity deltas of +0.010 (4-bit) / +0.051 (3-bit)** on Qwen3-14B; and Tether's QVAC SDK 0.12.0 ships it in production for local AI. So: shipping-adjacent — expect mainline support to be attainable by you (the patches exist) rather than guaranteed upstream.

**Limits:** it compresses KV only — at short contexts weights dominate memory and TurboQuant buys you little; encode/decode adds compute (the Hadamard transform is cheap, but the ROCm numbers show ~5–10% throughput cost, more on weak CPUs — measure on phones); block layout is tied to head-dim (d=128 codebooks); fused flash-attention paths must be taught to consume it (the hard integration work); and 2.5–3-bit settings do measurably degrade retrieval-heavy long-context tasks — keep 3.5–4 bits as default.

**Versus the alternatives:**

| Method | Bits (K/V) | Calibration-free | Mechanism | Notes for you |
|---|---|---|---|---|
| **TurboQuant** | ~2.5–4 | ✅ (data-oblivious, online) | Random rotation + Lloyd-Max (+1-bit QJL for inner products) | Near-optimal distortion guarantees; llama.cpp patches exist; best theory + best portability story |
| KIVI | ~2 | ✅ (tuning-free) | Per-channel asymmetric keys, per-token values; keeps a recent FP16 residual window | Aggressive; residual window complicates paging; quality dips on some models |
| QJL | ~1(keys)+ | ✅ | 1-bit JL transform sketch of keys, zero-overhead score estimation | Same authors' precursor; keys-only; TurboQuant subsumes the idea |
| PolarQuant | ~3–4 | ✅ | Polar-coordinate transform of KV (AISTATS 2026) | Same research line; fewer integrations |
| KVQuant | ~2–3 | ❌ (calibration) | Per-channel keys pre-RoPE, non-uniform, outlier isolation | Great accuracy, but calibration per model conflicts with your "any GGUF" goal |
| llama.cpp q8_0/q4_0 KV | 8 / 4 | ✅ | Blockwise scalar quant | Shipping today, zero risk at q8_0; q4_0 KV quality is model-dependent — TurboQuant at ~3.5b beats q4_0 at similar size |

**Recommended KV policy:** q8_0 KV everywhere as the safe default; TurboQuant TQ4 on memory-tight stages; TQ3 only under pressure and never for the value cache of retrieval-critical workloads.

## 2.5 Fault tolerance: keeping a token stream alive

The failure taxonomy for this fleet: **hard dropout** (phone locked, laptop lid closed, Wi-Fi roam), **stragglers** (thermal throttling — sustained phone SoC performance is commonly ~50–70% of burst within minutes; label: well-established for mobile SoCs, exact ratio device-specific), **transient network loss**, and **memory kills** (iOS jetsam, Android LMK).

The state of the art is Petals' protocol, and you should adopt it wholesale: the **client (or coordinator) retains the per-stage input activations it has sent**; each stage's KV cache is *derived state* reproducible from those inputs; on stage failure, pick a replacement placement covering the lost layer range and **replay the retained inputs to rebuild KV**, then resume the token stream. Cost: memory at the coordinator of hidden × tokens (a 4k-token session at int8 is ~32 MB — cheap), plus a recovery latency of one prefill-of-the-session over the lost layers. Enhancements for your setting: (1) **warm spares** — if pooled memory allows, over-provision so that the most failure-prone device's layer range is *also* resident (cold, mmap-backed) on a neighbor, making failover a pointer swap plus KV replay; (2) **straggler policy** — pipeline parallelism localizes slowness to one stage, so the scheduler demotes a persistently slow stage at the next request boundary (Halda already implements device exclusion); (3) **thermal telemetry** — workers report SoC temperature/throttle state in heartbeats so demotion is predictive, not reactive; (4) **speculative continuation is a research option, not a plan** — skipping a dead stage's layers corrupts the model; there is no sound "approximate bypass" for missing dense layers (for MoE, dropping *experts* degrades more gracefully — a genuine MoE robustness bonus).

## 2.6 Scheduling, orchestration, and the cross-OS control plane

Discovery: **mDNS/DNS-SD on LAN** (works on all five OSes; iOS requires the local-network permission and a Bonjour services declaration) plus a lightweight **rendezvous server for WAN** (or "bring your own Tailscale"). Cluster formation: for a home fleet of ≤10 devices, skip Raft/DHT ceremony — elect the most stable, best-connected node as **coordinator** (deterministic score: uptime × memory × wired-link bonus), with re-election on loss; a DHT (hivemind/libp2p) only pays off at Petals-like public scale. Health: QUIC-ping heartbeats at ~500 ms carrying {queue depth, temperature/throttle flag, memory headroom, battery state}; three misses ⇒ suspected, trigger shadow-plan failover. Per-OS worker reality: Linux/Windows/macOS run a daemon/menu-bar app; Android runs a foreground service (persistent notification) or Termux — expect Doze/background limits to kill anything less; **iOS can only contribute while the app is foreground and the device is charging/screen-on** — schedule it as an ephemeral, optional accelerator, never as a load-bearing stage.

---

# PART 3 — SYSTEM DESIGN: "Hydra" (working name)

*"Foolproof" is not achievable in a fleet whose members thermal-throttle, sleep, and roam; the design goal is graceful degradation with bounded recovery time.*

## 3.1 Architecture overview

```
                        ┌────────────────────────────────────────────┐
                        │              CONTROL PLANE                 │
                        │  Coordinator (elected; runs on best node)  │
                        │  ┌──────────┐ ┌──────────┐ ┌────────────┐ │
                        │  │ Registry │ │ Scheduler│ │ Session Mgr│ │
                        │  │ (members,│ │ (Halda-  │ │ (KV replay │ │
                        │  │  probes) │ │  style)  │ │  buffers)  │ │
                        │  └──────────┘ └──────────┘ └────────────┘ │
                        └───────▲──────────────▲──────────────▲─────┘
                 heartbeats/    │              │              │  placement plans,
                 telemetry      │       QUIC (TLS 1.3)        │  shard manifests
                        ┌───────┴──────┐ ┌─────┴──────┐ ┌─────┴───────┐
                        │  WORKER      │ │  WORKER    │ │  WORKER     │
                        │  desktop     │ │  MacBook   │ │  Android    │
                        │ ┌──────────┐ │ │ ┌────────┐ │ │ ┌─────────┐ │
                        │ │ Engine:  │ │ │ │ Engine │ │ │ │ Engine  │ │
                        │ │ ggml     │ │ │ │ ggml   │ │ │ │ ggml    │ │
                        │ │ (CUDA +  │ │ │ │ (Metal)│ │ │ │ (CPU/   │ │
                        │ │  CPU)    │ │ │ │        │ │ │ │ Vulkan) │ │
                        │ └──────────┘ │ │ └────────┘ │ │ └─────────┘ │
                        │ layers 0–39  │ │ layers     │ │ layers      │
                        │ + KV (q8/TQ4)│ │ 40–71      │ │ 72–79 + head│
                        └──────┬───────┘ └──▲───┬─────┘ └──▲──────────┘
                               │            │   │          │
                               └────────────┘   └──────────┘
                          DATA PLANE: activation ring (QUIC streams,
                          int8 hidden states, 8 KB/token/hop; PRP-style
                          multi-round rings when a device streams from disk)

   Optional islands: [Mac ⇆ Mac over Thunderbolt/RDMA: one logical stage, TP inside]
   Optional edge:    [iPhone: client UI, tokenizer/embeddings, spec-draft model,
                      opportunistic shard while charging + foreground]
```

Components: **Coordinator** (registry + prober, scheduler, session manager with replay buffers, OpenAI-compatible API gateway); **Worker** (one binary/app per OS: engine host around ggml, shard cache on disk, telemetry agent); **Communication layer** (QUIC everywhere, FlatBuffers frames, mDNS + rendezvous discovery); **Model service** (splits a GGUF into per-stage shard files + manifest with hashes, so a phone downloads only its 3 GB, not the 42 GB).

## 3.2 Partitioning & parallelism (the choice and why)

**Pipeline/layer sharding as the backbone; PRP multi-round rings for memory-poor devices; TP only inside fast islands.** Justification is Part 2.1's arithmetic: PP's per-token traffic (~8–16 KB/hop) survives Wi-Fi; TP's 160 collectives/token do not. Heterogeneity handling is native (layers ∝ measured speed); fault tolerance is native (a stage is a replaceable unit). Islands recover TP's latency wins where the physics allow (Thunderbolt Macs, multi-GPU desktop). Single-request latency will not beat the strongest single device that could hold the model — accept this; the cluster's win is *making the model runnable at all*, plus near-linear multi-request throughput scaling (exo's measured 2.2× on 3 devices).

## 3.3 Networking & serialization

QUIC (quinn) with TLS 1.3 and a cluster PSK/cert for auth (fixing the "no security" hole in llama.cpp RPC and exo). One long-lived bidirectional stream per pipeline edge for activations; separate streams for control so a stalled frame can't block heartbeats. Frames: FlatBuffers, {session, token range, layer range, dtype=int8 (blockwise scales), payload}; per-hop decode payload ~8 KB. Prefill: chunked microbatches (e.g., 512 tokens) pipelined across stages so bandwidth and compute overlap; optional zstd on prefill frames only (decode frames are too small to benefit). WAN mode v1 = "works over Tailscale/WireGuard, one WAN edge max"; v2 = built-in ICE/STUN with TURN fallback.

## 3.4 Quantization & KV stack

Weights: **GGUF Q4_K_M default** (IQ4_XS when tight; MXFP4-native for gpt-oss-class MoE), mmap-streamed with PRP prefetch on memory-poor stages. KV: **q8_0 default; TurboQuant TQ4 on constrained stages; TQ3 emergency mode** — integrated at the ggml cache-type level following llama.cpp discussion #20969's TQ3/TQ4 types, with paged (block) KV allocation vLLM-style so long sessions don't fragment. KV never crosses the network; each stage owns KV for its layers, and the coordinator's replay buffer (int8 hidden states per stage boundary, ~8 KB/token) is the recovery source of truth. Speculative decoding: a 1–3B draft model on the *client-side* device (even the iPhone can do this) proposes k tokens; the ring verifies them in one batched pass — this multiplies effective tok/s by ~1.5–2.5× and amortizes per-token ring latency (prima.cpp's 26 tok/s on 32B leaned on exactly this).

## 3.5 Device abstraction layer

Adopt **ggml-backend as the abstraction** rather than inventing one: it already provides a uniform tensor/graph interface over CPU (NEON/AVX), Metal, CUDA, Vulkan, HIP, and SYCL, with buffer types and backend schedulers — the exact "one worker runtime, many accelerators" contract. Per-OS packaging: Linux/Windows = daemon (CUDA/Vulkan/CPU); macOS = menu-bar app (Metal; optional MLX island driver for TB-linked Macs); Android = foreground-service app embedding llama.cpp via JNI (CPU first — big.LITTLE-aware threading — Vulkan where drivers are sane; Termux supported for hackers); iOS = app embedding llama.cpp Metal, worker mode gated on {foreground, charging, thermal-nominal}, with ANE/Core ML explicitly deferred to a research track (conversion friction is high and decode-shard workloads don't clearly win on ANE — estimate/judgment). NPU backends (QNN via ONNX Runtime) are a stretch-phase experiment, not a dependency.

## 3.6 Fault tolerance & dynamic membership

Petals-style **input-replay recovery** (Part 2.5) is the core: coordinator retains per-boundary int8 activations for live sessions; on stage death, apply the precomputed shadow plan, replay to rebuild KV, resume — target recovery: seconds for short sessions, ~prefill-time for long ones. Membership: join ⇒ probe ⇒ scheduler decides at next request boundary whether the newcomer earns layers (Halda-style exclusion means a weak phone may legitimately get zero — surface this honestly in the UI). Leave (graceful) ⇒ drain current token, hand off. Stragglers ⇒ thermal/queue telemetry drives predictive demotion. Redundancy ⇒ optional warm-spare layer ranges on neighbors when pooled memory allows. What is *not* promised: mid-token transparency for hard coordinator loss (v1 restarts in-flight requests on coordinator failover; Raft-grade session HA is out of scope for a home cluster).

## 3.7 Tech stack & build-vs-reuse

| Layer | Choice | Build or reuse | Why |
|---|---|---|---|
| Compute kernels | ggml/llama.cpp (MIT) | **Reuse** | Only engine covering Metal/CUDA/Vulkan/CPU on all 5 OSes; GGUF ecosystem; TurboQuant patches exist against it |
| Pipeline execution | prima.cpp PRP + Halda (MIT) | **Reuse/fork** | Published SOTA for exactly this problem (ICLR 2026); saves you the two hardest algorithms |
| Control plane | New, in **Rust** (tokio + quinn) | **Build** | Nothing reusable is secure + cross-platform + light enough for phones; Rust gives one codebase for daemon/JNI/Swift-bridge |
| Transport | QUIC (quinn), FlatBuffers | Reuse libs | 0-RTT reconnect, no HoL blocking, TLS built in |
| Discovery | mDNS (libmdns) + rendezvous; Tailscale-friendly | Reuse libs | Works on all OSes incl. iOS local-network |
| Scheduler solver | HiGHS or good heuristic port of Halda | Reuse | prima.cpp already uses HiGHS |
| Mac islands | MLX / exo interop (Apache-2.0/MIT) | Optional reuse | TP over TB5 where available |
| KV compression | q8_0 (upstream) + TurboQuant TQ3/TQ4 (port of #20969 patches) | Reuse/port | Working code exists; near-optimal theory |
| Client API | OpenAI-compatible HTTP + SSE | Build (thin) | Ecosystem compatibility (matches exo/Ollama norms) |
| **Avoid** | Ray, Kubernetes, gRPC-everywhere, Petals' PyTorch runtime, Cake (FAIR license: business use requires a commercial agreement) | — | Too heavy for phones / license incompatible with a permissive OSS project |

Everything above is MIT/Apache/BSD. The licensing landmine in the space is Cake's FAIR (source-available, business-restrictive) license — study it as prior art, don't vendor its code. Model weights carry their own licenses (Llama Community License, Gemma terms, etc.) and are a separate compliance check from code.

## 3.8 What breaks and why

| Failure | Effect | Mitigation |
|---|---|---|
| Wi-Fi tail-latency spike | Whole ring stalls a token | QUIC loss recovery; small frames; wired-preferred placement; token deadline + retry |
| Phone thermal throttle | Stage slows 30–50% within minutes | Telemetry-driven predictive demotion; assign phones few/no layers by default |
| iOS jetsam / app background | iPhone stage vanishes instantly | iPhone never load-bearing; charging+foreground gate; warm spare covers its range |
| Device dropout mid-stream | Token stream stops | Shadow plan + input-replay KV rebuild (Petals protocol) |
| Coordinator loss | Cluster headless | Deterministic re-election; sessions restart (documented limitation v1) |
| Long-prompt prefill over Wi-Fi | Minutes of transfer+compute | Chunked pipelined prefill; run heavy stages of prefill on strongest device; prompt caching; spec-decode later |
| Aggregate memory shortfall | OOM or refusal | mmap/PRP streaming (slow but alive); TurboQuant KV; honest admission control ("no valid placement") |
| Mixed-quant numerical drift | Quality degradation | Uniform GGUF quant per model; KV ≥ TQ4 default; perplexity CI tests |
| Untrusted worker reads activations | Prompt-reconstruction attacks are demonstrated | TLS + cluster auth; document that shard hosts can see activations — only pool devices you trust |
| Two requests, one ring | Latency interference | Continuous batching at stage level; per-session QoS |

## 3.9 Phased roadmap

**MVP (≈ 2–3 months, 1–2 engineers):** Linux/CUDA desktop + 2 Macs on wired LAN; fork prima.cpp (or llama.cpp + own pipeline executor); static Halda-style placement computed once; QUIC transport with TLS; OpenAI API endpoint; 70B Q4 target ≥ 2 tok/s decode. *Milestone: 70B answers a chat request end-to-end with any one worker's Wi-Fi glitch not killing the session (replay recovery).*
**v1 (≈ +3–4 months):** Android worker app (JNI, foreground service); dynamic membership at request boundaries; shadow-plan failover; TurboQuant TQ4 KV; shard-manifest distribution; dashboard. *Milestone: pull the Android phone's battery mid-generation; stream resumes < 10 s.*
**v1.5:** MoE first-class (gpt-oss-120B-class), speculative decoding with client-side draft, Mac TB islands (TP inside a stage), prompt caching.
**Stretch:** iOS worker app (charging+foreground opportunistic), built-in NAT traversal, QNN/ANE experiments, multi-request continuous batching across the ring, public-swarm mode (only then consider DHT + incentive design — and read the prompt-reconstruction-attack literature first).

## 3.10 Honest feasibility assessment

**Where this is realistic.** Making a 70B–80B model *runnable* on hardware people already own is proven (prima.cpp: 674 ms/token on four ordinary devices; Petals: ~6 tok/s over the internet on pooled GPUs). Expected performance for your fleet (estimates, anchored to those data points): **70B dense Q4: 1.5–4 tok/s decode over Wi-Fi, 3–8 tok/s with a 4090-class stage on wired LAN; 80–120B MoE (~5B active): 8–25 tok/s; ×1.5–2.5 with speculative decoding.** Time-to-first-token: 5–30 s for 1–4k prompts, minutes for 32k+ prompts — prefill is the honest weak spot. Multi-request throughput scales well (~2× on 3 devices, per exo's measurements).

**Where physics is against you.** (1) Interactive-agent speeds (30+ tok/s single-stream) on a dense 70B over Wi-Fi are out of reach — ring latency plus weakest-stage compute bound you regardless of engineering. (2) TP over wireless is a dead end; don't burn time there. (3) Phones contribute little compute per watt of engineering effort: an iPhone adds ~3 GB of fragile memory and disappears when the screen locks; the honest design uses phones as clients/drafts, with worker mode as a delighter. (4) Availability math is brutal: five devices at 95% individual availability give ~77% full-ring availability — redundancy and fast recovery are the product, not an add-on.

**The simpler-alternative check you asked for.** If the *goal* is "run a 70B at home," a used RTX 3090 pair (~48 GB, ~$1,200–1,500) or a 64–128 GB Mac runs it faster, simpler, and more reliably than any phone-inclusive cluster — and gpt-oss-120B on a single 64 GB Mac likely beats your whole federation on tokens/sec (estimate). The federation is the right project when the constraint is "hardware already owned, $0 budget," when the point is the open-source system itself, or when pooling *many* households (Petals' regime). Build it for those reasons, with the MoE-first, phones-optional, pipeline-parallel shape argued above — that version is feasible, and prima.cpp has already published the proof of concept you can stand on.

---

## Key sources

- exo repo & benchmarks: github.com/exo-explore/exo · blog.exolabs.net/day-1 · third-party review (Medium, Oct 2025)
- prima.cpp: arXiv:2504.08791 · ICLR 2026 poster · ggml-org/llama.cpp discussion #12852
- Petals: arXiv:2209.01188 · petals.dev · NeurIPS'23 "Distributed inference over the Internet"
- llama.cpp RPC in practice: hpcwithus.discoverer.bg/?p=439
- distributed-llama: github.com/b4rtaz/distributed-llama
- Parallax: gradient.network/parallax.pdf · OpenReview 1PhUigVew4
- TurboQuant: arXiv:2504.19874 (ICLR 2026) · llama.cpp discussion #20969 (TQ3/TQ4 implementation) · Pascal-SAPUI5/llama.cpp-turboquant (ROCm benchmarks) · QVAC SDK 0.12.0 release notes · Wikipedia: TurboQuant
- KV alternatives: KIVI arXiv:2402.02750 · QJL arXiv:2406.03482 · PolarQuant arXiv:2502.02617 · KVQuant arXiv:2401.18079 · BalanceKV arXiv:2502.07861
- Privacy of distributed inference: arXiv:2606.18710 (image/prompt reconstruction from intermediate embeddings)
- Server parallelism: vLLM, Megatron-LM, DeepSpeed-Inference, TGI, TensorRT-LLM official repos/docs

---

# ADDENDUM (July 2026): Corrections and design revisions from external review

An external technical review of this report was received and independently verified where factual. Accepted changes, in priority order:

**Factual corrections.** (1) Cake's status and license were wrong in v1 of this report and are now corrected above (active development; FAIR v1.0.0 source-available license, not GPL; business use requires a commercial agreement). (2) The "~32 MB replay buffer for a 4k session" figure counted **one** stage boundary; a P-stage pipeline retains P−1 boundary streams (≈96 MiB at 4k tokens, ≈3 GiB at 128k for four stages, before scales/metadata/multi-session/speculative copies). Recovery state is a first-class capacity-planning item, not a rounding error.

**Design revisions accepted into the architecture.**
- **Recovery store becomes three-tier:** RAM rolling window (recent 512–2k tokens) → encrypted, checksummed SSD boundary log (async append) → optional periodic KV snapshots for long sessions. Failover-time promises are conditional on context length, spare readiness, and disk speed. A warm *weight* spare is not a warm *session* spare — KV rebuild is O(context) regardless; expose Basic / Recoverable / High-Availability session modes, with HA (async-mirrored KV) opt-in.
- **Exactly-once execution semantics:** QUIC does not provide application-level idempotency. Every activation frame carries {model-manifest hash, session id + epoch, placement version, stage generation, branch id, absolute token position, microbatch range, attempt id, checksum}; stages key KV writes to absolute positions, keep computed/committed watermarks, and dedupe retries. Retransmission may never cause a second KV append. Stale placement versions and stage generations are rejected.
- **Speculative decoding is transactional,** not a free multiplier: provisional KV for the drafted positions, commit on accepted-prefix length, rollback of the remainder, and failure handling mid-commit; draft length k adapts to measured acceptance rate. The 1.5–2.5× figure is a benchmark target, not an architectural assumption.
- **MoE-first is downgraded to MoE-conditional:** the compute argument holds only when every expert a stage's layers might route to stays memory-resident. Input-dependent expert selection turns mmap streaming into effectively random SSD I/O, which can make an MoE slower than a smaller dense model. Decision rule: dense when streaming is the fit strategy; MoE when experts are resident (or inside a fast island); never paged experts on a phone.
- **TurboQuant moves out of v1:** prototype quality is real, but the early Metal path in the llama.cpp integration ran ~3.3–8× slower than q8 cache ops due to unfused transform/attention kernels. Sequence: q8 KV (MVP) → llama.cpp q4/q8 cache with quality gates (v1) → TurboQuant behind a feature flag (v1.5) → default only after fused kernels pass perf/quality thresholds on all four backends.
- **Transport becomes an interface, not a decision:** QUIC's stream independence does not remove in-stream head-of-line blocking, and its userspace crypto costs battery on phones while TCP enjoys kernel offload. Ship TCP+mTLS and QUIC behind one interface, select per link on measured p50/p95/p99 frame latency and CPU cost. Drop zstd-by-default on activations (quantized activations are near-high-entropy); compress only after a cheap compressibility probe.
- **Paged attention is out of v1:** block tables, indirect-access kernels on four backends, copy-on-write, and branch handling constitute a major runtime project. v1 uses contiguous/segmented per-session KV with reserved capacity and admission control.
- **Security model is made explicit:** v1's trust boundary is *one trusted household; Byzantine workers are out of scope*. Even so: per-device Ed25519 identity + mutual TLS + QR/PIN pairing + revocation (a single shared PSK lets one compromised phone impersonate the fleet), signed model manifests with per-tensor hashes, hard frame/tensor size limits with validation before allocation, encrypted replay logs, and authenticated/rate-limited API. Workers legitimately see plaintext activations — prompt-reconstruction attacks are demonstrated — so untrusted-swarm mode is a separate future system, not an extension.
- **Coordinator: fixed for MVP;** later failover requires leases, monotonic leader epochs, and fencing tokens on every worker command (deterministic re-election alone permits split-brain under partition). Only stable desktop-class nodes may ever be voting members.
- **Shard distribution becomes a content-addressed tensor cache** (per-tensor hashes, immutable chunks, placement manifests referencing hashes) instead of per-stage GGUF files — deduplicates tied embeddings/shared experts and makes rebalancing a delta download.
- **Scheduler objective refined:** minimize the *serial* per-token sum of p95 stage-compute and link times (pipeline decode latency is a sum, not a max — the cluster fits the model and scales multi-request throughput; it does not speed one stream), with penalty terms for failure risk, page-fault risk, thermal trend, energy policy, and recovery cost; 15–30% memory headroom reserved. Device benchmarking uses 30–120 s sustained runs with EWMA updates, not 10 s bursts (phones lie under burst).
- **Roadmap re-baselined:** Phase 0 benchmark harness (4–6 wks) → trusted-LAN static MVP (+2–4 mo) → recoverable sessions v1 (+3–6 mo) → performance extensions (speculative, MoE tracks, TB islands, experimental TurboQuant); iOS worker, public swarms, Byzantine validation, true paged attention move to a research track. Revised decode envelopes: mixed Wi-Fi 1.5–4 tok/s (dense 70B Q4), wired desktop+Macs 2–7 tok/s, phone-in-critical-path 0.5–2 tok/s; MoE claims are model-and-residency-specific, not generic.

**Reviewer claims verified or accepted on evidence:** Cake repo state (fetched directly), prima.cpp testbed conditions (320–610 Mbps, 3–7 ms links; GPU-can-hurt finding), Petals O(context) replay recovery and its own trusted-network caveat, and the pipeline-decode-latency-is-a-sum argument (consistent with exo's measured single-request slowdown when adding devices).

---

# ADDENDUM 2 (July 2026): Second stress test — gaps remaining after the first review

Scope: issues *not* covered (or only glancingly covered) by the report v1 or the first review's revisions. Ordered by how likely each is to bite in practice.

## A. Wi-Fi physics the design still models wrong

**A1. Shared airtime, not per-link bandwidth.** The placement model treats links as independent pipes. On a typical home network, every peer-to-peer hop traverses the AP: one activation frame = **two radio transmissions** (device→AP, AP→device), and *all* hops in the ring contend for the same airtime, along with recovery copies, shard downloads, and the household's Netflix stream. A 4-stage ring over one AP does not have 3 independent links; it has one collision domain carrying ~6 transmissions per frame cycle. Consequences: prefill bandwidth is roughly halved versus the naive model, and concurrent transfers interfere superlinearly. **Fix:** the scheduler must score *aggregate airtime* per placement, not per-edge bandwidth; prefer wired/direct paths structurally; cap concurrent bulk transfers cluster-wide.

**A2. Recovery traffic competes with the workload it protects.** Petals-style async boundary copies to the coordinator are the same size as the forward activations — on shared airtime, recovery logging can *double* prefill's radio cost precisely when the network is busiest. **Fix:** an explicit recovery-traffic budget; log at the sending stage's local disk with lazy upload; degrade logging fidelity (checkpoint intervals) under congestion rather than degrading inference.

**A3. AP client isolation and mesh/VLAN quirks silently break peer-to-peer.** Guest SSIDs and many mesh systems block client-to-client traffic and drop mDNS across nodes. Symptom: devices "discover" via the rendezvous but the data plane can't connect, or vice versa. **Fix:** a startup full-mesh connectivity matrix probe (every pair, both directions, both planes) with human-readable diagnosis; never infer B↔C reachability from A↔B and A↔C.

**A4. Small stuff that causes weird bugs:** PMTU mismatches on mixed jumbo/1500 wired+wireless paths; IPv6 link-local addresses with zone IDs breaking naive address handling; dual-stack happy-eyeballs between peers choosing different families in each direction (breaks the "directed link measurement" assumption).

## B. The model-architecture drift risk (the KV subsystem is over-fit to 2024-era GQA)

**B1. MLA changes everything.** DeepSeek-class multi-head latent attention stores a compressed latent instead of full K/V — roughly an order of magnitude smaller KV natively. If the target model is MLA-based, the entire KV-compression stack (TurboQuant included, whose codebooks assume head-dim-128 blocks) becomes low-priority, and the memory calculus that motivated it shifts back to weights. **Fix:** the KV subsystem must be a per-architecture strategy interface, not a hardcoded GQA layout.

**B2. Hybrid SSM/attention models** (Jamba/Granite-class Mamba layers) carry O(1) recurrent state instead of per-token KV. Replay-based recovery still works (state is a deterministic function of boundary inputs) but checkpointing semantics, state size accounting, and the per-layer cost model all differ. **B3. Interleaved sliding-window/global attention** (Gemma/Mistral-style) means per-layer KV sizes differ within one model — placement and KV budgeting must be per-layer, and SWA eviction policy must be reconciled with the replay log's assumptions.

**B4. Activation-quantization error compounds per hop.** Petals validated int8 hidden states at its stage counts; a 6–8-stage ring performs 5–7 quantize/dequantize cycles on the residual stream. Nobody has published quality data for that regime. **Fix:** quality-gate wire precision as a function of stage count; allow per-boundary FP16 fallback; include a perplexity-vs-stage-count test in CI.

## C. The sampler/tokenizer plane — an undesigned component that owns correctness

**C1. Streaming detokenization:** BPE byte-fallback tokens can split UTF-8 codepoints; the coordinator must buffer until valid boundaries and handle token-healing. Trivial until it corrupts a user's CJK/emoji output mid-stream.

**C2. Who owns sampling state, and what crosses the wire:** repetition/frequency penalties need the full emitted-token history at the sampler; grammar-constrained decoding needs per-step vocab masks (~16 KB/token at 128k vocab) or the grammar automaton pushed to the final stage; `logprobs` naively means shipping full logits (~256 KB/token FP16) to the coordinator. **Fix:** final stage is the single sampling owner; coordinator pushes grammar automata and sampling config at session start; top-k logprobs computed at the final stage. This is a real component with real state — it belongs in the architecture diagram.

**C3. Recovery must teacher-force, never re-sample.** After failover, replay through a replacement backend can produce *different logits*; already-emitted tokens are canon. The recovery log's source of truth is the committed token-ID sequence plus the RNG stream position, and replay forces those tokens regardless of what the replacement stage would have sampled. (v1 implied this; it needs to be an explicit invariant.)

## D. Storage, lifecycle, and supply chain

**D1. Model files are untrusted input.** GGUF parsing has had real CVEs in llama.cpp (2024 heap-overflow class). A cluster that auto-downloads community quantizations and streams shards to every device multiplies the blast radius. **Fix:** hash-pinned manifests from a trusted source, hardened/fuzzed parser path, ideally a sandboxed loader.

**D2. Replay-log key management.** "Encrypted SSD logs" needs an owner: per-session keys held by the coordinator, sealed to device keystores (Secure Enclave / TPM / StrongBox) at rest — otherwise a stolen laptop yields activation logs from which prompts are reconstructable (the attack literature already covers this).

**D3. Log/snapshot format versioning:** a worker upgrade must not orphan recovery state — version headers plus migrate-or-invalidate-with-warning, tested in CI. **D4. Flash wear:** append-heavy logs and KV snapshots on phone UFS storage need write budgets and atomic segment rotation (plus the disk-full-mid-append case). **D5. Models on NAS:** users *will* point the model path at SMB/NFS; mmap page faults over the network are pathological — detect and refuse or copy-local. **D6. Idle and cold start:** pinning 40+ GB across family machines 24/7 is antisocial; define idle eviction and an honest cold-start TTFT (tens of seconds to minutes to re-page weights), with exo-style instance recycling semantics in the API. **D7. Boot storm:** N devices fetching shards simultaneously saturate the AP; stagger, and let the content-addressed cache fill torrent-style from LAN peers rather than the WAN.

## E. Control-plane and operational gaps

**E1. DNS rebinding / CSRF against the LAN API.** Local inference servers (Ollama-class) have been exploited via browsers on the same LAN hitting unauthenticated localhost/0.0.0.0 HTTP APIs. The coordinator's OpenAI endpoint and dashboard need auth *even on LAN*, Host/Origin validation, and must not bind 0.0.0.0 by default.

**E2. Cascading demotion.** Thermal demotion of one stage shifts layers to survivors → they heat → they demote → oscillation or collapse. Rebalancing needs global damping: minimum interval between plan changes, hysteresis bands, and a defined load-shed ladder (reduce concurrency → refuse new sessions → suggest smaller model) instead of unbounded re-planning.

**E3. Cross-domain misattribution.** KV growth → OS memory pressure → mmap weight eviction → page faults → the stage looks *network-slow*. Telemetry must carry causal signals (page-fault rate, throttle state, queue depth, radio retry rate) so the scheduler blames the right resource; otherwise it will "fix" a memory problem by rerouting the network.

**E4. Rolling upgrades and retry storms.** Mixed protocol versions mid-cluster (negotiate + drain-before-upgrade); clients that see 30 s of silence and retry, creating duplicate sessions (API-level idempotency keys). **E5. Recovery under load:** replay competes with live decode on the same stages; recovery needs admission control and a fairness policy, or one failure degrades every session at once.

## F. Platform realities not yet priced in

**F1. iOS:** mDNS browsing requires the multicast entitlement (granted by request), plus the local-network permission prompt; a "worker" app with no user-visible foreground purpose is an App Store review risk — design the iOS app UI around a legitimate foreground role (chat client that *also* contributes while charging). **F2. Android:** 14+ restricts foreground-service *types* and Play policy scrutinizes them; OEM task killers (Samsung/Xiaomi) ignore foreground semantics anyway; Termux is sideload-only; Vulkan driver quality is a lottery — CPU must be the default backend. **F3. macOS/Windows:** App Nap and lid-close need explicit power assertions (`caffeinate`-equivalent) and Modern Standby handling, or "laptop stage" means "stage that dies at 30% battery savings settings." **F4. Energy honesty:** measure **joules/token** as a first-class metric. A five-device fleet drawing 150–400 W to produce 2 tok/s has a real electricity and battery-wear cost that should appear in the feasibility section next to the cloud-API price comparison (estimate; measure in Phase 0).

## G. Verification methodology gap

The chaos suite tests the implementation; nothing yet *proves the protocol*. Add **deterministic simulation testing** (FoundationDB-style): the session state machine, epochs, fencing, idempotent KV commits, and speculative rollback run against a discrete-event simulator injecting duplication, reordering, delay, partitions, and crash-restarts at every await point — before any real network is involved. Complement with golden-token cross-backend CI vectors and per-stage activation fingerprinting (hash of a fixed random projection of the boundary activation) so numeric divergence can be localized to a stage in production.

## H. Combined-failure game days (single faults are the easy case)

Run these as scripted scenarios with pass/fail invariants (never silent corruption; always a terminal state):
1. **Thursday night:** 4K streaming + game download + phone joining/leaving during a 16k prefill — p99 must degrade, correctness must not.
2. Stage failure **during** a 32k prefill while a second session decodes (recovery-vs-live contention).
3. AP reboot in the middle of a speculative commit (provisional KV on three stages, accepted-length message lost).
4. Disk-full during log append followed immediately by worker crash (recoverable state must not be half-written).
5. Model upgrade initiated while a session is paused on a tool call (KV retention vs. manifest epoch).

## Net changes to the design

Score **airtime**, not links; budget **recovery traffic** explicitly; make the KV subsystem **per-architecture** (MLA/SSM/SWA-aware) and gate wire precision by stage count; promote the **sampler/tokenizer plane** to a named component with teacher-forced recovery as an invariant; treat model files and the LAN API as **attack surfaces** (parser hardening, rebinding defenses); add **damped** rebalancing with a load-shed ladder; price **platform entitlements and energy** into the roadmap; and put **deterministic simulation testing** in Phase 0, not v1. None of these change the core architecture — pipeline sharding, trusted-LAN boundary, fixed coordinator, replay recovery all survive — but several (A1, B1, C2, E1) would have produced exactly the "strong architecture that fails unpredictably" outcome the first review warned about.

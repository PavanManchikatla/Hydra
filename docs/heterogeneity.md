# Heterogeneity data point — M3 Phase 1 (P1·2)

> **What this is.** The first **real-hardware** capability measurement across Hydra's 3-node
> heterogeneity set, banked while the cloud VMs are alive (they die 2026-08-05). It is the first
> fixture for the M3 startup benchmark (P2·1) and the input the placement solver (P2·3) is validated
> against. Produced by `cargo run --bin hydra-bench` **on each node locally** (no networking).
>
> **Honesty.** CPU backend (`n_gpu_layers=0`, the deterministic DoD backend) on all three nodes for a
> fair comparison. These are **compute-capability** numbers only — **link costs are not included**
> (the Mac↔VM legs are WAN/Tailscale, the VM↔VM leg is cloud-VNet; link probing is P2·2). Small-model
> (0.5B) numbers; not a tuned throughput target. The **ratio**, not the absolute value, drives placement.

## The 3-node set

| Node | Machine | Arch | RAM | Tailscale IP |
|---|---|---|---|---|
| Mac | pavans-macbook-air (M2) | arm64 | 8 GB | 100.93.110.78 |
| myVm-1 | Azure B2ms | x86-64 | 8 GB | 100.115.200.62 |
| myVm-2 | Azure B2als_v2 | x86-64 | 4 GiB | 100.73.205.31 |

## Measured capability (Qwen2.5-0.5B fp16, CPU backend, 2026-07-19)

Single-token autoregressive **decode** (the TPOT-dominating path) through the full 24-layer model,
each number the stable value across two runs:

| Node | decode tok/s | ms / token | **ms / layer-token** | model load | capability ratio |
|---|---|---|---|---|---|
| Mac (M2, arm64) | **41.5** | 24.1 | **1.00** | 1108 ms | **4.0×** |
| myVm-2 (B2als_v2, 4 GiB) | **22.0** | 45.4 | **1.89** | ~300–600 ms | **2.1×** |
| myVm-1 (B2ms, 8 GB) | **10.4** | 96.4 | **4.02** | ~350 ms | **1.0×** |

### Finding: capability does not track RAM or vCPU count

Both VMs are 2-vCPU Azure burstable instances, yet **myVm-2 (4 GiB) is ~2× faster than myVm-1
(8 GB)** at decode — the newer AMD-based `B2als_v2` generation beats the older `B2ms` despite half the
RAM. The result is **stable across re-runs** (not burstable-credit throttling: 10.37/10.37 tok/s on
myVm-1, 22.0/21.7 on myVm-2). The Apple M2 is fastest at ~4× the slowest node. **Takeaway for the
solver: capability must be *measured*, never inferred from a spec sheet** — exactly why P2·1 exists.

## Placement decision (this asymmetric set)

Pipeline throughput is bounded by the **slowest stage**, so contiguous layer ranges should be sized so
each stage takes ~equal wall-time, i.e. **layers per node ∝ its decode tok/s**. With capability
weights Mac : myVm-2 : myVm-1 = 41.5 : 22.0 : 10.4 (sum 73.9):

| Node | share of layers | 24-layer (0.5B) split |
|---|---|---|
| Mac | 56 % | `[0, 14)` |
| myVm-2 | 30 % | `[14, 21)` |
| myVm-1 | 14 % | `[21, 24)` |

### This set empirically validates capability-weighted placement

The measured asymmetry is large enough that **a naïve uniform split is badly wasteful** — the concrete
justification for weighting by measured capability (and for P2·1/P2·3 existing at all):

| Split | Mac stage | myVm-2 stage | myVm-1 stage | pipeline TPOT (bottleneck) |
|---|---|---|---|---|
| **uniform** 8/8/8 | 8.0 ms/tok | 15.1 ms/tok | **32.1 ms/tok** | ~32 ms/tok |
| **capability-weighted** 14/7/3 | 14.1 ms/tok | 13.3 ms/tok | 12.1 ms/tok | ~14 ms/tok |

(stage time = layers × ms/layer-token). Pipeline throughput is set by the **slowest stage**: the
uniform split lets the slow myVm-1 stage bottleneck the whole pipeline at ~32 ms/token, whereas the
capability-weighted split balances every stage to ~13–14 ms/token — a **~2.3× throughput loss
avoided** on this set (and the raw per-node capability spread is **4×**, Mac vs myVm-1). Uniform
placement would waste that 2–4× on the slowest stage; measured-capability weighting recovers it.

Caveats the solver (P2·3) must fold in and this compute-only number does **not** capture:
- **Link cost.** Mac↔VM is WAN/Tailscale (~high latency); myVm-1↔myVm-2 is cloud-VNet (sub-ms). A
  layer split should also minimize the number/size of boundary crossings on slow links — favor
  keeping adjacent stages on the fast VNet leg where possible (P2·2 link prober feeds this).
- **Memory.** The current engine loads **full weights per worker**, so RAM caps model size, not layer
  count, for now; once weights are sharded (P2·10) the 4 GiB node's layer share becomes RAM-bounded
  for large models. For the 0.5B here, RAM is not the binding constraint.
- **Backend.** The Mac number is CPU-only; on Metal it would be faster, changing its share. Measure in
  the backend the deployment will actually use.

## Reproduce

```bash
# on each node (Mac locally; VMs over SSH), with the llama.cpp build tree on the library path:
HYDRA_NODE=<label> LD_LIBRARY_PATH=~/hydra/vendor/llama.cpp/build/bin cargo run --bin hydra-bench
```

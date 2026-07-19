# WAN run — real-second-machine + WAN data point (M2 DoD)

> **What this is.** The amended M2 DoD (BLUEPRINT §3, owner-ratified 2026-07-12) splits the
> real-second-machine requirement into a **cloud VM over Tailscale**: run the correctness suite
> across two real machines separated by a real WAN, plus a performance data point, annotated
> **WAN/Tailscale** and **never compared against wired-LAN targets** (the honesty rule, §8).
> This file doubles as the **M4 deployment-story dry run** — every provisioning step is recorded
> so a non-author can reproduce it.

> **Honesty banner (binding).** Every number below is a **WAN/Tailscale** measurement on
> asymmetric hardware (arm64 Mac ↔ x86-64 cloud VM). It is **not** a wired-LAN number and must
> never be read as one. Wired-LAN perf stays owed at the M3 gate (§8).

## Topology

```
   Mac (arm64, coordinator + S1 = layers [0,k))          Azure VM (x86-64, S_P = layers [k,-1) + sampler)
   100.93.110.78  ────────────── Tailscale (WAN) ──────────────  100.115.200.62
```

- **Coordinator + S1** on the Mac (`pavans-macbook-air`, arm64, Metal/CPU).
- **S_P** (final stage + sampler) on the VM (`myVm-1`, Ubuntu 24.04, x86-64, CPU).
- Wire: real TCP + mTLS over the Tailscale tailnet — the same handshake/framing/code path as a
  LAN, only the transport is a WAN.

## Machine facts

| | Mac | Azure VM |
|---|---|---|
| Name | pavans-macbook-air | myVm-1 (`myvm-1` MagicDNS) |
| Arch | arm64 (Apple Silicon) | x86-64 |
| OS | macOS | Ubuntu 24.04 LTS |
| CPU / RAM | M2, 8 GB | B2ms, 2 vCPU / 7.9 GB |
| Tailscale IP | 100.93.110.78 | 100.115.200.62 |
| Access | — | `ssh hydra-vm` (alias) / `ssh -i ~/.ssh/myVm-1_key.pem azureuser@100.115.200.62` |

> **⚠️ Cross-arch equivalence tier.** arm64 ↔ x86-64 boundary tensors are **not** bit-exact
> (different FP rounding / kernel order) — spec I8 documented semantic-continuity behavior. So the
> WAN correctness run applies the **mixed-backend tier** (top-k(10) overlap ≥ 9/10 per step,
> deterministic replay reproduces the same tokens on the same placement), **not** the bit-exact
> anchor (which is arm64-only / same-arch). This is stated explicitly wherever a result is reported.

## Provisioning (the deployment dry run)

All commands run from the Mac via `ssh hydra-vm` unless noted. Mind the 8 GB during builds
(`-j2`).

### 1. OS build dependencies
```bash
sudo apt-get update
sudo apt-get install -y git cmake build-essential pkg-config libssl-dev curl
```
Verified: git 2.43.0, cmake 3.28.3, gcc 13.3.0; 26 GB free on `/`.

### 2. Rust toolchain
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```
Verified: rustc / cargo 1.97.1.

### 3. Clone the repo + pinned submodule
```bash
git clone https://github.com/PavanManchikatla/Hydra.git ~/hydra
cd ~/hydra && git submodule update --init          # vendor/llama.cpp @ 13f2b28b
```

### 4. Apply the M−1 layer-window patch + build llama.cpp (CPU, shared libs)
The `hydra-engine-sys` FFI links the **vendored build-tree** dylibs at
`vendor/llama.cpp/build/bin`. The pinned submodule is upstream llama.cpp; the ~47-line per-arch
layer-window patch is applied locally (it is not upstream), then the tree is built shared.
```bash
git -C vendor/llama.cpp apply ~/hydra/spike/llama-cpp-layer-window.patch
cmake -B vendor/llama.cpp/build -S vendor/llama.cpp \
      -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
      -DGGML_CUDA=OFF -DGGML_METAL=OFF -DLLAMA_CURL=OFF \
      -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF
cmake --build vendor/llama.cpp/build -j2          # produces libllama.so, libggml{,-base,-cpu}.so
```

### 5. Dev model
The Qwen2.5-0.5B fp16 GGUF is git-ignored; transferred from the Mac over Tailscale:
```bash
scp ~/Documents/hydra/models/qwen2.5-0.5b-instruct-fp16.gguf hydra-vm:~/hydra/models/
```

### 6. Build the workspace
```bash
cd ~/hydra && source ~/.cargo/env
cargo build --workspace                            # engine-sys links the build-tree .so's
```

## Running the suite

The Mac-side runner `hydra-wan` (`crates/hydra-worker/src/bin/hydra-wan.rs`) provisions the remote
S_P over SSH (bound to the Tailscale IP **only**, never 0.0.0.0), runs both phases, and stops it:

```bash
cargo run --bin hydra-wan            # Mac; connects to the VM over Tailscale
```

## Results — 2026-07-13 (Qwen2.5-0.5B fp16, greedy)

> Cross-arch (arm64 ↔ x86-64). Bit-exactness is **not** expected across architectures (spec I8);
> the **mixed-backend tier** applies. All numbers are **WAN/Tailscale** and are **not** wired-LAN
> numbers.

### Phase 1 — cross-machine correctness + perf (S1 on Mac `[0,12)` ↔ S_P on VM `[12,24)`)
- **Greedy argmax agreement: 12/12 steps** vs the Mac's unsplit greedy reference — the boundary
  residual crossed arm64→x86-64 over the WAN and every argmax still matched. This is the
  mixed-backend tier's "deterministic replay reproduces the same tokens" clause holding (the
  ~0.04-logit FP16 boundary drift leaves the argmax stable, consistent with the M−1 spike). **Not**
  a bit-exact claim (which is same-arch only).
- **Deterministic replay (fresh workers, same placement): IDENTICAL.**
- **Perf: 12 tokens in 10.73 s → ~1.12 tok/s** over Tailscale. Latency-bound (per-token S1→S_P WAN
  round-trip on a 0.5 B model); a small-model WAN datapoint, **not** a throughput target.

### Phase 2 — WAN kill-window (real machine death), full-range D0-class S_P on the VM
- Generated 6 tokens, then **`kill -9` the VM S_P over the WAN**; brought up a replacement and drove
  recovery **through the real machinery** (BEGIN_RECOVERY Case A → catch-up `REBUILD_APPLY` of the
  durable tokens → INSTALL_SAMPLER_CHECKPOINT → activation → SAMPLE_NEXT@goal+1), reconstructing the
  inputs from the driver-held ledger.
- **Recovered stream (pre-kill ⊕ resumed) byte-identical to the uninterrupted VM run** — both S_P
  generations run on the VM (x86-64), so the recovery is same-arch and exact.
- **detection→resumed: 25.4 s** over the WAN. Dominated by the replacement worker's cold model load
  on the 2-vCPU VM + the catch-up replay round-trips over Tailscale. **Honesty:** this is a WAN,
  cold-replacement number on a small dev VM — **not** the <15 s LAN/M3 D1 target (which assumes a
  warm survivor-preserving Strategy A on real hardware).
- The split-stage (S1↔S_P) recovery — where S_P's KV is rebuilt from durably-copied **boundaries**
  with S1 survivor-preserved — is the multi-node D1 flow welded to the worker→worker `FWD` slice,
  out of this run's scope; here the victim is a full-range S_P recovered by token replay (the
  C-part-2 machinery).

### Security posture
- All Hydra services bound to the **Tailscale IP `100.115.200.62` only** (never 0.0.0.0), per the
  standing v1 security boundary.
- ⚠️ **Owner action:** the Azure NSG still allows **public port 22** as a provisioning fallback.
  Now that the WAN run has succeeded, **close it** (leave only the Tailscale path).

### What this closes / what stays owed
- **Closes** the M2 DoD **real-second-machine + WAN data point** component (correctness across two
  real machines over a real WAN; a perf datapoint; real machine-death recovery over the WAN).
- **Still owed (M3 gate):** the **wired-LAN performance envelope** — no wired-LAN number is implied
  by any figure above (honesty rule, §8).

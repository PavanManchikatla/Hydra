// shard_split — M-1 feasibility demonstration.
//
// Proves shard-style execution of a GGUF over the vendored (patched) llama.cpp:
//   Shard A = layers [0, k)  : tokens -> boundary residual (raw, entering layer k)
//   Shard B = layers [k, N)  : injected residual -> final norm + lm_head -> logits
// and checks the result against the UNSPLIT model on the same CPU backend.
//
// Exercises the M-1 checklist (BLUEPRINT §3):
//   (a) load/run an arbitrary contiguous layer range only   -> il_start/il_end window
//   (b) inject boundary activations + extract them          -> batch.embd / get_embeddings
//   (c) KV allocated per assigned layers                    -> each shard's ctx has its own KV
//   (d) truncate KV at an input position and replay         -> Check D
//   (e) run final range to logits WITHOUT sampling          -> raw get_logits_ith
//   (f) round-trip FP16 boundary tensors                    -> Check B (fp16) vs Check C (f32)
//
// DoD: split A->B logits match unsplit within 1e-3 max-abs; KV truncate+replay reproduces them.

#include "llama.h"
#include "ggml.h"
#include "ggml-backend.h"

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cmath>
#include <string>
#include <vector>
#include <algorithm>

// Exported by libllama (C++-mangled; not in the public header). Used only as an
// independent cross-check that Shard A reproduces the unsplit layer-k input residual.
float * llama_get_embeddings_layer_inp(struct llama_context * ctx, uint32_t lid);
void    llama_set_embeddings_layer_inp(struct llama_context * ctx, uint32_t lid, bool value);

static void die(const char* m) { fprintf(stderr, "shard_split: %s\n", m); exit(1); }

// eval-callback capture of one named graph node (e.g. "l_out-11") for all positions
struct CBData { std::string target; std::vector<float> buf; bool got = false; };
static bool capture_cb(struct ggml_tensor* t, bool ask, void* ud) {
    CBData* d = (CBData*)ud;
    if (ask) return true;                        // observe all nodes (mirrors eval-callback.cpp)
    if (t->buffer == nullptr) return true;       // unmaterialized (reserve/warmup)
    if (strcmp(t->name, d->target.c_str()) != 0) return true;
    size_t nbytes = ggml_nbytes(t);
    if (ggml_backend_buffer_is_host(t->buffer)) {
        d->buf.assign((const float*)t->data, (const float*)t->data + nbytes/sizeof(float));
    } else {
        d->buf.resize(nbytes / sizeof(float));
        ggml_backend_tensor_get(t, d->buf.data(), 0, nbytes);
    }
    d->got = true;
    return true;
}

struct DiffStat { double max_abs; double mean_abs; int argmax_ref; int argmax_got; int topk_overlap; };

static DiffStat compare(const std::vector<float>& a, const std::vector<float>& b, int topk = 10) {
    DiffStat d{}; d.max_abs = 0; d.mean_abs = 0;
    const size_t n = a.size();
    for (size_t i = 0; i < n; i++) {
        double e = std::fabs((double)a[i] - (double)b[i]);
        d.max_abs = std::max(d.max_abs, e);
        d.mean_abs += e;
    }
    d.mean_abs /= (double)n;
    auto argmax = [](const std::vector<float>& v){ return (int)(std::max_element(v.begin(), v.end()) - v.begin()); };
    d.argmax_ref = argmax(a); d.argmax_got = argmax(b);
    auto topset = [&](const std::vector<float>& v){
        std::vector<int> idx(v.size()); for (size_t i=0;i<v.size();i++) idx[i]=(int)i;
        std::partial_sort(idx.begin(), idx.begin()+topk, idx.end(), [&](int x,int y){return v[x]>v[y];});
        return std::vector<int>(idx.begin(), idx.begin()+topk);
    };
    auto ta = topset(a), tb = topset(b);
    std::sort(ta.begin(),ta.end()); std::sort(tb.begin(),tb.end());
    std::vector<int> inter; std::set_intersection(ta.begin(),ta.end(),tb.begin(),tb.end(),std::back_inserter(inter));
    d.topk_overlap = (int)inter.size();
    return d;
}

int main(int argc, char** argv) {
    std::string model_path, prompt = "The capital of France is";
    int k = -1; // split layer (default n_layer/2)
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-m") && i+1<argc) model_path = argv[++i];
        else if (!strcmp(argv[i], "-p") && i+1<argc) prompt = argv[++i];
        else if (!strcmp(argv[i], "-k") && i+1<argc) k = atoi(argv[++i]);
    }
    if (model_path.empty()) die("usage: shard_split -m model.gguf [-p prompt] [-k split_layer]");

    ggml_backend_load_all();

    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = 0;                       // CPU backend — the DoD comparison backend
    llama_model* model = llama_model_load_from_file(model_path.c_str(), mp);
    if (!model) die("failed to load model");

    const int n_layer = llama_model_n_layer(model);
    const int n_embd  = llama_model_n_embd(model);
    const llama_vocab* vocab = llama_model_get_vocab(model);
    const int n_vocab = llama_vocab_n_tokens(vocab);
    if (k <= 0 || k >= n_layer) k = n_layer / 2;

    // tokenize
    int n = -llama_tokenize(vocab, prompt.c_str(), (int)prompt.size(), nullptr, 0, true, true);
    std::vector<llama_token> toks(n);
    if (llama_tokenize(vocab, prompt.c_str(), (int)prompt.size(), toks.data(), n, true, true) < 0) die("tokenize failed");

    printf("model: n_layer=%d n_embd=%d n_vocab=%d | split k=%d  (A=[0,%d) B=[%d,%d))\n",
           n_layer, n_embd, n_vocab, k, k, k, n_layer);
    printf("prompt: \"%s\"  (%d tokens)\n", prompt.c_str(), n);

    auto make_ctx = [&](int il_start, int il_end, bool embeddings) {
        llama_context_params cp = llama_context_default_params();
        cp.n_ctx = n + 8; cp.n_batch = n; cp.n_ubatch = n; cp.no_perf = true;
        cp.il_start = il_start; cp.il_end = il_end;
        cp.embeddings = embeddings;
        if (embeddings) cp.pooling_type = LLAMA_POOLING_TYPE_NONE; // per-token residual, no pooling
        return llama_init_from_model(model, cp);
    };

    // ---- 0) Reference: unsplit full model -> logits at last position ----
    // Also capture the TRUE residual entering layer k ("l_out-{k-1}") via eval callback.
    std::vector<float> ref(n_vocab);
    std::vector<float> true_boundary;
    CBData cbd; cbd.target = "l_out-" + std::to_string(k-1);
    {
        llama_context_params cp = llama_context_default_params();
        cp.n_ctx = n + 8; cp.n_batch = n; cp.n_ubatch = n; cp.no_perf = true;
        cp.cb_eval = capture_cb; cp.cb_eval_user_data = &cbd; // capture true l_out-{k-1}
        llama_context* ctx = llama_init_from_model(model, cp);
        if (!ctx) die("ctx_full");
        llama_batch b = llama_batch_init(n, 0, 1);
        b.n_tokens = n;
        for (int i = 0; i < n; i++) {
            b.token[i] = toks[i]; b.pos[i] = i; b.n_seq_id[i] = 1; b.seq_id[i][0] = 0; b.logits[i] = (i == n-1);
        }
        if (llama_decode(ctx, b) != 0) die("decode full");
        float* lg = llama_get_logits_ith(ctx, n-1);
        if (!lg) die("no reference logits");
        memcpy(ref.data(), lg, n_vocab*sizeof(float));
        if (cbd.got) true_boundary = cbd.buf;
        llama_batch_free(b);
        llama_free(ctx);
    }

    // ---- Shard A: layers [0,k), emit raw boundary residual for all positions ----
    std::vector<float> boundary((size_t)n_embd * n);
    {
        llama_context* ctx = make_ctx(0, k, /*embeddings=*/true);
        if (!ctx) die("ctx_A");
        llama_set_causal_attn(ctx, true); // embeddings ctx defaults non-causal; the LM is causal
        // explicit batch: output every position (embeddings, pooling NONE)
        llama_batch b = llama_batch_init(n, 0, 1);
        b.n_tokens = n;
        for (int i = 0; i < n; i++) {
            b.token[i] = toks[i]; b.pos[i] = i; b.n_seq_id[i] = 1; b.seq_id[i][0] = 0; b.logits[i] = 1;
        }
        if (llama_decode(ctx, b) != 0) die("decode A");
        for (int i = 0; i < n; i++) {
            float* e = llama_get_embeddings_ith(ctx, i);
            if (!e) die("no embeddings from shard A");
            memcpy(&boundary[(size_t)i*n_embd], e, n_embd*sizeof(float));
        }
        llama_batch_free(b);
        llama_free(ctx);
    }

    // ---- Check A: Shard A boundary vs the TRUE residual entering layer k (from callback) ----
    double a_max = -1;
    if (!true_boundary.empty() && true_boundary.size() == boundary.size()) {
        std::vector<float> a(boundary.begin(), boundary.end());
        DiffStat d = compare(a, true_boundary, 5);
        a_max = d.max_abs;
        printf("[Check A] shard-A residual vs unsplit l_out-%d: max_abs=%.3e mean_abs=%.3e  (expect ~0)\n",
               k-1, d.max_abs, d.mean_abs);
    } else {
        printf("[Check A] (skipped: no matching l_out-%d capture)\n", k-1);
    }

    // helper: run shard B given a boundary buffer -> logits at last position
    auto run_shardB = [&](const std::vector<float>& bnd) {
        llama_context* ctx = make_ctx(k, -1, /*embeddings=*/false);
        if (!ctx) die("ctx_B");
        llama_batch b = llama_batch_init(n, n_embd, 1);
        b.n_tokens = n;
        for (int i = 0; i < n; i++) {
            memcpy(&b.embd[(size_t)i*n_embd], &bnd[(size_t)i*n_embd], n_embd*sizeof(float));
            b.pos[i] = i; b.n_seq_id[i] = 1; b.seq_id[i][0] = 0; b.logits[i] = (i == n-1);
        }
        if (llama_decode(ctx, b) != 0) die("decode B");
        std::vector<float> out(n_vocab);
        memcpy(out.data(), llama_get_logits_ith(ctx, n-1), n_vocab*sizeof(float));
        llama_batch_free(b);
        return std::make_pair(ctx, out); // caller frees ctx
    };

    // ---- Check C (DoD): split with F32 boundary at the backend's native precision ----
    // This is the DoD's "same-backend" test: the split mechanism must reproduce the
    // unsplit logits to within 1e-3 max-abs. F32 boundary = the natural same-backend payload.
    double f32_max = 1e9;
    {
        auto [ctx, got] = run_shardB(boundary);
        llama_free(ctx);
        DiffStat d = compare(ref, got);
        f32_max = d.max_abs;
        printf("[Check C] split (F32 boundary) vs unsplit logits: max_abs=%.3e mean_abs=%.3e argmax %d%s%d top10=%d/10  -> %s\n",
               d.max_abs, d.mean_abs, d.argmax_ref, d.argmax_ref==d.argmax_got?"==":"!=", d.argmax_got, d.topk_overlap,
               (f32_max < 1e-3 && d.argmax_ref==d.argmax_got) ? "PASS(<1e-3)" : "FAIL");
    }

    // ---- Check B: FP16 boundary round-trip characterization (test item f) ----
    // The blueprint's default activation payload is FP16 (spec §1.3). This measures the
    // logit cost of an FP16 boundary; it is NOT gated to 1e-3 — FP drift is documented
    // semantic-continuity behavior (spec I8). We check argmax stability instead.
    std::vector<float> boundary_f16(boundary.size());
    {
        std::vector<ggml_fp16_t> h(boundary.size());
        ggml_fp32_to_fp16_row(boundary.data(), h.data(), (int64_t)boundary.size());
        ggml_fp16_to_fp32_row(h.data(), boundary_f16.data(), (int64_t)boundary.size());
    }
    double f16_max = 1e9; bool f16_argmax_ok = false; bool replay_ok = false;
    {
        auto [ctx, got] = run_shardB(boundary_f16);
        DiffStat d = compare(ref, got);
        f16_max = d.max_abs; f16_argmax_ok = (d.argmax_ref == d.argmax_got);
        printf("[Check B] split (FP16 boundary) vs unsplit logits: max_abs=%.3e mean_abs=%.3e argmax %d%s%d top10=%d/10  (payload cost; argmax %s)\n",
               d.max_abs, d.mean_abs, d.argmax_ref, d.argmax_ref==d.argmax_got?"==":"!=", d.argmax_got, d.topk_overlap,
               f16_argmax_ok ? "stable" : "CHANGED");

        // ---- Check D (DoD): KV truncate at an input position and replay (test item d) ----
        int p = n / 2; if (p < 1) p = 1;
        llama_memory_t mem = llama_get_memory(ctx);
        if (!llama_memory_seq_rm(mem, 0, p, -1)) die("seq_rm failed");
        llama_batch rb = llama_batch_init(n - p, n_embd, 1);
        rb.n_tokens = n - p;
        for (int i = p; i < n; i++) {
            int j = i - p;
            memcpy(&rb.embd[(size_t)j*n_embd], &boundary_f16[(size_t)i*n_embd], n_embd*sizeof(float));
            rb.pos[j] = i; rb.n_seq_id[j] = 1; rb.seq_id[j][0] = 0; rb.logits[j] = (i == n-1);
        }
        if (llama_decode(ctx, rb) != 0) die("decode replay");
        std::vector<float> replay(n_vocab);
        float* rlg = llama_get_logits_ith(ctx, (n - p) - 1); // last token of the replay batch
        if (!rlg) die("no replay logits");
        memcpy(replay.data(), rlg, n_vocab*sizeof(float));
        llama_batch_free(rb);
        DiffStat dr = compare(got, replay);
        replay_ok = dr.max_abs < 1e-4;
        printf("[Check D] KV truncate@pos%d + replay vs pre-truncate logits: max_abs=%.3e  -> %s\n",
               p, dr.max_abs, replay_ok ? "PASS(reproduces)" : "FAIL");
        llama_free(ctx);
    }

    // DoD: the split mechanism reproduces unsplit logits on the same backend (F32, <1e-3)
    // and KV truncate+replay reproduces them. FP16 payload cost is reported separately.
    bool boundary_ok = (a_max < 0) || (a_max < 1e-4);   // exact if the callback ran
    bool dod_pass = (f32_max < 1e-3) && replay_ok && boundary_ok;
    printf("\n--- M-1 summary ---\n");
    printf("  boundary extraction exact : %s (max_abs=%.3e)\n", (a_max>=0 && a_max<1e-4)?"yes":(a_max<0?"n/a":"NO"), a_max<0?0.0:a_max);
    printf("  F32 split == unsplit      : %s (max_abs=%.3e, DoD <1e-3)\n", f32_max<1e-3?"yes":"NO", f32_max);
    printf("  KV truncate+replay exact  : %s\n", replay_ok?"yes":"NO");
    printf("  FP16 boundary payload cost: max_abs=%.3e on logits, argmax %s (item f; spec I8)\n",
           f16_max, f16_argmax_ok?"stable":"CHANGED");
    printf("\n=== M-1 DoD: %s ===\n", dod_pass ? "PASS" : "FAIL");
    llama_model_free(model);
    return dod_pass ? 0 : 1;
}

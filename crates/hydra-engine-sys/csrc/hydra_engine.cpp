// hydra_engine.cpp — implementation of the narrow C ABI over the vendored (patched) llama.cpp.
// Wraps exactly the calls the M-1 spike proved (spike/src/shard_split.cpp): windowed layer-range
// contexts, boundary inject/extract via batch.embd / get_embeddings, logits without sampling, KV
// truncate. No protocol logic here.

#include "hydra_engine.h"

#include "llama.h"
#include "gguf.h"   /* [audit H6] the vendored parser, for the fuzz probe */

#include <cstring>
#include <string>
#include <vector>

struct HydraModel {
    llama_model*       model = nullptr;
    const llama_vocab* vocab = nullptr;
    int32_t n_layer = 0, n_embd = 0, n_vocab = 0;
    // [P2·10b] the shard load window this model was loaded with; (0, -1) == full load.
    int32_t load_l0 = 0, load_l1 = -1;
};

struct HydraContext {
    llama_context* ctx = nullptr;
    int32_t n_embd = 0, n_vocab = 0;
    int32_t n_ctx = 0, n_batch = 0;   // [audit H7] the bounds every apply is checked against
    bool embeddings = false;
};

// [audit 1c] Every entry point below is wrapped in try/catch(...). llama.cpp throws
// std::runtime_error from loaders, context init and the tokenizer on malformed input; an
// exception escaping a C ABI into Rust is undefined behaviour, not an error return. The catch
// maps any throw to the entry point's failure value (nullptr / a negative status / no-op).
#define HYDRA_GUARD_BEGIN try {
#define HYDRA_GUARD_END(fail) } catch (...) { return fail; }
#define HYDRA_GUARD_END_VOID } catch (...) { return; }

static bool g_backends_loaded = false;

extern "C" {

HydraModel* hydra_model_load(const char* path, int32_t n_gpu_layers) {
    HYDRA_GUARD_BEGIN
    if (!path) return nullptr;
    if (!g_backends_loaded) { ggml_backend_load_all(); g_backends_loaded = true; }
    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = n_gpu_layers;
    llama_model* model = llama_model_load_from_file(path, mp);
    if (!model) return nullptr;
    auto* h = new HydraModel();
    h->model   = model;
    h->vocab   = llama_model_get_vocab(model);
    h->n_layer = llama_model_n_layer(model);
    h->n_embd  = llama_model_n_embd(model);
    h->n_vocab = llama_vocab_n_tokens(h->vocab);
    return h;
    HYDRA_GUARD_END(nullptr)
}

HydraModel* hydra_model_load_shard(const char* path, int32_t l0, int32_t l1, int32_t n_gpu_layers) {
    HYDRA_GUARD_BEGIN
    if (!path) return nullptr;
    if (l1 <= l0 || l0 < 0) return nullptr;   // an empty/inverted window is never a shard
    if (!g_backends_loaded) { ggml_backend_load_all(); g_backends_loaded = true; }
    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers  = n_gpu_layers;
    mp.il_load_start = l0;
    mp.il_load_end   = l1;   // >= 0 activates the shard load window (see llama-model.cpp)
    llama_model* model = llama_model_load_from_file(path, mp);
    if (!model) return nullptr;
    auto* h = new HydraModel();
    h->model   = model;
    h->vocab   = llama_model_get_vocab(model);
    h->n_layer = llama_model_n_layer(model);   // the FULL model's layer count (shard metadata is verbatim)
    h->n_embd  = llama_model_n_embd(model);
    h->n_vocab = llama_vocab_n_tokens(h->vocab);
    h->load_l0 = l0;
    h->load_l1 = l1;
    return h;
    HYDRA_GUARD_END(nullptr)
}

void hydra_model_load_window(const HydraModel* m, int32_t* l0, int32_t* l1) {
    HYDRA_GUARD_BEGIN
    if (!m) return;
    if (l0) *l0 = m->load_l0;
    if (l1) *l1 = m->load_l1;
    HYDRA_GUARD_END_VOID
}

HydraModel* hydra_model_load_vocab_only(const char* path) {
    HYDRA_GUARD_BEGIN
    if (!path) return nullptr;
    if (!g_backends_loaded) { ggml_backend_load_all(); g_backends_loaded = true; }
    llama_model_params mp = llama_model_default_params();
    mp.vocab_only = true;
    llama_model* model = llama_model_load_from_file(path, mp);
    if (!model) return nullptr;
    auto* h = new HydraModel();
    h->model   = model;
    h->vocab   = llama_model_get_vocab(model);
    h->n_layer = llama_model_n_layer(model);
    h->n_embd  = llama_model_n_embd(model);
    h->n_vocab = llama_vocab_n_tokens(h->vocab);
    return h;
    HYDRA_GUARD_END(nullptr)
}

void hydra_model_free(HydraModel* m) {
    HYDRA_GUARD_BEGIN
    if (!m) return;
    if (m->model) llama_model_free(m->model);
    delete m;
    HYDRA_GUARD_END_VOID
}

HydraModelInfo hydra_model_info(const HydraModel* m) {
    HYDRA_GUARD_BEGIN
    HydraModelInfo info{0, 0, 0};
    if (m) { info.n_layer = m->n_layer; info.n_embd = m->n_embd; info.n_vocab = m->n_vocab; }
    return info;
    HYDRA_GUARD_END((HydraModelInfo{0, 0, 0}))
}

int32_t hydra_tokenize(const HydraModel* m, const char* text, int32_t text_len,
                       int32_t* out, int32_t cap) {
    HYDRA_GUARD_BEGIN
    if (!m || !text) return -HYDRA_E_NULL;
    int32_t need = -llama_tokenize(m->vocab, text, text_len, nullptr, 0, /*add_special=*/true, /*parse_special=*/true);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need; // caller resizes and retries
    int32_t got = llama_tokenize(m->vocab, text, text_len, out, cap, true, true);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
    HYDRA_GUARD_END(-HYDRA_E_TOKENIZE)
}

int32_t hydra_tokenize_ex(const HydraModel* m, const char* text, int32_t text_len,
                          int32_t add_special, int32_t parse_special, int32_t* out, int32_t cap) {
    HYDRA_GUARD_BEGIN
    if (!m || !text) return -HYDRA_E_NULL;
    int32_t need = -llama_tokenize(m->vocab, text, text_len, nullptr, 0, add_special != 0, parse_special != 0);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need;
    int32_t got = llama_tokenize(m->vocab, text, text_len, out, cap, add_special != 0, parse_special != 0);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
    HYDRA_GUARD_END(-HYDRA_E_TOKENIZE)
}

int32_t hydra_token_to_piece(const HydraModel* m, int32_t token, int32_t special,
                             uint8_t* out, int32_t cap) {
    HYDRA_GUARD_BEGIN
    if (!m) return -HYDRA_E_NULL;
    int32_t need = -llama_token_to_piece(m->vocab, token, nullptr, 0, /*lstrip=*/0, special != 0);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need;
    int32_t got = llama_token_to_piece(m->vocab, token, (char*) out, cap, 0, special != 0);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
    HYDRA_GUARD_END(-HYDRA_E_TOKENIZE)
}

HydraContext* hydra_context_new(HydraModel* m, int32_t l0, int32_t l1,
                                int32_t embeddings, int32_t n_ctx, int32_t n_batch) {
    HYDRA_GUARD_BEGIN
    if (!m) return nullptr;
    // [P2·10b] A shard-loaded model holds ONLY layers [load_l0, load_l1). A compute window that
    // escapes it would reference tensors that were never created — so refuse at the boundary
    // instead of null-dereferencing inside the graph. Structural, not advisory: the same
    // refuse-don't-warn posture the manifest verification takes on the Rust side.
    if (m->load_l1 >= 0) {
        const int32_t want_l1 = (l1 < 0) ? m->n_layer : l1;
        if (l0 < m->load_l0 || want_l1 > m->load_l1) return nullptr;
    }
    if (n_ctx <= 0 || n_batch <= 0) return nullptr;
    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = (uint32_t) n_ctx;
    cp.n_batch = (uint32_t) n_batch;
    cp.n_ubatch = (uint32_t) n_batch;
    cp.no_perf = true;
    cp.il_start = l0;
    cp.il_end = l1;                 // -1 => to the last layer
    cp.embeddings = embeddings != 0;
    if (cp.embeddings) cp.pooling_type = LLAMA_POOLING_TYPE_NONE; // per-token residual, no pooling
    llama_context* ctx = llama_init_from_model(m->model, cp);
    if (!ctx) return nullptr;
    if (cp.embeddings) llama_set_causal_attn(ctx, true); // embeddings ctx defaults non-causal; LM is causal
    auto* h = new HydraContext();
    h->ctx = ctx; h->n_embd = m->n_embd; h->n_vocab = m->n_vocab; h->embeddings = cp.embeddings;
    h->n_ctx = n_ctx; h->n_batch = n_batch;
    return h;
    HYDRA_GUARD_END(nullptr)
}

void hydra_context_free(HydraContext* c) {
    HYDRA_GUARD_BEGIN
    if (!c) return;
    if (c->ctx) llama_free(c->ctx);
    delete c;
    HYDRA_GUARD_END_VOID
}

int32_t hydra_apply(HydraContext* c, const int32_t* tokens, const float* boundary_in,
                    int32_t pos0, int32_t n, float* boundary_out) {
    HYDRA_GUARD_BEGIN
    if (!c || !c->ctx) return HYDRA_E_NULL;
    if (n <= 0) return HYDRA_E_ARG;
    if ((tokens == nullptr) == (boundary_in == nullptr)) return HYDRA_E_ARG; // exactly one
    // [audit H7] n > n_batch is refused here as well as in Rust: llama_batch_init would allocate
    // for it and llama_decode would fail (or worse) inside the graph. Same for a position range
    // escaping n_ctx and a token id outside the vocabulary (M5) — the shim is the last line, not
    // the first, but it is a line.
    if (n > c->n_batch) return HYDRA_E_ARG;
    if (pos0 < 0 || pos0 > c->n_ctx - n) return HYDRA_E_ARG;
    if (tokens) {
        for (int i = 0; i < n; i++) if (tokens[i] < 0 || tokens[i] >= c->n_vocab) return HYDRA_E_ARG;
    }

    const int32_t n_embd = c->n_embd;
    llama_batch b = llama_batch_init(n, boundary_in ? n_embd : 0, 1);
    b.n_tokens = n;
    for (int i = 0; i < n; i++) {
        if (boundary_in) {
            memcpy(&b.embd[(size_t) i * n_embd], &boundary_in[(size_t) i * n_embd], n_embd * sizeof(float));
        } else {
            b.token[i] = tokens[i];
        }
        b.pos[i] = pos0 + i;
        b.n_seq_id[i] = 1;
        b.seq_id[i][0] = 0;
        // embeddings ctx: output every position (extract residual); logits ctx: only the last.
        b.logits[i] = c->embeddings ? 1 : (i == n - 1);
    }
    int rc = llama_decode(c->ctx, b);
    if (rc != 0) { llama_batch_free(b); return HYDRA_E_DECODE; }

    if (boundary_out && c->embeddings) {
        for (int i = 0; i < n; i++) {
            float* e = llama_get_embeddings_ith(c->ctx, i);
            if (!e) { llama_batch_free(b); return HYDRA_E_DECODE; }
            memcpy(&boundary_out[(size_t) i * n_embd], e, n_embd * sizeof(float));
        }
    }
    llama_batch_free(b);
    return HYDRA_OK;
    HYDRA_GUARD_END(HYDRA_E_DECODE)
}

int32_t hydra_logits(HydraContext* c, int32_t at_pos, float* out, int32_t out_cap) {
    HYDRA_GUARD_BEGIN
    if (!c || !c->ctx || !out) return HYDRA_E_NULL;
    if (out_cap < c->n_vocab) return HYDRA_E_SHAPE;
    float* lg = llama_get_logits_ith(c->ctx, at_pos);
    if (!lg) return HYDRA_E_DECODE;
    memcpy(out, lg, (size_t) c->n_vocab * sizeof(float));
    return HYDRA_OK;
    HYDRA_GUARD_END(HYDRA_E_DECODE)
}

int32_t hydra_kv_truncate(HydraContext* c, int32_t pos) {
    HYDRA_GUARD_BEGIN
    if (!c || !c->ctx) return HYDRA_E_NULL;
    llama_memory_t mem = llama_get_memory(c->ctx);
    if (!mem) return HYDRA_E_KV;
    if (!llama_memory_seq_rm(mem, 0, pos, -1)) return HYDRA_E_KV;
    return HYDRA_OK;
    HYDRA_GUARD_END(HYDRA_E_KV)
}

/* [audit H6] The vendored-parser fuzz entry point. Guarded like every other entry point: the
 * point of the exercise is that a hostile GGUF must produce a return value, never an unwind into
 * Rust and never an abort. */
int32_t hydra_gguf_probe(const char* path) {
    HYDRA_GUARD_BEGIN
    if (!path) return HYDRA_E_NULL;
    struct gguf_init_params p = { /*no_alloc=*/true, /*ctx=*/NULL };
    struct gguf_context * g = gguf_init_from_file(path, p);
    if (!g) return HYDRA_E_LOAD;
    /* Walk the accessors as well: a parser that accepts a hostile file and then hands out an
     * out-of-range value has moved the crash rather than prevented it (the same discipline the
     * Rust gguf target uses). */
    const int64_t n_kv = gguf_get_n_kv(g);
    for (int64_t i = 0; i < n_kv; i++) { (void) gguf_get_kv_type(g, i); (void) gguf_get_key(g, i); }
    const int64_t n_t = gguf_get_n_tensors(g);
    for (int64_t i = 0; i < n_t; i++) { (void) gguf_get_tensor_name(g, i); (void) gguf_get_tensor_type(g, i); (void) gguf_get_tensor_offset(g, i); }
    gguf_free(g);
    return HYDRA_OK;
    HYDRA_GUARD_END(HYDRA_E_LOAD)
}

} // extern "C"

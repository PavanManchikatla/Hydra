// hydra_engine.cpp — implementation of the narrow C ABI over the vendored (patched) llama.cpp.
// Wraps exactly the calls the M-1 spike proved (spike/src/shard_split.cpp): windowed layer-range
// contexts, boundary inject/extract via batch.embd / get_embeddings, logits without sampling, KV
// truncate. No protocol logic here.

#include "hydra_engine.h"

#include "llama.h"

#include <cstring>
#include <string>
#include <vector>

struct HydraModel {
    llama_model*       model = nullptr;
    const llama_vocab* vocab = nullptr;
    int32_t n_layer = 0, n_embd = 0, n_vocab = 0;
};

struct HydraContext {
    llama_context* ctx = nullptr;
    int32_t n_embd = 0, n_vocab = 0;
    bool embeddings = false;
};

static bool g_backends_loaded = false;

extern "C" {

HydraModel* hydra_model_load(const char* path, int32_t n_gpu_layers) {
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
}

HydraModel* hydra_model_load_vocab_only(const char* path) {
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
}

void hydra_model_free(HydraModel* m) {
    if (!m) return;
    if (m->model) llama_model_free(m->model);
    delete m;
}

HydraModelInfo hydra_model_info(const HydraModel* m) {
    HydraModelInfo info{0, 0, 0};
    if (m) { info.n_layer = m->n_layer; info.n_embd = m->n_embd; info.n_vocab = m->n_vocab; }
    return info;
}

int32_t hydra_tokenize(const HydraModel* m, const char* text, int32_t text_len,
                       int32_t* out, int32_t cap) {
    if (!m || !text) return -HYDRA_E_NULL;
    int32_t need = -llama_tokenize(m->vocab, text, text_len, nullptr, 0, /*add_special=*/true, /*parse_special=*/true);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need; // caller resizes and retries
    int32_t got = llama_tokenize(m->vocab, text, text_len, out, cap, true, true);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
}

int32_t hydra_tokenize_ex(const HydraModel* m, const char* text, int32_t text_len,
                          int32_t add_special, int32_t parse_special, int32_t* out, int32_t cap) {
    if (!m || !text) return -HYDRA_E_NULL;
    int32_t need = -llama_tokenize(m->vocab, text, text_len, nullptr, 0, add_special != 0, parse_special != 0);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need;
    int32_t got = llama_tokenize(m->vocab, text, text_len, out, cap, add_special != 0, parse_special != 0);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
}

int32_t hydra_token_to_piece(const HydraModel* m, int32_t token, int32_t special,
                             uint8_t* out, int32_t cap) {
    if (!m) return -HYDRA_E_NULL;
    int32_t need = -llama_token_to_piece(m->vocab, token, nullptr, 0, /*lstrip=*/0, special != 0);
    if (need < 0) return -HYDRA_E_TOKENIZE;
    if (!out || cap < need) return -need;
    int32_t got = llama_token_to_piece(m->vocab, token, (char*) out, cap, 0, special != 0);
    if (got < 0) return -HYDRA_E_TOKENIZE;
    return got;
}

HydraContext* hydra_context_new(HydraModel* m, int32_t l0, int32_t l1,
                                int32_t embeddings, int32_t n_ctx, int32_t n_batch) {
    if (!m) return nullptr;
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
    return h;
}

void hydra_context_free(HydraContext* c) {
    if (!c) return;
    if (c->ctx) llama_free(c->ctx);
    delete c;
}

int32_t hydra_apply(HydraContext* c, const int32_t* tokens, const float* boundary_in,
                    int32_t pos0, int32_t n, float* boundary_out) {
    if (!c || !c->ctx) return HYDRA_E_NULL;
    if (n <= 0) return HYDRA_E_ARG;
    if ((tokens == nullptr) == (boundary_in == nullptr)) return HYDRA_E_ARG; // exactly one

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
}

int32_t hydra_logits(HydraContext* c, int32_t at_pos, float* out, int32_t out_cap) {
    if (!c || !c->ctx || !out) return HYDRA_E_NULL;
    if (out_cap < c->n_vocab) return HYDRA_E_SHAPE;
    float* lg = llama_get_logits_ith(c->ctx, at_pos);
    if (!lg) return HYDRA_E_DECODE;
    memcpy(out, lg, (size_t) c->n_vocab * sizeof(float));
    return HYDRA_OK;
}

int32_t hydra_kv_truncate(HydraContext* c, int32_t pos) {
    if (!c || !c->ctx) return HYDRA_E_NULL;
    llama_memory_t mem = llama_get_memory(c->ctx);
    if (!mem) return HYDRA_E_KV;
    if (!llama_memory_seq_rm(mem, 0, pos, -1)) return HYDRA_E_KV;
    return HYDRA_OK;
}

} // extern "C"

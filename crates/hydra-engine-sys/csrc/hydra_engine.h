/* hydra_engine.h — narrow C ABI over the vendored (patched) llama.cpp.
 *
 * COMPUTE ONLY. This shim moves tensors and runs contiguous layer ranges; it holds no protocol
 * concept (no sessions/epochs/attempts/WAL/fencing) — those live in `hydra-state` (BLUEPRINT §1.4).
 * Boundaries cross this ABI as f32 (wire precision is the Rust transport's job).
 *
 * The layer window is a *context* property (`llama_context_params.il_start/il_end`, the M-1 spike
 * patch): the full model loads once, each context executes only layers [l0, l1). l1 == -1 means
 * "to the last layer". An embeddings context (l1 < n_layer) skips the final norm + lm_head and
 * emits the raw residual entering layer l1; a non-embeddings context runs to logits.
 */
#ifndef HYDRA_ENGINE_H
#define HYDRA_ENGINE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct HydraModel   HydraModel;
typedef struct HydraContext HydraContext;

typedef struct {
    int32_t n_layer;
    int32_t n_embd;
    int32_t n_vocab;
} HydraModelInfo;

/* status codes (0 == OK) */
#define HYDRA_OK          0
#define HYDRA_E_NULL      1
#define HYDRA_E_LOAD      2
#define HYDRA_E_CONTEXT   3
#define HYDRA_E_TOKENIZE  4
#define HYDRA_E_DECODE    5
#define HYDRA_E_SHAPE     6
#define HYDRA_E_KV        7
#define HYDRA_E_ARG       8

/* Load the full GGUF. n_gpu_layers: 0 = CPU (the deterministic DoD backend), 99 = all on GPU. */
HydraModel* hydra_model_load(const char* path, int32_t n_gpu_layers);
void        hydra_model_free(HydraModel* m);
HydraModelInfo hydra_model_info(const HydraModel* m);

/* Tokenize `text` (`text_len` bytes) with BOS + special handling. Writes up to `cap` tokens into
 * `out`; returns the token count, or the NEGATED required count if `cap` was too small. */
int32_t hydra_tokenize(const HydraModel* m, const char* text, int32_t text_len,
                       int32_t* out, int32_t cap);

/* New context windowed to layers [l0, l1) (l1 == -1 => to the last layer). embeddings != 0 makes
 * a boundary-emitting context (extract the residual leaving l1-1); 0 makes a logits context. */
HydraContext* hydra_context_new(HydraModel* m, int32_t l0, int32_t l1,
                                int32_t embeddings, int32_t n_ctx, int32_t n_batch);
void          hydra_context_free(HydraContext* c);

/* Apply `n` positions [pos0, pos0+n) through this context's layer window. Exactly one of
 * `tokens` / `boundary_in` (n_embd*n f32) must be non-NULL — tokens for a shard that embeds
 * (l0 == 0), boundary_in for a shard consuming an injected residual. If `boundary_out`
 * (n_embd*n f32) is non-NULL and this is an embeddings context, the residual leaving l1-1 is
 * written there. NEVER samples. */
int32_t hydra_apply(HydraContext* c, const int32_t* tokens, const float* boundary_in,
                    int32_t pos0, int32_t n, float* boundary_out);

/* Copy the retained (unsampled) logits at position `at_pos` into `out` (must hold >= n_vocab). */
int32_t hydra_logits(HydraContext* c, int32_t at_pos, float* out, int32_t out_cap);

/* Drop cached KV for positions >= pos (recovery truncate; I7a). */
int32_t hydra_kv_truncate(HydraContext* c, int32_t pos);

#ifdef __cplusplus
}
#endif
#endif /* HYDRA_ENGINE_H */

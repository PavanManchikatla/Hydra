// ref_logits — M-1 reference path.
// Loads a GGUF on the CPU backend (n_gpu_layers = 0), runs one prompt through UNSPLIT
// llama.cpp, and dumps the raw next-token logits for the final prompt position.
// This is the ground truth the sharded path (shard_split) must match within 1e-3 max-abs.
//
// Output: writes <out>.logits.bin  (n_vocab little-endian f32)
//         writes <out>.tokens.bin  (n_tokens int32) so the split path uses identical tokens
//         prints n_vocab, argmax, and top-5 to stdout.

#include "llama.h"

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>
#include <algorithm>

static void die(const char* m) { fprintf(stderr, "ref_logits: %s\n", m); exit(1); }

int main(int argc, char** argv) {
    std::string model_path, out_prefix = "ref";
    std::string prompt = "The capital of France is";
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-m") && i + 1 < argc)      model_path = argv[++i];
        else if (!strcmp(argv[i], "-o") && i + 1 < argc) out_prefix = argv[++i];
        else if (!strcmp(argv[i], "-p") && i + 1 < argc) prompt = argv[++i];
    }
    if (model_path.empty()) die("usage: ref_logits -m model.gguf [-p prompt] [-o out_prefix]");

    ggml_backend_load_all();

    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = 0;                       // CPU backend — the DoD comparison backend
    llama_model* model = llama_model_load_from_file(model_path.c_str(), mp);
    if (!model) die("failed to load model");

    const llama_vocab* vocab = llama_model_get_vocab(model);
    const int n_vocab = llama_vocab_n_tokens(vocab);

    // tokenize (add BOS, parse special) — the split path reuses these exact ids
    int n_prompt = -llama_tokenize(vocab, prompt.c_str(), (int)prompt.size(), nullptr, 0, true, true);
    std::vector<llama_token> toks(n_prompt);
    if (llama_tokenize(vocab, prompt.c_str(), (int)prompt.size(), toks.data(), (int)toks.size(), true, true) < 0)
        die("tokenize failed");

    llama_context_params cp = llama_context_default_params();
    cp.n_ctx   = n_prompt + 8;
    cp.n_batch = n_prompt;
    cp.no_perf = true;
    llama_context* ctx = llama_init_from_model(model, cp);
    if (!ctx) die("failed to create context");

    llama_batch batch = llama_batch_get_one(toks.data(), (int)toks.size());
    if (llama_decode(ctx, batch) != 0) die("decode failed");

    // logits for the LAST position (default: only last is computed)
    float* logits = llama_get_logits_ith(ctx, (int)toks.size() - 1);
    if (!logits) die("no logits");

    std::vector<float> lv(logits, logits + n_vocab);

    // dump logits + tokens for the split path
    {
        std::string lf = out_prefix + ".logits.bin";
        FILE* f = fopen(lf.c_str(), "wb");
        fwrite(lv.data(), sizeof(float), n_vocab, f);
        fclose(f);
        std::string tf = out_prefix + ".tokens.bin";
        FILE* g = fopen(tf.c_str(), "wb");
        std::vector<int32_t> ti(toks.begin(), toks.end());
        fwrite(ti.data(), sizeof(int32_t), ti.size(), g);
        fclose(g);
    }

    // report argmax + top-5
    std::vector<int> idx(n_vocab);
    for (int i = 0; i < n_vocab; i++) idx[i] = i;
    std::partial_sort(idx.begin(), idx.begin() + 5, idx.end(),
                      [&](int a, int b){ return lv[a] > lv[b]; });
    printf("n_tokens=%d n_vocab=%d\n", (int)toks.size(), n_vocab);
    printf("argmax=%d logit=%.6f\n", idx[0], lv[idx[0]]);
    printf("top5:");
    for (int i = 0; i < 5; i++) {
        char buf[128]; int n = llama_token_to_piece(vocab, idx[i], buf, sizeof(buf), 0, true);
        printf(" [%d:%.4f:'%.*s']", idx[i], lv[idx[i]], n > 0 ? n : 0, buf);
    }
    printf("\nwrote %s.logits.bin (%d f32) and %s.tokens.bin\n",
           out_prefix.c_str(), n_vocab, out_prefix.c_str());

    llama_free(ctx);
    llama_model_free(model);
    return 0;
}

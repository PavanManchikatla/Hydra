cmake_minimum_required(VERSION 3.20)
project(hydra_spike CXX C)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

# Link against the vendored, pinned llama.cpp build tree (BLUEPRINT §1.2).
# The exported llama-config.cmake targets an *installed* layout, so we link the
# build-tree dylibs directly by path instead.
set(LLAMA_ROOT  "${CMAKE_CURRENT_SOURCE_DIR}/../vendor/llama.cpp")
set(LLAMA_BIN   "${LLAMA_ROOT}/build/bin")

set(LLAMA_INCLUDE_DIRS
    "${LLAMA_ROOT}/include"
    "${LLAMA_ROOT}/ggml/include")

find_library(LIB_LLAMA      llama      HINTS "${LLAMA_BIN}" REQUIRED)
find_library(LIB_GGML       ggml       HINTS "${LLAMA_BIN}" REQUIRED)
find_library(LIB_GGML_BASE  ggml-base  HINTS "${LLAMA_BIN}" REQUIRED)
find_library(LIB_GGML_CPU   ggml-cpu   HINTS "${LLAMA_BIN}" REQUIRED)

set(SPIKE_LIBS ${LIB_LLAMA} ${LIB_GGML} ${LIB_GGML_BASE} ${LIB_GGML_CPU})

function(hydra_spike_bin name)
  add_executable(${name} src/${name}.cpp)
  target_include_directories(${name} PRIVATE ${LLAMA_INCLUDE_DIRS})
  target_link_libraries(${name} PRIVATE ${SPIKE_LIBS})
  set_target_properties(${name} PROPERTIES
      BUILD_RPATH "${LLAMA_BIN}" INSTALL_RPATH "${LLAMA_BIN}")
endfunction()

# ref_logits: unsplit reference — dumps raw logits for the last prompt position (CPU).
hydra_spike_bin(ref_logits)

# shard_split: the M-1 demonstration — two contiguous layer ranges, boundary-activation
# inject/extract, per-range KV, KV truncate+replay, final-range logits (no sampling).
hydra_spike_bin(shard_split)

//! Curated, version-locked LLM model presets surfaced in the Settings UI.
//! Custom (free-text) is the escape hatch; it forces
//! `thinking_kwarg = ThinkingKwarg::None` because the model family can't
//! be inferred from a raw HF repo name.

use serde::{Deserialize, Serialize};

/// Quantization type used for both K and V cache tensors.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KvCacheType {
    Q4_0,
    Q4_1,
    IQ4_NL,
    Q5_0,
    Q5_1,
    Q8_0,
}

impl KvCacheType {
    pub fn runner_value(self) -> &'static str {
        match self {
            Self::Q4_0 => "KV_CACHE_TYPE_Q4_0",
            Self::Q4_1 => "KV_CACHE_TYPE_Q4_1",
            Self::IQ4_NL => "KV_CACHE_TYPE_IQ4_NL",
            Self::Q5_0 => "KV_CACHE_TYPE_Q5_0",
            Self::Q5_1 => "KV_CACHE_TYPE_Q5_1",
            Self::Q8_0 => "KV_CACHE_TYPE_Q8_0",
        }
    }

    pub fn from_runner_value(value: &str) -> Option<Self> {
        match value.trim() {
            "KV_CACHE_TYPE_Q4_0" | "Q4_0" => Some(Self::Q4_0),
            "KV_CACHE_TYPE_Q4_1" | "Q4_1" => Some(Self::Q4_1),
            "KV_CACHE_TYPE_IQ4_NL" | "IQ4_NL" => Some(Self::IQ4_NL),
            "KV_CACHE_TYPE_Q5_0" | "Q5_0" => Some(Self::Q5_0),
            "KV_CACHE_TYPE_Q5_1" | "Q5_1" => Some(Self::Q5_1),
            "KV_CACHE_TYPE_Q8_0" | "Q8_0" => Some(Self::Q8_0),
            _ => None,
        }
    }

    fn bytes_per_element(self) -> f32 {
        match self {
            // ggml block quantization stores scale/min side data per 32 values.
            Self::Q4_0 | Self::IQ4_NL => 18.0 / 32.0,
            Self::Q4_1 => 20.0 / 32.0,
            Self::Q5_0 => 22.0 / 32.0,
            Self::Q5_1 => 24.0 / 32.0,
            Self::Q8_0 => 34.0 / 32.0,
        }
    }
}

pub const DEFAULT_KV_CACHE_TYPE: KvCacheType = KvCacheType::Q4_0;

/// What `chat_template_kwargs.enable_thinking` value to send for a given
/// preset. Per-model because the jinja templates flip the meaning in
/// opposite directions:
///
///   - **Qwen3**: `enable_thinking: false` SUPPRESSES the
///     `<think>…</think>` block that swallows tool calls (QwenLM/Qwen3
///     #1817, ggml-org/llama.cpp #20837).
///   - **Gemma 4 26B-A4B**: `enable_thinking: true` SUPPRESSES the
///     `<|channel>thought\n<channel|>` assistant prefix that the jinja
///     otherwise injects, freeing position 0 for the PEG-Gemma4 grammar
///     to emit `<|tool_call>call:…`. The 26B-A4B model card documents
///     the empty-thought tag emission caveat; the plugin's
///     `apply_oai_template_with_tools` then forwards this value both as
///     the jinja kwarg AND as the `OpenAIChatTemplateParams.enable_thinking`
///     bool that drives the C++ grammar/parser path.
///   - **None**: skip the kwarg entirely. Models whose chat template
///     does not branch on `enable_thinking` (most LFM/Functionary/etc.
///     specialised templates, the custom free-text path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingKwarg {
    /// Do not send `chat_template_kwargs`.
    None,
    /// Send `chat_template_kwargs={"enable_thinking":false}`.
    Disable,
    /// Send `chat_template_kwargs={"enable_thinking":true}`.
    Enable,
}

/// A single curated model preset exposed in Settings.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LlmPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub hf_repo: &'static str,
    pub gguf_file: &'static str,
    pub recommended_ctx_size: u32,
    pub min_ctx_size: u32,
    pub estimated_model_ram_gb: f32,
    /// Coarse headline figure (model + KV cache at the recommended ctx_size with
    /// the default KV cache type). The UI shows a live figure via the
    /// `estimate_*_ram_gb` math instead; this is kept within ~1GB of that live
    /// total by `coarse_ram_estimates_track_recommended_ctx_with_default_kv_cache`
    /// so the headline never contradicts the detailed line.
    pub estimated_ram_gb: u32,
    pub kv_layers: u32,
    pub kv_embd_k_gqa: u32,
    pub kv_embd_v_gqa: u32,
    /// i18n key (not a localized string) the frontend resolves via `t()`.
    /// Backend stays locale-agnostic so language switching is instant; the
    /// translations live in the bundled `i18n/locales/{ja,en}.json`.
    pub description: &'static str,
    /// Per-model `enable_thinking` policy — see [`ThinkingKwarg`].
    pub thinking_kwarg: ThinkingKwarg,
    /// Enables LLMPromptRunner's runner-level MTP speculative decoding.
    pub mtp_enabled: bool,
    /// Separate MTP draft GGUF for model families that ship one.
    /// Same-file MTP models leave this empty and reuse the target model.
    pub mtp_draft_model: Option<&'static str>,
}

/// Identifier of the default preset. Keep this aligned with `PRESETS[0]`
/// because the UI treats the first row as the visible default selection.
pub const DEFAULT_PRESET_ID: &str = "gemma-4-e2b-it-qat-ud-q4-k-xl";

/// Sentinel id for "free-text custom entry" in the Settings UI. The
/// frontend dropdown emits this when the user picks the custom row;
/// `resolve_local_runtime` then reads `local_model_file` / `local_hf_repo`
/// from settings instead of looking up `PRESETS`.
pub const CUSTOM_PRESET_ID: &str = "custom";

/// Curated, tested model list. Index 0 is the default — see [`DEFAULT_PRESET_ID`].
pub const PRESETS: &[LlmPreset] = &[
    LlmPreset {
        id: DEFAULT_PRESET_ID,
        display_name: "Gemma 4 E2B IT QAT (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-E2B-it-qat-GGUF",
        gguf_file: "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 131_072,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 2.6,
        estimated_ram_gb: 4,
        kv_layers: 35,
        kv_embd_k_gqa: 256,
        kv_embd_v_gqa: 256,
        description: "settings.llmPreset.desc.gemma-4-e2b-it-qat-ud-q4-k-xl",
        // Same Gemma 4 jinja semantics as 26B-A4B — see comment below.
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "gemma-4-e2b-it-qat-mtp-ud-q4-k-xl",
        display_name: "Gemma 4 E2B IT QAT MTP (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-E2B-it-qat-GGUF",
        gguf_file: "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 131_072,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 2.6,
        estimated_ram_gb: 4,
        kv_layers: 35,
        kv_embd_k_gqa: 256,
        kv_embd_v_gqa: 256,
        description: "settings.llmPreset.desc.gemma-4-e2b-it-qat-mtp-ud-q4-k-xl",
        // Same Gemma 4 jinja semantics as 26B-A4B — see comment below.
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: true,
        mtp_draft_model: Some("mtp-gemma-4-E2B-it.gguf"),
    },
    LlmPreset {
        id: "gemma-4-e4b-it-qat-ud-q4-k-xl",
        display_name: "Gemma 4 E4B IT QAT (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-E4B-it-qat-GGUF",
        gguf_file: "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 131_072,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 4.2,
        estimated_ram_gb: 7,
        kv_layers: 42,
        kv_embd_k_gqa: 512,
        kv_embd_v_gqa: 512,
        description: "settings.llmPreset.desc.gemma-4-e4b-it-qat-ud-q4-k-xl",
        // Same Gemma 4 jinja semantics as 26B-A4B — see comment below.
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "gemma-4-e4b-it-qat-mtp-ud-q4-k-xl",
        display_name: "Gemma 4 E4B IT QAT MTP (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-E4B-it-qat-GGUF",
        gguf_file: "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 131_072,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 4.2,
        estimated_ram_gb: 7,
        kv_layers: 42,
        kv_embd_k_gqa: 512,
        kv_embd_v_gqa: 512,
        description: "settings.llmPreset.desc.gemma-4-e4b-it-qat-mtp-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: true,
        mtp_draft_model: Some("mtp-gemma-4-E4B-it.gguf"),
    },
    LlmPreset {
        id: "gemma-4-12b-it-qat-ud-q4-k-xl",
        display_name: "Gemma 4 12B IT QAT (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-12B-it-qat-GGUF",
        gguf_file: "gemma-4-12B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 7.4,
        estimated_ram_gb: 21,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.gemma-4-12b-it-qat-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "gemma-4-12b-it-qat-mtp-ud-q4-k-xl",
        display_name: "Gemma 4 12B IT QAT MTP (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/gemma-4-12B-it-qat-GGUF",
        gguf_file: "gemma-4-12B-it-qat-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 7.4,
        estimated_ram_gb: 21,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.gemma-4-12b-it-qat-mtp-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: true,
        mtp_draft_model: Some("mtp-gemma-4-12b-it.gguf"),
    },
    LlmPreset {
        id: "gemma-4-26b-a4b-it-ud-iq4-nl",
        display_name: "Gemma 4 26B-A4B IT (MoE, IQ4_NL / Unsloth)",
        hf_repo: "unsloth/gemma-4-26B-A4B-it-GGUF",
        gguf_file: "gemma-4-26B-A4B-it-UD-IQ4_NL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 14.0,
        estimated_ram_gb: 26,
        kv_layers: 42,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.gemma-4-26b-a4b-it-ud-iq4-nl",
        // Gemma 4 jinja: `{% if not enable_thinking | default(false) %}<|channel>thought\n<channel|>`.
        // With the default / `false`, an empty thought span is INJECTED as
        // the assistant prefix and the PEG-Gemma4 tool-call grammar
        // (`COMMON_CHAT_FORMAT_PEG_GEMMA4`) never sees position 0 — the
        // model writes prose after the prefix and `<|tool_call>call:…` is
        // not emitted. Sending `enable_thinking:true` omits the prefix so
        // the grammar can fire from token 0 and tool calls actually run.
        // The plugin's `apply_oai_template_with_tools` then mirrors the
        // kwarg into the C++ `OpenAIChatTemplateParams.enable_thinking`
        // bool that drives the grammar/parser pair (model.rs:2087-2110).
        thinking_kwarg: ThinkingKwarg::Enable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-5-9b-ud-q4-k-xl",
        display_name: "Qwen3.5 9B (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/Qwen3.5-9B-GGUF",
        gguf_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 6.0,
        estimated_ram_gb: 16,
        kv_layers: 36,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-5-9b-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-5-9b-mtp-ud-q4-k-xl",
        display_name: "Qwen3.5 9B (MTP, Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/Qwen3.5-9B-MTP-GGUF",
        gguf_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 2048,
        estimated_model_ram_gb: 6.1,
        estimated_ram_gb: 17,
        kv_layers: 36,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-5-9b-mtp-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: true,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-6-27b-ud-q4-k-xl",
        display_name: "Qwen3.6 27B (Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/Qwen3.6-27B-GGUF",
        gguf_file: "Qwen3.6-27B-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 18.0,
        estimated_ram_gb: 32,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-6-27b-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-6-27b-mtp-ud-q4-k-xl",
        display_name: "Qwen3.6 27B (MTP, Q4_K_XL / Unsloth)",
        hf_repo: "unsloth/Qwen3.6-27B-MTP-GGUF",
        gguf_file: "Qwen3.6-27B-UD-Q4_K_XL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 17.9,
        estimated_ram_gb: 32,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-6-27b-mtp-ud-q4-k-xl",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: true,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-6-35b-a3b-ud-iq4-nl",
        display_name: "Qwen3.6 35B-A3B (MoE, IQ4_NL / Unsloth)",
        hf_repo: "unsloth/Qwen3.6-35B-A3B-GGUF",
        gguf_file: "Qwen3.6-35B-A3B-UD-IQ4_NL.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 16.0,
        estimated_ram_gb: 30,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-6-35b-a3b-ud-iq4-nl",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-6-35b-a3b-mtp-ud-q4-k-m",
        display_name: "Qwen3.6 35B-A3B (MTP, Q4_K_M / Unsloth)",
        hf_repo: "unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
        gguf_file: "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 22.7,
        estimated_ram_gb: 37,
        kv_layers: 48,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-6-35b-a3b-mtp-ud-q4-k-m",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: true,
        mtp_draft_model: None,
    },
    LlmPreset {
        // Multi-part GGUF: the file is split into 3 shards
        // (`-00001-of-00003.gguf` etc., total ~60 GB). llama.cpp
        // auto-loads the remaining shards when pointed at the first
        // one, so only the index-1 filename is named here.
        id: "qwen3-5-122b-a10b-ud-iq4-xs",
        display_name: "Qwen3.5 122B-A10B (MoE, IQ4_XS / Unsloth)",
        hf_repo: "unsloth/Qwen3.5-122B-A10B-GGUF",
        gguf_file: "UD-IQ4_XS/Qwen3.5-122B-A10B-UD-IQ4_XS-00001-of-00003.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 60.0,
        estimated_ram_gb: 83,
        kv_layers: 80,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-5-122b-a10b-ud-iq4-xs",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: false,
        mtp_draft_model: None,
    },
    LlmPreset {
        id: "qwen3-5-122b-a10b-mtp-ud-q4-k-s",
        display_name: "Qwen3.5 122B-A10B (MTP, Q4_K_S / Unsloth)",
        hf_repo: "unsloth/Qwen3.5-122B-A10B-MTP-GGUF",
        gguf_file: "UD-Q4_K_S/Qwen3.5-122B-A10B-UD-Q4_K_S-00001-of-00003.gguf",
        recommended_ctx_size: 262_144,
        min_ctx_size: 4096,
        estimated_model_ram_gb: 73.4,
        estimated_ram_gb: 96,
        kv_layers: 80,
        kv_embd_k_gqa: 1024,
        kv_embd_v_gqa: 1024,
        description: "settings.llmPreset.desc.qwen3-5-122b-a10b-mtp-ud-q4-k-s",
        thinking_kwarg: ThinkingKwarg::Disable,
        mtp_enabled: true,
        mtp_draft_model: None,
    },
];

// Parity reference for the live estimate the Settings UI computes in
// `Settings.tsx` (`estimateKvCacheRamGb` / `estimateTotalRamGb`). The frontend
// owns the displayed figure so it can update without a round-trip; this Rust
// copy exists to pin the formula and per-quant constants under unit test.
pub fn estimate_kv_cache_ram_gb(preset: &LlmPreset, ctx_size: u32, kv_type: KvCacheType) -> f32 {
    let bytes = ctx_size as f32
        * preset.kv_layers as f32
        * (preset.kv_embd_k_gqa + preset.kv_embd_v_gqa) as f32
        * kv_type.bytes_per_element();
    bytes / 1024.0 / 1024.0 / 1024.0
}

pub fn estimate_total_ram_gb(preset: &LlmPreset, ctx_size: u32, kv_type: KvCacheType) -> f32 {
    preset.estimated_model_ram_gb + estimate_kv_cache_ram_gb(preset, ctx_size, kv_type)
}

pub fn find_preset(id: &str) -> Option<&'static LlmPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

const LEGACY_PRESET_ID_ALIASES: &[(&str, &str)] = &[
    ("gemma-4-e2b-it-ud-q4-k-xl", "gemma-4-e2b-it-qat-ud-q4-k-xl"),
    ("gemma-4-e4b-it-ud-q4-k-xl", "gemma-4-e4b-it-qat-ud-q4-k-xl"),
    ("gemma-4-12b-it-ud-q4-k-xl", "gemma-4-12b-it-qat-ud-q4-k-xl"),
];

pub fn canonical_preset_id(id: &str) -> Option<&'static str> {
    find_preset(id).map(|p| p.id).or_else(|| {
        LEGACY_PRESET_ID_ALIASES
            .iter()
            .find_map(|(legacy, canonical)| (*legacy == id).then_some(*canonical))
    })
}

pub fn find_preset_or_alias(id: &str) -> Option<&'static LlmPreset> {
    canonical_preset_id(id).and_then(find_preset)
}

pub fn default_preset() -> &'static LlmPreset {
    &PRESETS[0]
}

#[tauri::command]
pub fn list_llm_presets() -> Vec<LlmPreset> {
    PRESETS.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_nonempty() {
        // The Settings UI assumes at least one row to render the dropdown;
        // an empty PRESETS would leave the user with only "custom".
        assert!(!PRESETS.is_empty());
    }

    #[test]
    fn find_preset_returns_none_for_unknown_id() {
        assert!(find_preset("there-is-no-such-preset").is_none());
    }

    #[test]
    fn find_preset_returns_known_preset() {
        let p = find_preset(DEFAULT_PRESET_ID).expect("default preset must be findable");
        assert_eq!(p.id, DEFAULT_PRESET_ID);
    }

    #[test]
    fn gemma_4_e2b_preset_matches_hf_artifact() {
        let p = find_preset("gemma-4-e2b-it-qat-ud-q4-k-xl")
            .expect("Gemma 4 E2B QAT preset must be exposed");
        assert_eq!(p.display_name, "Gemma 4 E2B IT QAT (Q4_K_XL / Unsloth)");
        assert_eq!(p.hf_repo, "unsloth/gemma-4-E2B-it-qat-GGUF");
        assert_eq!(p.gguf_file, "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf");
        assert_eq!(p.recommended_ctx_size, 131_072);
        assert_eq!(p.thinking_kwarg, ThinkingKwarg::Enable);
        assert!(!p.mtp_enabled);
    }

    #[test]
    fn gemma_4_e4b_preset_matches_hf_text_config() {
        let p = find_preset("gemma-4-e4b-it-qat-ud-q4-k-xl")
            .expect("Gemma 4 E4B QAT preset must be exposed");
        assert_eq!(p.recommended_ctx_size, 131_072);
        assert_eq!(p.kv_layers, 42);
        assert_eq!(p.kv_embd_k_gqa, 512);
        assert_eq!(p.kv_embd_v_gqa, 512);
        assert!(!p.mtp_enabled);
    }

    #[test]
    fn gemma_4_e2b_qat_mtp_preset_matches_hf_artifact() {
        let p = find_preset("gemma-4-e2b-it-qat-mtp-ud-q4-k-xl")
            .expect("Gemma 4 E2B QAT MTP preset must be exposed");
        assert_eq!(p.display_name, "Gemma 4 E2B IT QAT MTP (Q4_K_XL / Unsloth)");
        assert_eq!(p.hf_repo, "unsloth/gemma-4-E2B-it-qat-GGUF");
        assert_eq!(p.gguf_file, "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf");
        assert_eq!(p.recommended_ctx_size, 131_072);
        assert_eq!(p.thinking_kwarg, ThinkingKwarg::Enable);
        assert!(p.mtp_enabled);
        assert_eq!(p.mtp_draft_model, Some("mtp-gemma-4-E2B-it.gguf"));
    }

    #[test]
    fn default_preset_id_pins_gemma_4_e2b_non_mtp() {
        assert_eq!(DEFAULT_PRESET_ID, "gemma-4-e2b-it-qat-ud-q4-k-xl");
        assert_eq!(default_preset().id, DEFAULT_PRESET_ID);
        assert_eq!(
            default_preset().gguf_file,
            "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf"
        );
        assert_eq!(default_preset().hf_repo, "unsloth/gemma-4-E2B-it-qat-GGUF");
        assert_eq!(default_preset().recommended_ctx_size, 131_072);
        assert_eq!(default_preset().thinking_kwarg, ThinkingKwarg::Enable);
        assert!(!default_preset().mtp_enabled);
        assert_eq!(default_preset().mtp_draft_model, None);
    }

    #[test]
    fn curated_presets_are_grouped_by_size_with_non_mtp_before_mtp() {
        // The first row is the default and the UI reads this list directly.
        // Keep paired non-MTP/MTP variants adjacent so users can compare them.
        let expected = [
            "gemma-4-e2b-it-qat-ud-q4-k-xl",
            "gemma-4-e2b-it-qat-mtp-ud-q4-k-xl",
            "gemma-4-e4b-it-qat-ud-q4-k-xl",
            "gemma-4-e4b-it-qat-mtp-ud-q4-k-xl",
            "gemma-4-12b-it-qat-ud-q4-k-xl",
            "gemma-4-12b-it-qat-mtp-ud-q4-k-xl",
            "gemma-4-26b-a4b-it-ud-iq4-nl",
            "qwen3-5-9b-ud-q4-k-xl",
            "qwen3-5-9b-mtp-ud-q4-k-xl",
            "qwen3-6-27b-ud-q4-k-xl",
            "qwen3-6-27b-mtp-ud-q4-k-xl",
            "qwen3-6-35b-a3b-ud-iq4-nl",
            "qwen3-6-35b-a3b-mtp-ud-q4-k-m",
            "qwen3-5-122b-a10b-ud-iq4-xs",
            "qwen3-5-122b-a10b-mtp-ud-q4-k-s",
        ];
        let actual: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn gemma_qat_non_mtp_and_mtp_pairs_share_target_and_only_mtp_has_draft() {
        let pairs = [
            (
                "gemma-4-e2b-it-qat-ud-q4-k-xl",
                "gemma-4-e2b-it-qat-mtp-ud-q4-k-xl",
                "mtp-gemma-4-E2B-it.gguf",
            ),
            (
                "gemma-4-e4b-it-qat-ud-q4-k-xl",
                "gemma-4-e4b-it-qat-mtp-ud-q4-k-xl",
                "mtp-gemma-4-E4B-it.gguf",
            ),
            (
                "gemma-4-12b-it-qat-ud-q4-k-xl",
                "gemma-4-12b-it-qat-mtp-ud-q4-k-xl",
                "mtp-gemma-4-12b-it.gguf",
            ),
        ];
        for (non_mtp_id, mtp_id, draft) in pairs {
            let non_mtp = find_preset(non_mtp_id).expect("non-MTP QAT preset must be exposed");
            let mtp = find_preset(mtp_id).expect("MTP QAT preset must be exposed");
            assert_eq!(non_mtp.hf_repo, mtp.hf_repo);
            assert_eq!(non_mtp.gguf_file, mtp.gguf_file);
            assert!(!non_mtp.mtp_enabled);
            assert_eq!(non_mtp.mtp_draft_model, None);
            assert!(mtp.mtp_enabled);
            assert_eq!(mtp.mtp_draft_model, Some(draft));
            assert!(!mtp.gguf_file.starts_with("mtp-"));
        }
    }

    #[test]
    fn qwen_mtp_preset_uses_same_file_mtp_without_draft_model() {
        let p = find_preset("qwen3-6-27b-mtp-ud-q4-k-xl")
            .expect("Qwen3.6 27B MTP preset must be exposed");
        assert_eq!(p.hf_repo, "unsloth/Qwen3.6-27B-MTP-GGUF");
        assert_eq!(p.gguf_file, "Qwen3.6-27B-UD-Q4_K_XL.gguf");
        assert!(p.mtp_enabled);
        assert_eq!(p.mtp_draft_model, None);
    }

    #[test]
    fn legacy_gemma_non_qat_presets_are_not_exposed() {
        for (retired_id, canonical_id) in [
            ("gemma-4-e2b-it-ud-q4-k-xl", "gemma-4-e2b-it-qat-ud-q4-k-xl"),
            ("gemma-4-e4b-it-ud-q4-k-xl", "gemma-4-e4b-it-qat-ud-q4-k-xl"),
            ("gemma-4-12b-it-ud-q4-k-xl", "gemma-4-12b-it-qat-ud-q4-k-xl"),
        ] {
            assert!(
                find_preset(retired_id).is_none(),
                "{retired_id} should not be exposed in the curated list"
            );
            assert_eq!(
                canonical_preset_id(retired_id),
                Some(canonical_id),
                "{retired_id} must migrate to its QAT replacement"
            );
            assert_eq!(
                find_preset_or_alias(retired_id).map(|p| p.id),
                Some(canonical_id)
            );
        }
    }

    #[test]
    fn default_preset_is_at_index_zero() {
        // `default_preset()` returns &PRESETS[0]; keep them aligned so a
        // future reorder of the list can't silently change the default.
        assert_eq!(PRESETS[0].id, DEFAULT_PRESET_ID);
    }

    #[test]
    fn all_preset_ids_are_unique() {
        // Duplicate ids would make `find_preset` non-deterministic depending
        // on iteration order, and the UI dropdown would render duplicate
        // options that disagree on settings.
        let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "preset ids must be unique");
    }

    #[test]
    fn thinking_kwarg_matches_model_family() {
        // The kwarg points OPPOSITE directions per family:
        //   - Qwen3 → Disable (`enable_thinking:false` suppresses the
        //     `<think>…</think>` block that swallows the tool call,
        //     QwenLM/Qwen3 #1817 + ggml-org/llama.cpp #20837).
        //   - Gemma 4 → Enable (`enable_thinking:true` suppresses the
        //     `<|channel>thought\n<channel|>` assistant prefix that
        //     would otherwise block the PEG-Gemma4 tool-call grammar
        //     from firing at position 0).
        //   - Anything else (or a custom preset id added later) → None.
        // The two-bool design we had before could not represent these
        // simultaneously and silently sent the wrong polarity.
        for p in PRESETS {
            let expected = if p.id.starts_with("qwen") {
                ThinkingKwarg::Disable
            } else if p.id.starts_with("gemma-4-") {
                ThinkingKwarg::Enable
            } else {
                ThinkingKwarg::None
            };
            assert_eq!(
                p.thinking_kwarg, expected,
                "preset {} thinking_kwarg mismatch",
                p.id
            );
        }
    }

    #[test]
    fn recommended_ctx_size_is_at_least_min_ctx_size() {
        // The Settings UI uses `recommended_ctx_size` as the placeholder
        // and `min_ctx_size` as the input lower bound; recommended < min
        // would render an out-of-range default.
        for p in PRESETS {
            assert!(
                p.recommended_ctx_size >= p.min_ctx_size,
                "preset {}: recommended ({}) < min ({})",
                p.id,
                p.recommended_ctx_size,
                p.min_ctx_size
            );
        }
    }

    #[test]
    fn q4_0_kv_cache_estimate_is_smaller_than_q8_0() {
        let p = default_preset();
        let q4 = estimate_kv_cache_ram_gb(p, p.recommended_ctx_size, KvCacheType::Q4_0);
        let q8 = estimate_kv_cache_ram_gb(p, p.recommended_ctx_size, KvCacheType::Q8_0);
        assert!(q4 < q8);
    }

    #[test]
    fn kv_cache_estimate_scales_with_ctx_size() {
        let p = default_preset();
        let small = estimate_kv_cache_ram_gb(p, 32_768, KvCacheType::Q4_0);
        let large = estimate_kv_cache_ram_gb(p, 262_144, KvCacheType::Q4_0);
        assert!((large / small - 8.0).abs() < 0.01);
    }

    #[test]
    fn total_ram_estimate_includes_model_and_kv_cache() {
        let p = default_preset();
        let total = estimate_total_ram_gb(p, 32_768, KvCacheType::Q4_0);
        assert!(total > p.estimated_model_ram_gb);
        assert!(total < p.estimated_ram_gb as f32);
    }

    #[test]
    fn coarse_ram_estimates_track_recommended_ctx_with_default_kv_cache() {
        // `estimated_ram_gb` is a coarse headline value. Keep it close to the
        // same model + KV-cache formula the Settings UI renders live; otherwise
        // a preset can look cheaper or more expensive than the detailed line.
        // 1GB allows rounding the float total to a friendly integer headline
        // without letting the two figures visibly disagree.
        const RAM_ESTIMATE_DRIFT_TOLERANCE_GB: f32 = 1.0;
        for p in PRESETS {
            let live = estimate_total_ram_gb(p, p.recommended_ctx_size, DEFAULT_KV_CACHE_TYPE);
            let drift = (p.estimated_ram_gb as f32 - live).abs();
            assert!(
                drift <= RAM_ESTIMATE_DRIFT_TOLERANCE_GB,
                "preset {}: estimated_ram_gb={} is too far from live total {:.1}",
                p.id,
                p.estimated_ram_gb,
                live
            );
        }
    }

    #[test]
    fn custom_preset_id_is_not_a_real_preset() {
        // The sentinel must not collide with a real preset id, otherwise
        // selecting "Custom" in the UI would route through `find_preset`
        // and use one of the curated rows instead of free-text fields.
        assert!(find_preset(CUSTOM_PRESET_ID).is_none());
    }

    #[test]
    fn list_llm_presets_returns_all_curated_entries() {
        // The Tauri command must mirror the static slice; serialisation
        // happens implicitly via `Serialize`. Compare ids to keep the
        // assertion stable when display strings change.
        let returned: Vec<&str> = list_llm_presets().into_iter().map(|p| p.id).collect();
        let expected: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(returned, expected);
    }

    #[test]
    fn gguf_file_extension_is_gguf() {
        // Defensive: a copy-paste error would surface here, not at first
        // download (which is minutes / GB later).
        for p in PRESETS {
            assert!(
                p.gguf_file.to_ascii_lowercase().ends_with(".gguf"),
                "preset {}: gguf_file does not end with .gguf",
                p.id
            );
        }
    }

    #[test]
    fn description_is_a_per_id_i18n_key() {
        // The frontend resolves `description` through `t()`, so the backend
        // must emit a stable i18n key (`settings.llmPreset.desc.<id>`), never a
        // localized string. A drift here would surface as a raw key in the UI;
        // the matching ja/en entries are pinned by the frontend dict test.
        for p in PRESETS {
            assert_eq!(
                p.description,
                format!("settings.llmPreset.desc.{}", p.id),
                "preset {}: description must be the i18n key for its id",
                p.id
            );
        }
    }

    #[test]
    fn hf_repo_is_org_slash_name_shape() {
        // The custom validation also enforces this shape; presets must
        // satisfy the same contract so the UI handles preset and custom
        // identically when surfacing the repo.
        for p in PRESETS {
            let parts: Vec<&str> = p.hf_repo.split('/').collect();
            assert_eq!(
                parts.len(),
                2,
                "preset {}: hf_repo {:?} must be `org/name`",
                p.id,
                p.hf_repo
            );
            assert!(
                !parts[0].is_empty() && !parts[1].is_empty(),
                "preset {}: hf_repo halves must be non-empty",
                p.id
            );
        }
    }
}

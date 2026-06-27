//! Curated, version-locked embedding model presets surfaced in the Settings UI.
//! Mirrors [`super::llm_presets`] in shape so the frontend can render both with
//! the same dropdown + custom-entry pattern. Custom (free-text) is the escape
//! hatch; it lets the user point at any HuggingFace repo at the cost of having
//! to specify `vector_size`, `max_sequence_length`, and `is_multimodal` by hand.

use serde::Serialize;

/// A single curated embedding preset exposed in Settings.
///
/// `vector_size` MUST match the model's actual output dimension — the memories
/// sidecar reads `MEMORY_VECTOR_SIZE` at startup and refuses to open a LanceDB
/// whose `embedding_dim` disagrees. A wrong value here would corrupt the
/// rollout (memories panics on dim mismatch).
///
/// `is_multimodal` gates the runtime image-search path: a `false` preset can
/// only embed text, so `embed_image` queries return no hits. The UI conveys
/// this through the preset's `display_name` ("text-only" / "multimodal")
/// rather than reading this flag directly.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EmbeddingPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub hf_repo: &'static str,
    /// Optional separate tokenizer repo. `None` means the runner reuses
    /// `hf_repo` as both the weights and tokenizer source — Qwen / Ruri
    /// publish them together so the second field is unset for every
    /// preset we ship. `Some(_)` becomes a `tokenizer_model_id` line in
    /// the staged worker YAML.
    pub tokenizer_hf_repo: Option<&'static str>,
    pub vector_size: u32,
    /// Stored as a string so the YAML loader receives the canonical enum
    /// name verbatim. The runner accepts `F16` / `BF16` / `F32`.
    pub dtype: &'static str,
    pub max_sequence_length: u32,
    /// `true` ⇒ the model jointly embeds text and images. `false` ⇒
    /// text-only; image search must be disabled (see module docs).
    pub is_multimodal: bool,
    pub estimated_ram_gb: u32,
    /// i18n key (not a localized string) the frontend resolves via `t()`.
    /// Backend stays locale-agnostic so language switching is instant; the
    /// translations live in the bundled `i18n/locales/{ja,en}.json`.
    pub description: &'static str,
}

/// Identifier of the default preset. Any reordering of `PRESETS` MUST keep
/// this id at index 0 because `default_preset()` returns the first row.
pub const DEFAULT_EMBEDDING_PRESET_ID: &str = "qwen3-embedding-0-6b";

/// Sentinel id for "free-text custom entry" in the Settings UI. Mirror of
/// [`super::llm_presets::CUSTOM_PRESET_ID`].
pub const CUSTOM_EMBEDDING_PRESET_ID: &str = "custom";

/// Curated, tested embedding model list. Index 0 is the default — see
/// [`DEFAULT_EMBEDDING_PRESET_ID`].
pub const PRESETS: &[EmbeddingPreset] = &[
    EmbeddingPreset {
        id: DEFAULT_EMBEDDING_PRESET_ID,
        display_name: "Qwen3-Embedding 0.6B (text-only, 1024 dim)",
        hf_repo: "Qwen/Qwen3-Embedding-0.6B",
        tokenizer_hf_repo: None,
        vector_size: 1024,
        dtype: "F16",
        max_sequence_length: 32_768,
        is_multimodal: false,
        estimated_ram_gb: 2,
        description: "settings.embeddingPreset.desc.qwen3-embedding-0-6b",
    },
    EmbeddingPreset {
        id: "qwen3-vl-embedding-2b",
        display_name: "Qwen3-VL-Embedding 2B (multimodal, 2048 dim)",
        hf_repo: "Qwen/Qwen3-VL-Embedding-2B",
        tokenizer_hf_repo: None,
        vector_size: 2048,
        dtype: "F16",
        max_sequence_length: 8192,
        is_multimodal: true,
        estimated_ram_gb: 6,
        description: "settings.embeddingPreset.desc.qwen3-vl-embedding-2b",
    },
    EmbeddingPreset {
        id: "qwen3-embedding-4b",
        display_name: "Qwen3-Embedding 4B (text-only, 2560 dim)",
        hf_repo: "Qwen/Qwen3-Embedding-4B",
        tokenizer_hf_repo: None,
        vector_size: 2560,
        dtype: "F16",
        max_sequence_length: 40_960,
        is_multimodal: false,
        estimated_ram_gb: 10,
        description: "settings.embeddingPreset.desc.qwen3-embedding-4b",
    },
    // NOTE: cl-nagoya/ruri-v3-310m (ModernBert) is intentionally NOT listed:
    // the MultimodalEmbeddingRunner currently supports only Qwen3-VL and
    // Qwen3 text-only checkpoints (architectures[0] branching). Adding ruri
    // back to this list requires ModernBert support in the runner first.
];

pub fn find_preset(id: &str) -> Option<&'static EmbeddingPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

pub fn default_preset() -> &'static EmbeddingPreset {
    &PRESETS[0]
}

#[tauri::command]
pub fn list_embedding_presets() -> Vec<EmbeddingPreset> {
    PRESETS.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_nonempty() {
        // The Settings UI assumes at least one row to render the dropdown; an
        // empty PRESETS would leave the user with only "custom".
        assert!(!PRESETS.is_empty());
    }

    #[test]
    fn find_preset_returns_none_for_unknown_id() {
        assert!(find_preset("there-is-no-such-preset").is_none());
    }

    #[test]
    fn find_preset_returns_known_preset() {
        let p = find_preset(DEFAULT_EMBEDDING_PRESET_ID).expect("default preset must be findable");
        assert_eq!(p.id, DEFAULT_EMBEDDING_PRESET_ID);
    }

    #[test]
    fn default_preset_id_pins_qwen3_embedding_0_6b() {
        assert_eq!(DEFAULT_EMBEDDING_PRESET_ID, "qwen3-embedding-0-6b");
        assert_eq!(default_preset().id, DEFAULT_EMBEDDING_PRESET_ID);
        assert_eq!(default_preset().hf_repo, "Qwen/Qwen3-Embedding-0.6B");
        assert_eq!(default_preset().vector_size, 1024);
        assert!(!default_preset().is_multimodal);
    }

    #[test]
    fn default_preset_is_at_index_zero() {
        assert_eq!(PRESETS[0].id, DEFAULT_EMBEDDING_PRESET_ID);
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
    fn custom_preset_id_is_not_a_real_preset() {
        // The sentinel must not collide with a real preset id, otherwise
        // selecting "Custom" in the UI would route through `find_preset` and
        // use one of the curated rows instead of free-text fields.
        assert!(find_preset(CUSTOM_EMBEDDING_PRESET_ID).is_none());
    }

    #[test]
    fn list_embedding_presets_returns_all_curated_entries() {
        let returned: Vec<&str> = list_embedding_presets().into_iter().map(|p| p.id).collect();
        let expected: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(returned, expected);
    }

    #[test]
    fn description_is_a_per_id_i18n_key() {
        // The frontend resolves `description` through `t()`, so the backend
        // must emit a stable i18n key (`settings.embeddingPreset.desc.<id>`),
        // never a localized string. The matching ja/en entries are pinned by
        // the frontend dict test.
        for p in PRESETS {
            assert_eq!(
                p.description,
                format!("settings.embeddingPreset.desc.{}", p.id),
                "preset {}: description must be the i18n key for its id",
                p.id
            );
        }
    }

    #[test]
    fn hf_repo_is_org_slash_name_shape() {
        // The custom validation enforces this shape; presets must satisfy the
        // same contract so the UI handles preset and custom identically when
        // surfacing the repo.
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

    #[test]
    fn vector_size_is_positive_and_within_range() {
        // Defensive: 0 would crash memories on LanceDB open; >8192 is beyond
        // anything reasonable for text/multimodal embedding models.
        for p in PRESETS {
            assert!(
                (1..=8192).contains(&p.vector_size),
                "preset {}: vector_size {} out of [1, 8192]",
                p.id,
                p.vector_size
            );
        }
    }

    #[test]
    fn dtype_is_supported_value() {
        // The runner only accepts these three; a typo here would surface at
        // sidecar startup as `UnsupportedDType`.
        for p in PRESETS {
            assert!(
                matches!(p.dtype, "F16" | "BF16" | "F32"),
                "preset {}: unsupported dtype {}",
                p.id,
                p.dtype
            );
        }
    }

    #[test]
    fn max_sequence_length_is_positive() {
        for p in PRESETS {
            assert!(
                p.max_sequence_length > 0,
                "preset {}: max_sequence_length is zero",
                p.id
            );
        }
    }

    #[test]
    fn only_qwen3_vl_preset_is_multimodal() {
        // Pin the truth-table: VL-2B is the one image-capable preset, so a
        // future addition that flips this assumption forces a deliberate update
        // to the image-search gating.
        for p in PRESETS {
            let expected = p.id == "qwen3-vl-embedding-2b";
            assert_eq!(
                p.is_multimodal, expected,
                "preset {}: is_multimodal {} mismatches expected {}",
                p.id, p.is_multimodal, expected
            );
        }
    }
}

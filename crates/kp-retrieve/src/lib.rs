//! Viewer-scoped hybrid retrieval, bounded SAG2 expansion, and opt-in evidence judges.

mod candidate_score;
mod citations;
mod diversify;
mod engine;
mod expansion;
pub mod guard;
pub mod judge;
pub mod prompt_budget;
mod redactor;

pub use citations::sanitize_citations;
pub use engine::{
    merge_federated_results, search, search_shallow, search_shallow_with_embedder,
    search_with_embedder, SearchOptions, SearchOutput, SearchResult, Trace,
};
pub use guard::{
    evaluate_tool_guards, extract_tool_targets, is_guarded_tool, split_shell_command_segments,
    GuardCheckInput, GuardHit, GuardVerdict, ToolTargets,
};
pub use kp_store::providers::{
    self, build_embedder_for_spec, default_global_config_path, embedding_recipes,
    find_embedding_recipe, load_global_config, load_global_config_from_path,
    parse_embedding_model_spec, resolve_init_embedding_choice, save_global_config_0600,
    DetectedEmbeddingProvider, EmbeddingRecipe, EmbeddingTouchpoint, InitEmbeddingChoice,
    KpGlobalConfig, ProviderEmbedder, ProviderEmbedderConfig, ProviderTestReport,
    ResolvedEmbeddingSpec, DEFAULT_LOCAL_EMBEDDING_DIMS, DEFAULT_LOCAL_EMBEDDING_MODEL,
    EMBEDDING_RECIPES,
};
pub use kp_store::{
    sanitize_and_validate_embedding, DisabledModelEmbedder, Embedder, FtsOnlyEmbedder, HashEmbedder,
};
#[cfg(feature = "local-embed")]
pub use kp_store::{DownloadProgress, LocalOnnxEmbedder};
pub use prompt_budget::{
    estimate_tokens, format_citation_header, format_citation_line, format_fact_kp_ref,
    format_prompt_candidates, is_low_signal_ack, retrieve_prompt_context,
    retrieve_prompt_context_with_embedder, strip_turn_tags_and_assistant_lines,
    FormattedPromptContext, FormattedPromptItem, PromptBudgetConfig, PromptCandidate,
    PromptPremise, DEFAULT_MIN_RELEVANCE_SCORE, MAX_PROMPT_CONTEXT_ITEMS,
    MAX_PROMPT_CONTEXT_TOKENS,
};
pub use redactor::SecretRedactor;

#[derive(Debug, thiserror::Error)]
pub enum RetrieveError {
    #[error(transparent)]
    Store(#[from] kp_store::StoreError),
    #[error("invalid retrieval configuration: {0}")]
    Config(&'static str),
    #[error("judge transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("judge returned HTTP {0}")]
    Http(u16),
    #[error("invalid judge response: {0}")]
    Protocol(&'static str),
    #[error("invalid text pattern")]
    Pattern(#[from] regex::Error),
}

pub type Result<T> = std::result::Result<T, RetrieveError>;

#[cfg(test)]
mod test_support;

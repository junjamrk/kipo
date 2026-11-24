//! The shared personal/community SQLite engine and raw-blob repository.

mod anchors;
mod audit_log;
mod db;
mod embedding;
mod episodes;
mod error;
mod exchange;
mod export;
mod facts;
mod governance;
mod inspector;
mod lexical;
#[cfg(feature = "local-embed")]
pub mod local_onnx;
mod maintenance;
mod migrations;
mod ops;
pub mod providers;
mod public_ingest;
mod queue;
mod reembed;
mod rsi;
mod skills;
mod slots;
mod snapshot;
mod tokenizer;
mod tombstone;
mod writeback;

#[cfg(test)]
mod m2_foundations_tests;
#[cfg(test)]
mod m3_writeback_audit_tests;
#[cfg(test)]
mod test_support;

pub use anchors::{CodeAnchor, CodeLocus};
pub use audit_log::ChangeLogEntry;
pub use db::{InitOptions, LibraryStatus, MemberToken, Store};
pub use episodes::{AnchoredSourceChunk, Episode, PendingEpisode, UnminedDreamSource};
pub use error::{Result, StoreError};
pub use export::ExportReport;
pub use governance::{
    is_ambiguous_scope_review_supported, is_open_review_currently_valid, new_concept_id,
    new_fact_id, new_job_id, new_justification_id, new_rejection_id, new_review_id, new_source_id,
    normalize_concept_label, ExtractionSourceContext, GuardAllowGrant, RejectOptions,
    ScopeReviewFactUpdates,
};
pub use inspector::{InspectorFactCard, InspectorQuoteHighlight, InspectorTimelineEntry};
pub use migrations::schema_ir_hash;
pub use ops::{ChunkHit, ChunkInput, RawSourceInput, SessionSubbatchSpec, StoredSource};
pub use providers::{
    build_embedder_for_spec, default_global_config_path, diagnose_embedder_spec_issue,
    diagnose_embedder_spec_issue_with_lookup, embedding_recipes, find_embedding_recipe,
    load_global_config, load_global_config_from_path, parse_embedding_model_spec,
    resolve_init_embedding_choice, save_global_config_0600, try_build_embedder_with_reason,
    validate_provider_base_url, DetectedEmbeddingProvider, EmbeddingRecipe, EmbeddingTouchpoint,
    InitEmbeddingChoice, KpGlobalConfig, ProviderEmbedder, ProviderEmbedderConfig,
    ProviderTestReport, ResolvedEmbeddingSpec, DEFAULT_LOCAL_EMBEDDING_DIMS,
    DEFAULT_LOCAL_EMBEDDING_MODEL, EMBEDDING_RECIPES,
};
pub use public_ingest::{
    decode_queue_model_policy, PublicIngestStreamReport, PublicRecordAppliedKind,
    PublicRecordApplyOutcome, PublicSessionSubbatchSpec,
};
pub use queue::{
    ExtractionJob, ExtractionJobInput, ExtractionJobStatus, FencedJobCommitSpec, SessionCursor,
    StagedAtomWrite, StagedExtractionBatch,
};
pub use reembed::ReembedReport;
pub use rsi::{AppliedEpisode, FeedbackResult};
pub use skills::ActiveSkill;
pub use tokenizer::Tokenizer;
pub use tombstone::{
    count_chunks_fts_matches, count_orphaned_chunk_vectors, count_self_echo_filtered_sources,
    find_sqlite_table_containing_bytes, outbox_payload_by_idempotency_key, tombstone_id,
};
pub use writeback::{ApplyReport, WritebackFactUpdate, WritebackProposalOp};

pub use embedding::{
    sanitize_and_validate_embedding, validate_vector, DisabledModelEmbedder, Embedder,
    FtsOnlyEmbedder, HashEmbedder, MIN_NON_DEGENERATE_NORM,
};
pub use lexical::ScopedIndex;
#[cfg(feature = "local-embed")]
pub use local_onnx::{
    compute_wheel_dylib_entry_sha256, current_platform_ort_wheel, extract_ort_dylib_from_wheel,
    find_local_onnx_spec, resolve_hf_endpoint, resolve_models_dir, resolve_ort_dylib_path,
    verify_explicit_ort_dylib, verify_managed_ort_dylib, DownloadProgress, LocalOnnxEmbedder,
    LocalOnnxModelSpec, ModelFileSpec, OrtWheelSpec, LOCAL_ONNX_MODELS, ORT_WHEEL_SPECS,
};
pub use slots::{FactInput, Remembered};
pub use snapshot::{
    strip_turn_tags_and_assistant_lines, PromptDocMeta, PromptPremise, RetrievalSnapshot,
    VisibleDocument, VisibleGuardCandidate, VisibleGuardPremise,
};

pub use exchange::{
    sha256_file_prefix, AcceptInitialPublishSpec, ChangeSnapshot, FinalizePublishSpec,
    InitialPublishWrite, OutboxEntry, PublishEnqueueSpec, SessionCursorUpdate, Tombstone,
    UnextractedSessionCursor,
};

/// Generates a 256-bit (64 lowercase hex characters) cryptographically random capability token
/// using OS CSPRNG entropy (`uuid::Uuid::new_v4`) hashed through SHA-256 for uniform bit
/// distribution across Unix and Windows without reading `/dev/urandom` directly.
pub fn random_256bit_hex_token() -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for _ in 0..3 {
        hasher.update(uuid::Uuid::new_v4().as_bytes());
    }
    hex::encode(hasher.finalize())
}

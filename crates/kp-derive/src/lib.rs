//! Deterministic Extract → Define → Canonicalize funnel with provenance-preserving storage.

pub mod collision;
mod extract;
mod funnel;
pub mod http_llm;
pub mod llm_runner;
pub mod nl_funnel;
pub mod prompts;

pub use collision::{
    align_concepts_and_derive, evaluate_ambiguous_scope_reviews, fact_route_policy,
    reconcile_contradicted_events, record_manual_concept_alignment, resolve_ambiguous_scope_review,
    CollisionDeriveOutcome, ConceptAlignmentTable, FactAlignmentKey, ScopeResolutionOutcome,
    ScopeReviewDecision, CONCEPT_ALIGNMENT_PREDICATE, CONCEPT_LABEL_BINDING_V2,
};
pub use extract::{parse_bracket_command_guard_tags, ExtractedFact, Extractor, RuleExtractor};
pub use funnel::{
    add_markdown, base_fact, default_auto_route_policy, markdown_chunks, remember, FunnelOptions,
    FunnelResult, Memory,
};
pub use http_llm::{
    build_named_endpoint_llm_client, detect_global_generative_model, is_cli_auth_or_spawn_error,
    is_quota_exhausted_error, load_secure_kp_config, probe_claude_logged_in, probe_codex_logged_in,
    resolve_named_endpoint_config, validate_gateway_base_url, DetectedGenerativeModel,
    DetectedHostCli, FallbackLlmClient, GenerativeApiProvider, HttpApiConfig, HttpApiLlmClient,
    HttpApiWireKind, HttpJsonRequest, HttpJsonResponse, HttpJsonTransport, KpGlobalModelConfig,
    ModelDetectEnv, NamedEndpointConfig, RawOnlyLlmClient, ReqwestHttpTransport,
};
pub use llm_runner::{
    detect_cli_auth_failure, detect_low_quota, extract_codex_provider_overrides,
    sanitize_child_env, HostCliConfig, HostCliKind, HostCliRunner, PolicyRoutedLlmClient,
    RecordedInvocation, RecordingMockTransport, StdSubprocessTransport, SubprocessRawOutput,
    SubprocessSpec, SubprocessTransport, INTERNAL_EXTRACTION_ENV,
};
pub use nl_funnel::{
    alloc_opaque_concept_id, alloc_opaque_fact_id, alloc_opaque_rejection_id,
    alloc_opaque_review_id, apply_extracted_atoms, build_extraction_batch_request,
    build_extraction_batch_request_for_policy, build_token_budgeted_chunk_batches,
    build_token_budgeted_turn_batches, extract_and_ingest_batch, reject_fact_and_record,
    resolve_actor_mention, stage_extracted_atoms, DroppedRejectedCandidate,
    ExtractionBatchBuildInput, NlBatchStatus, NlFunnelOptions, NlFunnelOutcome, RejectedModelAtom,
    TokenBatchSpec,
};
pub use prompts::{
    build_command_guard_backfill_prompt, build_entailment_prompt, build_extraction_prompt,
    command_guard_backfill_json_schema, entailment_verdict_json_schema,
    extraction_batch_json_schema, is_chunk_trivial_or_ack_only, is_near_duplicate,
    is_trivial_turn_text, jaccard_3gram, normalize_for_dedup, parse_command_guard_backfill_reply,
    parse_entailment_verdict, parse_extraction_envelope, shannon_entropy, shingles_3gram,
    MinHashSignature, ModelExtractionEnvelope, DEFAULT_AMBIGUOUS_QUESTION, JACCARD_DEDUP_THRESHOLD,
    SKIP_SUMMARY_SENTINEL,
};

mod rules;
mod synthesis;
pub mod tms;
pub mod why;
pub use rules::{
    derive, derive_with_rules, verify_hard_rules_match_full_recompute, verify_with_rules, Atom,
    DeriveResult, HornRule, Term,
};
pub use synthesis::*;

CREATE TABLE library_meta (
    library_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    mode TEXT NOT NULL CHECK(mode IN ('personal','community')),
    owner_actor TEXT NOT NULL,
    schema_ir_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    embedding_model TEXT NOT NULL DEFAULT 'hash:v1',
    embedding_dims INTEGER NOT NULL DEFAULT 256 CHECK(embedding_dims >= 0)
);
CREATE TABLE members_tokens (
    actor_id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('owner','maintainer','member')),
    token_sha256 TEXT UNIQUE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE tombstones (
    tombstone_id TEXT PRIMARY KEY NOT NULL,
    origin_library_id TEXT NOT NULL,
    purged_at INTEGER NOT NULL,
    reason TEXT NOT NULL
);
CREATE TABLE source_scopes (
    scope_id TEXT PRIMARY KEY NOT NULL,
    provider TEXT NOT NULL,
    external_container_id TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    current_audience_json TEXT NOT NULL,
    synced_at INTEGER NOT NULL,
    UNIQUE(provider, external_container_id)
);
CREATE TABLE raw_blobs (
    content_sha256 TEXT PRIMARY KEY NOT NULL,
    byte_len INTEGER NOT NULL CHECK(byte_len >= 0),
    content_blob BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE raw_sources (
    source_id TEXT PRIMARY KEY NOT NULL,
    uri TEXT NOT NULL,
    media_type TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    git_blob_or_commit TEXT NOT NULL,
    byte_len INTEGER NOT NULL CHECK(byte_len >= 0),
    visibility_scope TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    self_echo_filtered INTEGER NOT NULL DEFAULT 0 CHECK(self_echo_filtered IN (0,1)),
    ingested_by_actor TEXT,
    source_scope_id TEXT,
    external_source_key TEXT,
    superseded_by_source TEXT
);
CREATE INDEX raw_sources_ext_key ON raw_sources(external_source_key) WHERE external_source_key IS NOT NULL;
CREATE TABLE chunks (
    fts_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id TEXT NOT NULL UNIQUE,
    source_id TEXT NOT NULL REFERENCES raw_sources(source_id) ON DELETE CASCADE,
    source_sha256 TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    heading TEXT NOT NULL,
    body TEXT NOT NULL,
    segmented_body TEXT NOT NULL,
    anchor_json TEXT NOT NULL,
    readable_by_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(source_id, ordinal)
);
CREATE VIRTUAL TABLE chunks_fts USING fts5(title, heading, segmented_body, content='', contentless_delete=1);
CREATE TRIGGER chunks_delete_fts AFTER DELETE ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.fts_rowid;
END;
CREATE TABLE chunk_vectors (
    chunk_id TEXT PRIMARY KEY NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    dims INTEGER NOT NULL CHECK(dims > 0),
    vec_blob BLOB NOT NULL CHECK(length(vec_blob) = dims * 4),
    model_id TEXT NOT NULL DEFAULT 'hash:v1'
);
CREATE TABLE schema_slots (
    predicate TEXT PRIMARY KEY NOT NULL,
    namespace TEXT NOT NULL,
    cardinality TEXT NOT NULL CHECK(cardinality IN ('single','multi')),
    mutability TEXT NOT NULL CHECK(mutability IN ('fixed','time_varying')),
    value_kind TEXT NOT NULL,
    description TEXT NOT NULL,
    instruction TEXT NOT NULL
);
CREATE TABLE facts (
    fts_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id TEXT NOT NULL UNIQUE,
    origin_library_id TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('observation','fact_slot','event','constraint','decision','membership','closed_negative','alias','derived_fact','collision_warning','conflict','synthesis','playbook_bullet','skill_version')),
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object_json TEXT NOT NULL,
    tags_json TEXT NOT NULL DEFAULT '[]',
    author_actor TEXT NOT NULL,
    ingested_by_actor TEXT,
    trust_tier INTEGER NOT NULL CHECK(trust_tier BETWEEN 0 AND 3),
    audience_json TEXT NOT NULL,
    source_scope_id TEXT,
    governed_scope_json TEXT,
    broader_scope_json TEXT,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL CHECK(valid_until > valid_from),
    recorded_at INTEGER NOT NULL,
    expired_at INTEGER,
    status TEXT NOT NULL CHECK(status IN ('active','superseded','withdrawn','invalidated','stale_premise','quarantined','unreachable_hidden')),
    superseded_by TEXT,
    rank TEXT NOT NULL DEFAULT 'normal' CHECK(rank IN ('preferred','normal','deprecated')),
    source_anchor_json TEXT,
    unverified_gaps_json TEXT NOT NULL DEFAULT '[]',
    miss_count INTEGER NOT NULL DEFAULT 0 CHECK(miss_count >= 0)
);
CREATE INDEX facts_identity ON facts(origin_library_id, logical_key);
CREATE INDEX facts_slot ON facts(subject, predicate, status);
CREATE INDEX facts_source ON facts(json_extract(source_anchor_json, '$.source_id'));
CREATE INDEX facts_source_scope ON facts(source_scope_id);
CREATE VIRTUAL TABLE facts_fts USING fts5(subject, predicate, segmented_object, content='', contentless_delete=1);
CREATE TRIGGER facts_delete_fts AFTER DELETE ON facts BEGIN
    DELETE FROM facts_fts WHERE rowid = old.fts_rowid;
END;
CREATE TABLE fact_vectors (
    fact_id TEXT PRIMARY KEY NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    dims INTEGER NOT NULL CHECK(dims > 0),
    vec_blob BLOB NOT NULL CHECK(length(vec_blob) = dims * 4),
    model_id TEXT NOT NULL DEFAULT 'hash:v1'
);
CREATE TABLE fact_entities (
    fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    entity_slug TEXT NOT NULL,
    role TEXT NOT NULL,
    PRIMARY KEY(fact_id, entity_slug, role)
);
CREATE INDEX fact_entities_entity ON fact_entities(entity_slug, fact_id);
CREATE TABLE justifications (
    just_id TEXT PRIMARY KEY NOT NULL,
    conclusion_fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    derivation_mode TEXT NOT NULL CHECK(derivation_mode IN ('hard_rule','scoped_synthesis','rsi_attribution','assumed_premise')),
    rule_id TEXT NOT NULL,
    rule_version TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
    created_at INTEGER NOT NULL,
    invalidated_at INTEGER,
    invalidation_reason TEXT
);
CREATE INDEX justifications_conclusion ON justifications(conclusion_fact_id, active);
CREATE TABLE justification_premises (
    just_id TEXT NOT NULL REFERENCES justifications(just_id) ON DELETE CASCADE,
    premise_fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE RESTRICT,
    premise_origin_lib TEXT NOT NULL,
    premise_logical_key TEXT NOT NULL,
    premise_sha256 TEXT NOT NULL,
    PRIMARY KEY(just_id, premise_fact_id)
);
CREATE INDEX premises_fact ON justification_premises(premise_fact_id);
CREATE INDEX premises_identity ON justification_premises(premise_origin_lib, premise_logical_key);
CREATE TABLE code_anchors (
    anchor_id TEXT PRIMARY KEY NOT NULL,
    fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    repo_root TEXT,
    commit_sha TEXT,
    file_path TEXT,
    language TEXT,
    symbol_name TEXT,
    sig_hash TEXT,
    body_hash TEXT,
    start_line INTEGER,
    end_line INTEGER,
    verify_status TEXT CHECK(verify_status IN ('verified','moved_unchanged','body_modified','sig_changed','orphaned')),
    verified_at INTEGER
);
CREATE INDEX code_anchors_fact ON code_anchors(fact_id);
CREATE TABLE rsi_episodes (
    episode_id TEXT PRIMARY KEY NOT NULL,
    signal_kind TEXT NOT NULL CHECK(signal_kind IN ('human_correction','failure_to_recovery','cross_avatar_contrast')),
    session_id TEXT,
    before_trace_json TEXT,
    after_trace_json TEXT,
    step_ref TEXT,
    environment_json TEXT,
    processed INTEGER NOT NULL DEFAULT 0 CHECK(processed IN (0,1)),
    created_at INTEGER NOT NULL
);
CREATE TABLE rsi_episode_facts (
    episode_id TEXT NOT NULL REFERENCES rsi_episodes(episode_id) ON DELETE CASCADE,
    fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    PRIMARY KEY(episode_id, fact_id)
);
CREATE TABLE rsi_feedback (
    feedback_id TEXT PRIMARY KEY NOT NULL,
    skill_fact_id TEXT NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    episode_or_task_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    model_family TEXT,
    outcome TEXT NOT NULL CHECK(outcome IN ('helped','hurt','neutral')),
    created_at INTEGER NOT NULL
);
CREATE INDEX rsi_feedback_skill ON rsi_feedback(skill_fact_id);
CREATE UNIQUE INDEX rsi_feedback_task ON rsi_feedback(skill_fact_id, actor_id, episode_or_task_id);
CREATE TABLE skill_approvals (
    fact_id TEXT PRIMARY KEY NOT NULL REFERENCES facts(fact_id) ON DELETE CASCADE,
    content_sha256 TEXT NOT NULL,
    exec_approved INTEGER NOT NULL DEFAULT 0 CHECK(exec_approved IN (0,1)),
    approved_by TEXT NOT NULL,
    approved_at INTEGER NOT NULL
);
CREATE TABLE session_cursors (
    session_id TEXT PRIMARY KEY NOT NULL,
    client_kind TEXT NOT NULL DEFAULT 'unknown',
    transcript_path TEXT,
    last_byte_offset INTEGER NOT NULL DEFAULT 0 CHECK(last_byte_offset >= 0),
    last_line_count INTEGER NOT NULL DEFAULT 0 CHECK(last_line_count >= 0),
    prefix_sha256 TEXT NOT NULL DEFAULT '',
    last_activity_at INTEGER NOT NULL,
    last_extracted_offset INTEGER NOT NULL DEFAULT 0 CHECK(last_extracted_offset >= 0)
);
CREATE TABLE extraction_queue (
    job_id TEXT PRIMARY KEY NOT NULL,
    source_id TEXT NOT NULL,
    chunk_ids_json TEXT NOT NULL,
    chunk_batch_sha256 TEXT NOT NULL,
    author_actor TEXT NOT NULL,
    model_policy TEXT NOT NULL DEFAULT 'host',
    priority INTEGER NOT NULL DEFAULT 0,
    reference_time INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','leased','paused_quota','done','failed_permanent')),
    lease_until INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    thread_key TEXT,
    reply_to_key TEXT
);
CREATE INDEX extraction_queue_sched ON extraction_queue(status, priority DESC, reference_time DESC);
CREATE INDEX extraction_queue_thread ON extraction_queue(thread_key, reference_time ASC, created_at ASC) WHERE thread_key IS NOT NULL;
CREATE TABLE extracted_chunks (
    chunk_sha256 TEXT NOT NULL,
    author_actor TEXT NOT NULL,
    schema_ver TEXT NOT NULL,
    extracted_at INTEGER NOT NULL,
    PRIMARY KEY(chunk_sha256, author_actor, schema_ver)
);
CREATE TABLE rejected_claims (
    rejection_id TEXT PRIMARY KEY NOT NULL,
    rejected_by_actor TEXT NOT NULL,
    speaker_actor TEXT,
    chunk_sha256 TEXT,
    char_start INTEGER,
    char_end INTEGER CHECK(char_end IS NULL OR char_end > char_start),
    semantic_fingerprint TEXT,
    semantic_key_json TEXT,
    replacement_fact_id TEXT,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    CHECK(chunk_sha256 IS NOT NULL OR semantic_fingerprint IS NOT NULL)
);
CREATE INDEX rejected_claims_evidence ON rejected_claims(speaker_actor, chunk_sha256);
CREATE INDEX rejected_claims_semantic ON rejected_claims(rejected_by_actor, semantic_fingerprint);
CREATE TABLE review_items (
    review_id TEXT PRIMARY KEY NOT NULL,
    actor_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('ambiguous_scope','concept_alignment','identity_mapping','writeback_proposal')),
    target_id TEXT NOT NULL,
    question TEXT NOT NULL,
    options_json TEXT NOT NULL DEFAULT '[]',
    payload_json TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open','confirmed','dismissed')),
    created_at INTEGER NOT NULL,
    resolved_at INTEGER
);
CREATE INDEX review_items_open ON review_items(actor_id, status, created_at DESC);
CREATE TABLE community_concepts (
    concept_id TEXT PRIMARY KEY NOT NULL,
    concept_kind TEXT NOT NULL CHECK(concept_kind IN ('condition','action','group','topic')),
    canonical_label TEXT NOT NULL,
    domain TEXT NOT NULL DEFAULT '',
    polarity TEXT NOT NULL DEFAULT 'neutral',
    proposed_by TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','deprecated')),
    created_at INTEGER NOT NULL
);
CREATE TABLE external_identities (
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    proposed_actor_id TEXT,
    confirmed_actor_id TEXT,
    status TEXT NOT NULL DEFAULT 'proposed' CHECK(status IN ('proposed','confirmed','rejected')),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(provider, external_id)
);
CREATE TABLE projection_pages (
    page_slug TEXT NOT NULL,
    viewer_actor TEXT NOT NULL,
    base_seq INTEGER NOT NULL,
    page_sha256 TEXT NOT NULL,
    rendered_at INTEGER NOT NULL,
    PRIMARY KEY(page_slug, viewer_actor)
);
CREATE TABLE change_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    op TEXT NOT NULL,
    target_id TEXT,
    origin_library_id TEXT,
    logical_key TEXT,
    content_sha256 TEXT,
    actor_id TEXT,
    payload_json TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX change_log_target ON change_log(target_id);
CREATE INDEX change_log_identity ON change_log(origin_library_id, logical_key);
CREATE TABLE outbox (
    outbox_id INTEGER PRIMARY KEY AUTOINCREMENT,
    dest_community_url TEXT NOT NULL,
    event_kind TEXT NOT NULL CHECK(event_kind IN ('publish','withdraw','purge')),
    idempotency_key TEXT UNIQUE NOT NULL,
    payload_json TEXT NOT NULL,
    delivered_at INTEGER,
    retry_count INTEGER NOT NULL DEFAULT 0 CHECK(retry_count >= 0)
);
CREATE INDEX outbox_pending ON outbox(outbox_id) WHERE delivered_at IS NULL;

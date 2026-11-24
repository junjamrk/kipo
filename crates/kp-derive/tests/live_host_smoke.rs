//! Real-host CLI smoke & benchmark tests (`KP_LIVE_HOST=codex` or `KP_LIVE_HOST=claude`)
//! exercising 9 natural Chinese utterances across reading club, gym, travel reimbursement,
//! and multi-speaker chat, plus `low` vs default reasoning effort & 20/50-turn token-budgeted batches.
//!
//! Run outside sandbox with:
//! `KP_LIVE_HOST=codex cargo test -p kp-derive --test live_host_smoke --offline -- --ignored --nocapture`

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kp_derive::{
    apply_extracted_atoms, build_extraction_batch_request, build_token_budgeted_turn_batches,
    extract_and_ingest_batch, HostCliConfig, HostCliKind, HostCliRunner, NlBatchStatus,
    NlFunnelOptions, TokenBatchSpec,
};
use kp_model::{
    estimate_batch_tokens, format_tagged_turns, ActorId, AudienceSpec, CalendarTable, ChunkTurn,
    EpochMs, ExtractionBatchOutcome, ExtractionChunkSlice, FactAccess, FactKind, GovernedScope,
    LibraryMode, LlmClient, MemberRole, MembershipRelation, DEFAULT_MAX_BATCH_TOKENS,
};
use kp_store::{InitOptions, Store};

type SmokeResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Resolves the host CLI binary strictly from `KP_LIVE_HOST_BIN` or `PATH` (never hardcoded user paths).
fn discover_host_binary(kind: HostCliKind) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("KP_LIVE_HOST_BIN") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let bin_name = match kind {
        HostCliKind::Codex => "codex",
        HostCliKind::ClaudeCode => "claude",
    };
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(bin_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn resolve_host_kind_and_bin() -> SmokeResult<(HostCliKind, PathBuf)> {
    let host_env = std::env::var("KP_LIVE_HOST").unwrap_or_else(|_| "codex".into());
    let kind = match host_env.trim().to_ascii_lowercase().as_str() {
        "codex" => HostCliKind::Codex,
        "claude" | "claude-code" => HostCliKind::ClaudeCode,
        other => return Err(format!("unsupported KP_LIVE_HOST={other}").into()),
    };
    let program = discover_host_binary(kind).ok_or_else(|| {
        format!("host CLI binary for {kind:?} not found via KP_LIVE_HOST_BIN or PATH")
    })?;
    Ok((kind, program))
}

fn build_live_runner(
    kind: HostCliKind,
    program: PathBuf,
    reasoning_effort_override: Option<Option<String>>,
    timeout: Duration,
) -> SmokeResult<HostCliRunner> {
    let mut cfg = HostCliConfig::for_kind(kind);
    cfg.program = program;
    if let Some(override_effort) = reasoning_effort_override {
        cfg.reasoning_effort = override_effort;
    }
    cfg.timeout = timeout;
    if let Ok(model) = std::env::var("KP_LIVE_MODEL") {
        if !model.trim().is_empty() {
            cfg.model = Some(model);
        }
    }
    Ok(HostCliRunner::new(cfg)?)
}

fn fresh_club_store() -> SmokeResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let t_init = EpochMs(1_795_000_000_000);
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "live-smoke".parse()?,
            name: "Live Host Smoke".into(),
            mode: LibraryMode::Community,
            owner: "alice".parse()?,
            created_at: t_init,
        },
    )?;
    for member in ["bob", "carol", "erin"] {
        store.add_member(member.parse()?, MemberRole::Member, t_init)?;
    }
    Ok((dir, store))
}

#[test]
fn live_smoke_corpus_contains_zero_holdout_phrases_and_zero_personal_paths() {
    let source = include_str!("live_host_smoke.rs");
    let holdout_engine = format!("{}{}", "gb", "rain");
    let user_root = format!("/{}/", "Users");
    let home_root = format!("/{}/", "home");
    let forbidden = [
        "\u{4e0b}\u{66b4}\u{96e8}",
        "\u{66b4}\u{96e8}",
        "\u{96f7}\u{9635}\u{96e8}",
        "\u{8f6c}\u{6674}",
        "\u{4e0b}\u{96e8}",
        "\u{5f00}\u{8f66}",
        "\u{5bb6}\u{4eba}",
        "\u{53d1}\u{5e03}\u{51bb}\u{7ed3}",
        "\u{7d27}\u{6025}\u{4fee}\u{590d}",
        holdout_engine.as_str(),
    ];
    let lower = source.to_ascii_lowercase();
    for phrase in forbidden {
        assert!(
            !lower.contains(phrase),
            "forbidden holdout phrase found in live_host_smoke.rs: {phrase}"
        );
    }
    assert!(
        !source.contains(&user_root) && !source.contains(&home_root),
        "live_host_smoke.rs must resolve binaries strictly via KP_LIVE_HOST_BIN and PATH"
    );
}

#[derive(Clone, Debug)]
struct CaseMetric {
    case_no: usize,
    label: &'static str,
    passed: bool,
    elapsed_ms: u128,
    fact_count: usize,
    detail: String,
}

#[derive(Clone, Debug)]
struct NineCaseReport {
    metrics: Vec<CaseMetric>,
    passed_count: usize,
    total_ms: u128,
}

async fn extract_single_case(
    case_no: usize,
    label: &str,
    runner: &HostCliRunner,
    calendar: &CalendarTable,
    primary_speaker: &ActorId,
    turns: &[ChunkTurn],
    t0: EpochMs,
) -> SmokeResult<(Vec<kp_model::Fact>, u128)> {
    let (_dir, mut store) = fresh_club_store()?;
    let body = format_tagged_turns(turns);
    let req = build_extraction_batch_request(
        &store,
        format!("src_case_{case_no}").parse()?,
        primary_speaker.clone(),
        calendar.clone(),
        Vec::new(),
        vec![ExtractionChunkSlice {
            chunk_id: format!("c{case_no}"),
            heading: label.into(),
            body,
        }],
        5,
    )?;
    let started = Instant::now();
    let outcome = runner.extract_batch(&req).await?;
    let elapsed_ms = started.elapsed().as_millis();
    let ExtractionBatchOutcome::Completed { atoms } = outcome else {
        return Err(
            format!("case #{case_no} ({label}) expected Completed, got {outcome:?}").into(),
        );
    };
    let funnel_out = apply_extracted_atoms(
        &mut store,
        &req,
        &atoms,
        &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
    )?;
    let graph = store.derivation_view()?;
    let created: Vec<kp_model::Fact> = funnel_out
        .created_fact_ids
        .iter()
        .filter_map(|id| graph.facts.get(id).cloned())
        .collect();
    Ok((created, elapsed_ms))
}

#[allow(clippy::too_many_lines)]
async fn run_nine_cases(runner: &HostCliRunner, tag: &str) -> SmokeResult<NineCaseReport> {
    // Reference: Thursday 2026-11-19 20:00 UTC+8.
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let fri_win = calendar
        .window_for_date("2026-11-20")
        .ok_or("missing fri")?;
    let sat_win = calendar
        .window_for_date("2026-11-21")
        .ok_or("missing sat")?;
    let sun_win = calendar
        .window_for_date("2026-11-22")
        .ok_or("missing sun")?;
    let next_wed_win = calendar
        .window_for_date("2026-11-25")
        .ok_or("missing next wed")?;

    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let erin: ActorId = "erin".parse()?;

    type Checker = Box<dyn Fn(&[kp_model::Fact]) -> bool>;
    struct CaseDef {
        no: usize,
        label: &'static str,
        speaker: ActorId,
        turns: Vec<ChunkTurn>,
        check: Checker,
    }

    let a1 = alice.clone();
    let b2 = bob.clone();
    let a3 = alice.clone();
    let b4 = bob.clone();
    let a5 = alice.clone();
    let a6 = alice.clone();
    let b6 = bob.clone();
    let b7 = bob.clone();
    let a8 = alice.clone();
    let e8 = erin.clone();
    let a9 = alice.clone();
    let b9 = bob.clone();
    let c9 = carol.clone();

    let defs = vec![
        CaseDef {
            no: 1,
            label: "reading_club_tomorrow",
            speaker: alice.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "明天晚上七点我们在三楼阅览室办科幻读书会。".into(),
            }],
            check: Box::new(move |facts| {
                !facts.is_empty()
                    && facts.iter().all(|f| f.author == a1)
                    && facts.iter().any(|f| {
                        matches!(&f.access, FactAccess::Base { stamp, .. } if stamp.valid == fri_win)
                    })
            }),
        },
        CaseDef {
            no: 2,
            label: "gym_weekend_closure",
            speaker: bob.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(bob.clone()),
                is_assistant: false,
                text: "物业通知这周末两天健身房力量区闭馆检修。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().all(|f| f.author == b2)
                    && facts.iter().any(|f| {
                        matches!(
                            &f.access,
                            FactAccess::Base { stamp, .. }
                                if stamp.valid.valid_from == sat_win.valid_from
                                    && stamp.valid.valid_until == sun_win.valid_until
                        )
                    })
            }),
        },
        CaseDef {
            no: 3,
            label: "reimbursement_open_ended",
            speaker: alice.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "财务刚通知，下周三起差旅住宿报销上限调整为每晚六百元。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().all(|f| f.author == a3)
                    && facts.iter().any(|f| {
                        matches!(
                            &f.access,
                            FactAccess::Base { stamp, .. }
                                if stamp.valid.valid_from == next_wed_win.valid_from
                                    && stamp.valid.valid_until == EpochMs::OPEN_END
                        )
                    })
            }),
        },
        CaseDef {
            no: 4,
            label: "speaker_self_routine",
            speaker: bob.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(bob.clone()),
                is_assistant: false,
                text: "我自己晚上九点以后绝不喝美式咖啡。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().any(|f| {
                    f.author == b4
                        && f.governed_scope == Some(GovernedScope::SelfOnly { actor: b4.clone() })
                        && f.broader_scope.is_none()
                })
            }),
        },
        CaseDef {
            no: 5,
            label: "relation_group_book_club",
            speaker: alice.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "我们读书会每次线下沙龙都禁止外传录音。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().any(|f| {
                    f.author == a5
                        && matches!(
                            &f.governed_scope,
                            Some(GovernedScope::RelationGroup { anchor, .. }) if anchor == &a5
                        )
                })
            }),
        },
        CaseDef {
            no: 6,
            label: "named_mentions_bob",
            speaker: alice.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "明天巡检期间 Bob 不要重启预发环境数据库。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().any(|f| {
                    f.author == a6
                        && f.governed_scope
                            == Some(GovernedScope::Actors {
                                actors: BTreeSet::from([b6.clone()]),
                            })
                })
            }),
        },
        CaseDef {
            no: 7,
            label: "ambiguous_subjectless_rule",
            speaker: bob.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(bob.clone()),
                is_assistant: false,
                text: "出差短途一律坐高铁二等座。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().any(|f| {
                    f.author == b7
                        && f.governed_scope == Some(GovernedScope::SelfOnly { actor: b7.clone() })
                        && f.broader_scope.is_some()
                })
            }),
        },
        CaseDef {
            no: 8,
            label: "membership_erin_book_club",
            speaker: alice.clone(),
            turns: vec![ChunkTurn {
                speaker: Some(alice.clone()),
                is_assistant: false,
                text: "Erin 是我们读书会的成员。".into(),
            }],
            check: Box::new(move |facts| {
                facts.iter().any(|f| {
                    f.kind == FactKind::Membership
                        && serde_json::from_value::<MembershipRelation>(f.object.clone())
                            .is_ok_and(|m| m.anchor == a8 && m.member == e8)
                })
            }),
        },
        CaseDef {
            no: 9,
            label: "multi_speaker_and_assistant_context",
            speaker: alice.clone(),
            turns: vec![
                ChunkTurn {
                    speaker: Some(alice),
                    is_assistant: false,
                    text: "周五下午四点在二楼会议室集中核对差旅报销单。".into(),
                },
                ChunkTurn {
                    speaker: None,
                    is_assistant: true,
                    text: "建议顺便规定所有纸质票据必须在周四下班前拍照归档。".into(),
                },
                ChunkTurn {
                    speaker: Some(bob),
                    is_assistant: false,
                    text: "我自己每次打车都会当场导出电子行程单。".into(),
                },
                ChunkTurn {
                    speaker: Some(carol),
                    is_assistant: false,
                    text: "好的，收到！".into(),
                },
            ],
            check: Box::new(move |facts| {
                facts.iter().any(|f| f.author == a9)
                    && facts.iter().any(|f| f.author == b9)
                    && !facts.iter().any(|f| f.author == c9)
                    && !facts.iter().any(|f| {
                        f.source_anchor
                            .as_ref()
                            .and_then(|a| a.verbatim_quote.as_deref())
                            .is_some_and(|q| q.contains("纸质票据"))
                    })
            }),
        },
    ];

    let mut metrics = Vec::with_capacity(defs.len());
    let mut passed_count = 0usize;
    let mut total_ms = 0u128;

    for def in defs {
        let case_started = Instant::now();
        match extract_single_case(
            def.no,
            def.label,
            runner,
            &calendar,
            &def.speaker,
            &def.turns,
            t0,
        )
        .await
        {
            Ok((facts, elapsed_ms)) => {
                let passed = (def.check)(&facts);
                if passed {
                    passed_count += 1;
                }
                total_ms += elapsed_ms;
                let detail = if passed {
                    "ok".into()
                } else {
                    format!("predicate mismatch on {facts:?}")
                };
                println!(
                    "[live_host_smoke:{tag}] case #{} ({}): {} ms -> {} fact(s) [{}]",
                    def.no,
                    def.label,
                    elapsed_ms,
                    facts.len(),
                    if passed { "PASS" } else { "FAIL" }
                );
                metrics.push(CaseMetric {
                    case_no: def.no,
                    label: def.label,
                    passed,
                    elapsed_ms,
                    fact_count: facts.len(),
                    detail,
                });
            }
            Err(err) => {
                let elapsed_ms = case_started.elapsed().as_millis();
                total_ms += elapsed_ms;
                println!(
                    "[live_host_smoke:{tag}] case #{} ({}): {} ms -> ERROR: {err}",
                    def.no, def.label, elapsed_ms
                );
                metrics.push(CaseMetric {
                    case_no: def.no,
                    label: def.label,
                    passed: false,
                    elapsed_ms,
                    fact_count: 0,
                    detail: err.to_string(),
                });
            }
        }
    }

    Ok(NineCaseReport {
        metrics,
        passed_count,
        total_ms,
    })
}

fn synthetic_chat_turns(turn_count: usize) -> SmokeResult<Vec<ChunkTurn>> {
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    let carol: ActorId = "carol".parse()?;
    let erin: ActorId = "erin".parse()?;

    let templates: [(&ActorId, bool, &str); 10] = [
        (
            &alice,
            false,
            "周五下午三点我们在三楼阅览室核对读书会书单。",
        ),
        (&bob, false, "好的，收到！"),
        (&erin, false, "我自己平时做读书笔记只用纯文本格式。"),
        (
            &alice,
            true,
            "提示：如需投影仪可提前在行政系统登记借用时段。",
        ),
        (&carol, false, "收到"),
        (
            &bob,
            false,
            "下周三起部门差旅餐补标准统一按每日一百二十元执行。",
        ),
        (&erin, false, "恩"),
        (&alice, false, "这周末两天健身房游泳馆换水停业维护。"),
        (
            &carol,
            false,
            "明天下午巡检期间 Bob 不要重启测试服缓存节点。",
        ),
        (&bob, false, "明白！"),
    ];

    let mut turns = Vec::with_capacity(turn_count);
    for idx in 0..turn_count {
        let (actor, is_assistant, base_text) = templates[idx % templates.len()];
        let round = idx / templates.len() + 1;
        let text = if is_assistant || base_text.chars().count() <= 6 {
            base_text.to_owned()
        } else {
            format!("（第{round}组）{base_text}")
        };
        turns.push(ChunkTurn {
            speaker: (!is_assistant).then(|| actor.clone()),
            is_assistant,
            text,
        });
    }
    Ok(turns)
}

#[tokio::test]
#[ignore = "requires live host CLI outside sandbox; run with KP_LIVE_HOST=codex -- --ignored live_host_cli_nine_sentence_natural_chinese_smoke --nocapture"]
async fn live_host_cli_nine_sentence_natural_chinese_smoke() -> SmokeResult {
    let (kind, program) = resolve_host_kind_and_bin()?;
    let effort_override: Option<Option<String>> = std::env::var("KP_LIVE_REASONING_EFFORT")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(|val| {
            if val.eq_ignore_ascii_case("none") || val.eq_ignore_ascii_case("default") {
                None
            } else {
                Some(val)
            }
        });
    let runner = build_live_runner(
        kind,
        program.clone(),
        effort_override,
        Duration::from_secs(180),
    )?;
    let effective_effort = runner.config().reasoning_effort.clone();
    println!(
        "[live_host_smoke] host={kind:?} binary={} reasoning_effort={effective_effort:?}",
        program.display()
    );
    let tag = effective_effort.as_deref().unwrap_or("default");
    let report = run_nine_cases(&runner, tag).await?;
    for m in &report.metrics {
        assert!(
            m.passed,
            "case #{} ({}) failed: {}",
            m.case_no, m.label, m.detail
        );
    }
    assert_eq!(report.passed_count, 9);
    println!(
        "[live_host_smoke] summary: {}/9 passed in {:.1}s",
        report.passed_count,
        report.total_ms as f64 / 1000.0
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires live host CLI outside sandbox; compares low vs default reasoning effort + times 20-turn & 50-turn token-budgeted batches"]
async fn live_host_cli_reasoning_effort_and_batch_size_benchmark() -> SmokeResult {
    let (kind, program) = resolve_host_kind_and_bin()?;
    println!(
        "[live_benchmark] host={kind:?} binary={}",
        program.display()
    );

    let low_runner = build_live_runner(
        kind,
        program.clone(),
        Some(Some("low".into())),
        Duration::from_secs(240),
    )?;
    let low_report = run_nine_cases(&low_runner, "low").await?;

    let skip_default = std::env::var("KP_BENCH_SKIP_DEFAULT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let default_report = if skip_default {
        None
    } else {
        let default_runner =
            build_live_runner(kind, program.clone(), Some(None), Duration::from_secs(240))?;
        Some(run_nine_cases(&default_runner, "default").await?)
    };

    println!("\n### Reasoning Effort Comparison (9 Natural Chinese Cases)\n");
    println!("| Case | `low` (s) | `low` Facts | `default` (s) | `default` Facts |");
    println!("|---|---:|---:|---:|---:|");
    for (idx, low_m) in low_report.metrics.iter().enumerate() {
        let (def_sec, def_facts) = match &default_report {
            Some(rep) => (
                format!("{:.1}", rep.metrics[idx].elapsed_ms as f64 / 1000.0),
                format!(
                    "{} ({})",
                    rep.metrics[idx].fact_count,
                    if rep.metrics[idx].passed {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                ),
            ),
            None => ("skipped".into(), "-".into()),
        };
        println!(
            "| #{} {} | {:.1} | {} ({}) | {} | {} |",
            low_m.case_no,
            low_m.label,
            low_m.elapsed_ms as f64 / 1000.0,
            low_m.fact_count,
            if low_m.passed { "PASS" } else { "FAIL" },
            def_sec,
            def_facts
        );
    }
    if let Some(rep) = &default_report {
        println!(
            "\nTotals: `low` {}/9 in {:.1}s vs `default` {}/9 in {:.1}s",
            low_report.passed_count,
            low_report.total_ms as f64 / 1000.0,
            rep.passed_count,
            rep.total_ms as f64 / 1000.0
        );
        assert!(
            low_report.passed_count >= rep.passed_count,
            "low reasoning pass rate ({}/9) dropped below default ({}/9)",
            low_report.passed_count,
            rep.passed_count
        );
    }
    assert_eq!(low_report.passed_count, 9);

    // Part B: 20-turn and 50-turn token-budgeted batch latency benchmark on `low`.
    let t0 = EpochMs(1_795_089_600_000);
    let shanghai = kp_model::TimeZone::get("Asia/Shanghai")?;
    let calendar = CalendarTable::from_reference_in_tz(t0, &shanghai)?;
    let alice: ActorId = "alice".parse()?;

    println!("\n### Token-Budgeted Multi-Turn Batch Benchmark (`low`, max_batch_tokens={DEFAULT_MAX_BATCH_TOKENS})\n");
    println!("| Turns | Est. Tokens | Sub-batches | Elapsed (s) | Extracted Facts |");
    println!("|---:|---:|---:|---:|---:|");

    for turn_count in [20usize, 50usize] {
        let (_dir, mut store) = fresh_club_store()?;
        let turns = synthetic_chat_turns(turn_count)?;
        let est_tokens = estimate_batch_tokens(&format_tagged_turns(&turns));
        let spec = TokenBatchSpec {
            source_id: format!("src_batch_{turn_count}").parse()?,
            default_speaker: alice.clone(),
            reference_calendar: calendar.clone(),
            active_concepts: Vec::new(),
            heading: format!("{turn_count}-turn chat window"),
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            min_rate_limit_remaining_pct: 5,
        };
        let requests = build_token_budgeted_turn_batches(&store, &spec, &turns)?;
        assert!(!requests.is_empty());

        let started = Instant::now();
        let mut created_total = 0usize;
        for req in &requests {
            let out = extract_and_ingest_batch(
                &mut store,
                &low_runner,
                req,
                &NlFunnelOptions::personal_or_shared(AudienceSpec::AllMembers, t0),
            )
            .await?;
            assert_eq!(out.status, NlBatchStatus::Completed);
            created_total += out.created_fact_ids.len();
        }
        let elapsed_s = started.elapsed().as_secs_f64();
        println!(
            "| {turn_count} | {est_tokens} | {} | {elapsed_s:.1} | {created_total} |",
            requests.len()
        );
        assert!(created_total > 0);
    }

    Ok(())
}

#[tokio::test]
#[ignore = "requires live host CLI outside sandbox; run with KP_LIVE_HOST=codex -- --ignored live_host_cli_quote_aware_entailment_smoke --nocapture"]
async fn live_host_cli_quote_aware_entailment_smoke() -> SmokeResult {
    let (kind, program) = resolve_host_kind_and_bin()?;
    let runner = build_live_runner(kind, program.clone(), None, Duration::from_secs(180))?;
    println!(
        "[live_entailment_smoke] host={kind:?} binary={}",
        program.display()
    );

    let cases: [(&str, kp_model::EntailmentQuery, kp_model::EntailmentVerdict); 4] = [
        (
            "shared_word_x_vs_when_x_occurs_habitual_rule",
            kp_model::EntailmentQuery {
                left_label: "停电".into(),
                left_domain: "power".into(),
                right_label: "出现停电的时候".into(),
                right_domain: "lab".into(),
                left_quote: Some("本周五下午园区变电站检修停电。".into()),
                right_quote: Some("我们硬件实验室出现停电的时候都关闭精密仪器电源。".into()),
            },
            kp_model::EntailmentVerdict::Entails,
        ),
        (
            "paraphrase_event_vs_habitual_encounter_rule",
            kp_model::EntailmentQuery {
                left_label: "主库停机维护".into(),
                left_domain: "database".into(),
                right_label: "数据库不可用".into(),
                right_domain: "ops".into(),
                left_quote: Some("本周五凌晨账务数据库主库停机维护两小时。".into()),
                right_quote: Some("遇到数据库不可用的时候都切只读副本并暂停批量对账。".into()),
            },
            kp_model::EntailmentVerdict::Entails,
        ),
        (
            "same_city_and_friday_unrelated_situations",
            kp_model::EntailmentQuery {
                left_label: "杭州独立书展".into(),
                left_domain: "culture".into(),
                right_label: "海鲜过敏忌口".into(),
                right_domain: "dining".into(),
                left_quote: Some("本周五下午杭州举办秋季独立书展。".into()),
                right_quote: Some("在杭州聚餐的时候都不点贝类海鲜。".into()),
            },
            kp_model::EntailmentVerdict::Unrelated,
        ),
        (
            "same_floor_and_friday_unrelated_situations",
            kp_model::EntailmentQuery {
                left_label: "茶水间咖啡机除垢".into(),
                left_domain: "facility".into(),
                right_label: "核心模块双人评审".into(),
                right_domain: "engineering".into(),
                left_quote: Some("本周五上午三楼茶水间咖啡机例行除垢清洗。".into()),
                right_quote: Some("周五提交核心模块代码的时候都需要两位同事签字评审。".into()),
            },
            kp_model::EntailmentVerdict::Unrelated,
        ),
    ];

    for (name, query, expected) in cases {
        let started = Instant::now();
        let actual = runner.judge_entailment(&query).await?;
        let elapsed_s = started.elapsed().as_secs_f64();
        println!("[live_entailment_smoke] {name}: actual={actual:?} expected={expected:?} ({elapsed_s:.1}s)");
        assert_eq!(actual, expected, "entailment case `{name}` mismatch");
    }
    Ok(())
}

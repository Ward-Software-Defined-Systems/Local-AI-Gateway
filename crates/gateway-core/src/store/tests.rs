//! End-to-end store tests: write through the writer task, read through the pool.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use uuid::Uuid;

use super::read::{self, FindingFilter, RequestFilter, SeriesMetric, SeriesQuery};
use super::retention::RetentionPolicy;
use super::{open, open_writer_connection, rows, temp_db_path};
use crate::types::{
    Alert, AlertKind, ApiFormat, Backend, BackendGroup, Body, Direction, DlpAction, FailureSource,
    Finding, FindingCategory, HardwareSample, ModelInfo, ModelState, Outcome, ProcStats,
    RequestKind, RequestRecord, Severity,
};

const PROMPT: &str = r#"{"messages":[{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"}]}"#;

fn record(
    session_id: i64,
    backend: Backend,
    api_format: ApiFormat,
    outcome: Outcome,
    ended_ago_ms: i64,
) -> RequestRecord {
    // The store keeps milliseconds; build the record at that precision so it round-trips exactly.
    let ended = rows::from_ms(rows::ms(Utc::now() - Duration::milliseconds(ended_ago_ms)));
    RequestRecord {
        id: Uuid::now_v7(),
        session_id,
        backend,
        listener: "127.0.0.1:11435".into(),
        kind: RequestKind::Http,
        client_ip: "10.0.0.7".into(),
        user_agent: Some("curl/8".into()),
        client_request_id: None,
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        query: None,
        api_format,
        streamed: true,
        model_requested: Some("qwen3:14b".into()),
        model_resolved: Some("qwen3:14b".into()),
        is_cloud: false,
        started_at: ended - Duration::milliseconds(1200),
        first_byte_at: Some(ended - Duration::milliseconds(900)),
        ended_at: ended,
        status: Some(match &outcome {
            Outcome::Failed { status, .. } => *status,
            _ => 200,
        }),
        outcome,
        error_summary: None,
        prompt_tokens: Some(11),
        gen_tokens: Some(5),
        tokens_estimated: false,
        ttft_ms: Some(300.0),
        total_ms: 1200.0,
        tokens_per_sec: Some(18.5),
        timings_estimated: true,
        stop_reason: Some("stop".into()),
        request_bytes: 120,
        response_bytes: 340,
        request_headers: vec![("content-type".into(), "application/json".into())],
        response_headers: vec![("content-type".into(), "text/event-stream".into())],
        chunk_timeline: vec![(300, 80), (650, 120), (1200, 140)],
        finding_count: 0,
        max_severity: None,
        hypothetical_cost: BTreeMap::from([("claude-fable-5-1".to_string(), 0.00036)]),
        actual_cost_usd: None,
        routed_by: None,
    }
}

#[tokio::test]
async fn cloud_spend_and_group_scopes_separate_the_providers_from_the_local_servers() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&temp_db_path(&dir)).await.unwrap();
    let session = store.writer.start_session("0.1.0-test").await.unwrap();
    let mut priced = record(
        session,
        Backend::Anthropic,
        ApiFormat::AnthropicMessages,
        Outcome::Completed,
        10,
    );
    priced.is_cloud = true;
    priced.actual_cost_usd = Some(0.25);
    let mut unpriced = record(
        session,
        Backend::Anthropic,
        ApiFormat::AnthropicMessages,
        Outcome::Completed,
        20,
    );
    unpriced.is_cloud = true;
    let mut gemini = record(
        session,
        Backend::Gemini,
        ApiFormat::GeminiGenerate,
        Outcome::Completed,
        30,
    );
    gemini.is_cloud = true;
    gemini.actual_cost_usd = Some(1.5);
    let mut gemini_failed = record(
        session,
        Backend::Gemini,
        ApiFormat::GeminiGenerate,
        Outcome::Failed {
            status: 429,
            source: FailureSource::Upstream,
        },
        40,
    );
    gemini_failed.is_cloud = true;
    let local = record(
        session,
        Backend::Ollama,
        ApiFormat::OllamaNative,
        Outcome::Completed,
        50,
    );
    for rec in [&priced, &unpriced, &gemini, &gemini_failed, &local] {
        store
            .writer
            .persist_request(rec.clone(), Vec::new(), Vec::new())
            .await
            .unwrap();
    }
    let from = Utc::now() - Duration::hours(1);
    let to = Utc::now() + Duration::hours(1);
    let spend = store
        .reads
        .run(move |c| read::cloud_spend(c, from, to))
        .await
        .unwrap();
    assert_eq!(spend.len(), 2, "{spend:?}");
    let anthropic = &spend[0];
    assert_eq!(anthropic.backend, Backend::Anthropic);
    assert_eq!(anthropic.requests, 2);
    assert_eq!(anthropic.unpriced, 1);
    assert_eq!(anthropic.spend_usd, 0.25);
    assert_eq!(anthropic.prompt_tokens, 22);
    let gemini_row = &spend[1];
    assert_eq!(gemini_row.backend, Backend::Gemini);
    assert_eq!(gemini_row.requests, 1, "failed requests do not count");
    assert_eq!(gemini_row.spend_usd, 1.5);

    let (cloud_prompt, _) = store
        .reads
        .run(move |c| read::token_totals(c, from, to, None, Some(BackendGroup::Cloud)))
        .await
        .unwrap();
    assert_eq!(
        cloud_prompt, 33,
        "three completed cloud requests at 11 prompt tokens"
    );
    let (local_prompt, _) = store
        .reads
        .run(move |c| read::token_totals(c, from, to, None, Some(BackendGroup::Local)))
        .await
        .unwrap();
    assert_eq!(local_prompt, 11);
    let page = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    group: Some(BackendGroup::Cloud),
                    limit: 10,
                    ..RequestFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert!(
        page.items
            .iter()
            .all(|r| r.record.backend.is_cloud_provider())
    );
    let activity = store
        .reads
        .run(move |c| read::activity_since(c, from))
        .await
        .unwrap();
    assert_eq!(activity.cloud_requests, 4);
    assert_eq!(activity.cloud_spend_usd, 1.75);
}

#[tokio::test]
async fn the_routing_decision_and_real_cost_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&temp_db_path(&dir)).await.unwrap();
    let session = store.writer.start_session("0.1.0-test").await.unwrap();
    let mut rec = record(
        session,
        Backend::Anthropic,
        ApiFormat::AnthropicMessages,
        Outcome::Completed,
        10,
    );
    rec.is_cloud = true;
    rec.actual_cost_usd = Some(0.0123);
    rec.routed_by = Some("alias".into());
    store
        .writer
        .persist_request(rec.clone(), Vec::new(), Vec::new())
        .await
        .unwrap();
    let id = rec.id;
    let detail = store
        .reads
        .run(move |c| read::get_request(c, id))
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(detail.record, rec);
    let page = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    backend: Some(Backend::Anthropic),
                    limit: 10,
                    ..RequestFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].record.routed_by.as_deref(), Some("alias"));
    assert_eq!(page.items[0].record.actual_cost_usd, Some(0.0123));
}

fn body(request_id: Uuid, direction: Direction, text: &str) -> Body {
    Body {
        request_id,
        direction,
        content_type: Some("application/json".into()),
        original_bytes: text.len() as u64,
        stored_bytes: text.len() as u64,
        truncated: false,
        redacted: false,
        is_text: true,
        text: text.to_string(),
    }
}

fn finding(request_id: Uuid) -> Finding {
    Finding {
        request_id,
        direction: Direction::Request,
        rule_id: "aws-access-key-id".into(),
        category: FindingCategory::Secret,
        severity: Severity::Critical,
        location: "$.messages[0].content".into(),
        offset: Some(31),
        length: 20,
        fingerprint: "0123456789abcdef0123456789abcdef".into(),
        preview: "AKIA…MPLE".into(),
        action: DlpAction::Alert,
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn writes_are_durable_and_readable() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&temp_db_path(&dir)).await.unwrap();
    let session = store.writer.start_session("0.1.0-test").await.unwrap();
    assert_eq!(session, 1);

    let mut r1 = record(
        session,
        Backend::Ollama,
        ApiFormat::OpenAiChat,
        Outcome::Completed,
        3000,
    );
    r1.finding_count = 1;
    r1.max_severity = Some(Severity::Critical);
    store
        .writer
        .persist_request(
            r1.clone(),
            vec![
                body(r1.id, Direction::Request, PROMPT),
                body(r1.id, Direction::Response, "hello world"),
            ],
            vec![finding(r1.id)],
        )
        .await
        .unwrap();
    let r2 = record(
        session,
        Backend::LmStudio,
        ApiFormat::OpenAiChat,
        Outcome::Failed {
            status: 404,
            source: FailureSource::Upstream,
        },
        2000,
    );
    store
        .writer
        .persist_request(r2.clone(), vec![], vec![])
        .await
        .unwrap();
    let mut r3 = record(
        session,
        Backend::Ollama,
        ApiFormat::Passthrough,
        Outcome::Completed,
        1000,
    );
    r3.method = "GET".into();
    r3.path = "/api/tags".into();
    store
        .writer
        .persist_request(r3.clone(), vec![], vec![])
        .await
        .unwrap();
    // Replies come after the commit, so everything above is on disk once this returns.
    store.writer.end_session(session).await.unwrap();

    let totals = store.reads.run(read::lifetime_totals).await.unwrap();
    assert_eq!(totals.session_count, 1);
    assert_eq!(
        totals.request_count, 1,
        "only completed inference rows count"
    );
    assert_eq!((totals.prompt_tokens, totals.gen_tokens), (11, 5));

    let page = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2, "passthrough hidden by default");
    assert_eq!(page.items[0].record.id, r2.id, "newest first");
    assert_eq!(page.items[1].record.id, r1.id);
    assert!(page.items[1].has_request_body && page.items[1].has_response_body);
    assert!(!page.items[0].has_request_body);
    assert!(page.next_cursor.is_none());
    assert!(
        page.items[1].record.request_headers.is_empty(),
        "summaries skip the heavy columns"
    );

    let all = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    include_passthrough: true,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(all.items.len(), 3);

    let ollama = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    backend: Some(Backend::Ollama),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(ollama.items.len(), 1);
    let failed = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    outcome: Some("failed".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(
        failed.items[0].record.outcome,
        Outcome::Failed {
            status: 404,
            source: FailureSource::Upstream
        }
    );
    let with_findings = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    has_findings: Some(true),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(with_findings.items.len(), 1);
    let by_model = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    model: Some("qwen3:14b".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(by_model.items.len(), 2);

    let hit = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    q: Some("AKIAIOSFODNN7EXAMPLE".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(hit.items.len(), 1);
    assert_eq!(hit.items[0].record.id, r1.id);
    let prefix = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    q: Some("AKIAIOSF*".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(prefix.items.len(), 1, "prefix search");
    let miss = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    q: Some("zebra".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(miss.items.is_empty());

    let first = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 1,
                    include_passthrough: true,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.clone().expect("more pages");
    let second = store
        .reads
        .run(move |c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 1,
                    include_passthrough: true,
                    cursor: Some(cursor),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(second.items[0].record.id, r2.id);
    assert!(second.next_cursor.is_some());

    let id = r1.id;
    let detail = store
        .reads
        .run(move |c| read::get_request(c, id))
        .await
        .unwrap()
        .expect("r1 exists");
    assert_eq!(
        detail.record, r1,
        "the full record round-trips, headers and timeline included"
    );
    assert_eq!(
        detail.request_body.as_ref().unwrap().stored_bytes,
        PROMPT.len() as u64
    );
    assert!(detail.response_body.is_some());
    assert_eq!(detail.findings.len(), 1);
    assert_eq!(detail.findings[0].rule_id, "aws-access-key-id");
    assert_eq!(detail.findings[0].preview, "AKIA…MPLE");
    let response = store
        .reads
        .run(move |c| read::get_body(c, id, Direction::Response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.text, "hello world");
    let missing = store
        .reads
        .run(|c| read::get_request(c, Uuid::nil()))
        .await
        .unwrap();
    assert!(missing.is_none());

    let (findings, next) = store
        .reads
        .run(|c| {
            read::list_findings(
                c,
                &FindingFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].backend, Backend::Ollama);
    assert_eq!(findings[0].model.as_deref(), Some("qwen3:14b"));
    assert!(next.is_none());

    let sessions = store
        .reads
        .run(|c| read::list_sessions(c, 10))
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].ended_at.is_some());
    assert_eq!(sessions[0].app_version, "0.1.0-test");

    let alert_id = store
        .writer
        .persist_alert(Alert {
            id: 0,
            created_at: Utc::now(),
            kind: AlertKind::DlpFinding,
            severity: Severity::Critical,
            title: "secret in prompt".into(),
            detail: serde_json::json!({"rule": "aws-access-key-id"}),
            request_id: Some(r1.id),
            acknowledged_at: None,
            delivered: vec!["in_app".into()],
        })
        .await
        .unwrap();
    assert!(alert_id > 0);
    assert_eq!(store.reads.run(read::unacked_alert_count).await.unwrap(), 1);
    assert!(store.writer.ack_alert(alert_id).await.unwrap());
    assert!(
        !store.writer.ack_alert(alert_id).await.unwrap(),
        "already acknowledged"
    );
    let (alerts, _) = store
        .reads
        .run(|c| read::list_alerts(c, false, None, 10))
        .await
        .unwrap();
    assert_eq!(alerts[0].title, "secret in prompt");
    assert!(alerts[0].acknowledged_at.is_some());

    assert_eq!(store.writer.delete_bodies(r1.id).await.unwrap(), 2);
    assert!(
        store
            .reads
            .run(move |c| read::get_body(c, id, Direction::Request))
            .await
            .unwrap()
            .is_none()
    );
    let gone = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    q: Some("AKIAIOSFODNN7EXAMPLE".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(gone.items.is_empty(), "the FTS index follows the delete");

    let series = store
        .reads
        .run(|c| {
            let q = SeriesQuery {
                metric: SeriesMetric::ReqPerMin,
                from: Utc::now() - Duration::hours(1),
                to: Utc::now() + Duration::hours(1),
                bucket_ms: 3_600_000,
                backend: None,
                group: None,
                model: None,
            };
            Ok(read::aggregate_series(&read::series_rows(c, &q)?, &q))
        })
        .await
        .unwrap();
    assert_eq!(
        series.iter().map(|p| p.values["requests"]).sum::<f64>(),
        2.0
    );
    let (prompt, gen_n) = store
        .reads
        .run(|c| {
            read::token_totals(
                c,
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::hours(1),
                None,
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!((prompt, gen_n), (11, 5));
    assert!(store.reads.run(read::db_size_bytes).await.unwrap() > 0);
    store.writer.checkpoint().await.unwrap();
}

#[tokio::test]
async fn hardware_samples_roll_up_by_minute() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&temp_db_path(&dir)).await.unwrap();
    let base = Utc::now() - Duration::minutes(10);
    let base = base - Duration::milliseconds(base.timestamp_millis().rem_euclid(60_000));
    for (offset_s, cpu, gpu) in [
        (0, 10.0, Some(20.0)),
        (20, 30.0, None),
        (40, 50.0, Some(40.0)),
        (70, 70.0, Some(60.0)),
    ] {
        store
            .writer
            .persist_hardware(HardwareSample {
                at: base + Duration::seconds(offset_s),
                system_cpu_pct: cpu,
                mem_used_bytes: 1_000,
                mem_total_bytes: 2_000,
                mem_available_bytes: 1_000,
                lmstudio: ProcStats::default(),
                ollama: ProcStats {
                    process_count: 1,
                    cpu_pct: 5.0,
                    rss_bytes: 500,
                },
                gpu_util_pct: gpu,
                gpu_renderer_pct: None,
                gpu_tiler_pct: None,
                gpu_mem_used_bytes: Some(300),
                gpu_mem_alloc_bytes: None,
                ane_power_mw: None,
            })
            .await
            .unwrap();
    }
    store.writer.checkpoint().await.unwrap(); // a reply after the inserts: they are committed

    let raw = store
        .reads
        .run(move |c| {
            read::hardware_series(
                c,
                base - Duration::minutes(1),
                base + Duration::minutes(5),
                60_000,
            )
        })
        .await
        .unwrap();
    assert_eq!(raw.len(), 2);
    assert!(
        (raw[0].cpu.unwrap() - 30.0).abs() < 1e-9,
        "average of 10, 30, 50"
    );
    assert!(
        (raw[0].gpu.unwrap() - 30.0).abs() < 1e-9,
        "average of the two GPU readings"
    );
    assert_eq!(raw[1].cpu, Some(70.0));

    let rolled = store
        .reads
        .run(move |c| {
            read::hardware_series(
                c,
                base - Duration::hours(3),
                base + Duration::minutes(5),
                60_000,
            )
        })
        .await
        .unwrap();
    assert_eq!(rolled.len(), 2, "long windows read the minute rollups");
    assert!((rolled[0].cpu.unwrap() - 30.0).abs() < 1e-9);
    assert!((rolled[0].gpu.unwrap() - 30.0).abs() < 1e-9);
    assert_eq!(rolled[0].gpu_mem, Some(300.0));
}

#[tokio::test]
async fn retention_deletes_bodies_and_passthrough_rows_but_keeps_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&temp_db_path(&dir)).await.unwrap();
    let session = store.writer.start_session("t").await.unwrap();
    let old = record(
        session,
        Backend::Ollama,
        ApiFormat::OpenAiChat,
        Outcome::Completed,
        10 * 86_400_000,
    );
    store
        .writer
        .persist_request(
            old.clone(),
            vec![body(old.id, Direction::Request, "ancient prompt")],
            vec![],
        )
        .await
        .unwrap();
    let fresh = record(
        session,
        Backend::Ollama,
        ApiFormat::OpenAiChat,
        Outcome::Completed,
        1000,
    );
    store
        .writer
        .persist_request(
            fresh.clone(),
            vec![body(fresh.id, Direction::Request, "fresh prompt")],
            vec![],
        )
        .await
        .unwrap();
    let mut passthrough = record(
        session,
        Backend::Ollama,
        ApiFormat::Passthrough,
        Outcome::Completed,
        2 * 86_400_000,
    );
    passthrough.path = "/api/tags".into();
    store
        .writer
        .persist_request(passthrough.clone(), vec![], vec![])
        .await
        .unwrap();

    let policy = RetentionPolicy::from_config(&crate::config::CaptureConfig::default());
    let report = store
        .writer
        .run_retention(policy, Utc::now())
        .await
        .unwrap();
    assert_eq!(report.bodies_deleted, 1);
    assert_eq!(report.passthrough_records_deleted, 1);
    assert_eq!(report.hardware_samples_deleted, 0);

    let id = old.id;
    let detail = store
        .reads
        .run(move |c| read::get_request(c, id))
        .await
        .unwrap()
        .expect("metadata stays");
    assert!(detail.request_body.is_none(), "the old body is gone");
    let fresh_id = fresh.id;
    let detail = store
        .reads
        .run(move |c| read::get_request(c, fresh_id))
        .await
        .unwrap()
        .unwrap();
    assert!(detail.request_body.is_some(), "the fresh body stays");
    let pt = passthrough.id;
    assert!(
        store
            .reads
            .run(move |c| read::get_request(c, pt))
            .await
            .unwrap()
            .is_none()
    );
    let hit = store
        .reads
        .run(|c| {
            read::list_requests(
                c,
                &RequestFilter {
                    limit: 10,
                    q: Some("ancient".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(hit.items.is_empty(), "the FTS index follows retention");
}

#[test]
fn models_seen_keeps_first_seen_and_hourly_rollups_accumulate() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_writer_connection(&temp_db_path(&dir)).unwrap();
    let model = ModelInfo {
        backend: Backend::Ollama,
        id: "qwen3:14b".into(),
        kind: Some("llm".into()),
        compatibility_type: Some("gguf".into()),
        quantization: Some("Q4_K_M".into()),
        max_context_length: Some(40_960),
        loaded_context_length: None,
        state: ModelState::Loaded,
        publisher: None,
        arch: None,
        capabilities: vec![],
        size_bytes: None,
        size_vram_bytes: None,
        expires_at: None,
        remote_host: None,
    };
    let t1 = Utc::now() - Duration::hours(1);
    let t2 = Utc::now();
    rows::upsert_model_seen(&conn, &model, t1).unwrap();
    let mut unloaded = model.clone();
    unloaded.state = ModelState::NotLoaded;
    rows::upsert_model_seen(&conn, &unloaded, t2).unwrap();
    let (first, last, state): (i64, i64, String) = conn
        .query_row(
            "SELECT first_seen, last_seen, last_state FROM models_seen",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(first, t1.timestamp_millis());
    assert_eq!(last, t2.timestamp_millis());
    assert_eq!(state, "not-loaded");

    conn.execute(
        "INSERT INTO sessions (started_at, app_version) VALUES (0, 't')",
        [],
    )
    .unwrap();
    let a = record(
        1,
        Backend::Ollama,
        ApiFormat::OpenAiChat,
        Outcome::Completed,
        1000,
    );
    let mut b = record(
        1,
        Backend::Ollama,
        ApiFormat::OpenAiChat,
        Outcome::Failed {
            status: 500,
            source: FailureSource::Upstream,
        },
        500,
    );
    b.tokens_per_sec = None;
    for r in [&a, &b] {
        rows::insert_request(&conn, r).unwrap();
        rows::upsert_request_rollup(&conn, r).unwrap();
    }
    let (requests, completed, failed, tps_count): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT requests, completed, failed, tps_count FROM request_rollups_hourly",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((requests, completed, failed, tps_count), (2, 1, 1, 1));
}

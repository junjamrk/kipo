mod support;

use std::fs;

use axum::{http::header, routing::get, Router};
use kp_ingest::doc_router::{add_file, add_url};
use kp_model::SourceModality;

use support::{library, options, Result};

#[tokio::test]
async fn html_lrc_whisper_media_sidecar_and_ssrf_guarded_url_ingest_preserve_modality_anchors(
) -> Result {
    let (dir, mut store) = library()?;
    let opts = options()?;

    let html_path = dir.path().join("runbook.html");
    fs::write(
        &html_path,
        r#"<!doctype html><html><head><title>Release Runbook</title><script>evil()</script></head>
        <body><h1>Database Cutover</h1><p>Never run <code>prisma migrate deploy</code> without <code>--schema</code> &amp; backup.</p></body></html>"#,
    )?;
    let added_html = add_file(&mut store, &html_path, &opts, None)?;
    assert_eq!(added_html.chunks, 1);
    let html_chunks = store.source_anchored_chunks(&added_html.source.source_id)?;
    assert_eq!(html_chunks[0].heading, "Database Cutover");
    assert!(html_chunks[0].body.contains("`prisma migrate deploy`"));
    assert!(!html_chunks[0].body.contains("evil()"));

    let lrc_path = dir.path().join("meeting.lrc");
    fs::write(
        &lrc_path,
        "[ti:Architecture Sync]\n[01:15.50] 严禁直接执行 pip install，必须统一改用 uv pip install。\n[01:28.00] 下一步检查 CI 锁文件。\n",
    )?;
    let added_lrc = add_file(&mut store, &lrc_path, &opts, None)?;
    let lrc_chunks = store.source_anchored_chunks(&added_lrc.source.source_id)?;
    assert_eq!(lrc_chunks.len(), 2);
    assert_eq!(lrc_chunks[0].anchor.start_ms, Some(75_500));
    assert_eq!(lrc_chunks[0].anchor.end_ms, Some(88_000));

    let whisper_path = dir.path().join("recording.json");
    fs::write(
        &whisper_path,
        r#"{"segments":[{"start":12.4,"end":19.8,"speaker":"alice","text":"部署前必须执行 pnpm test 并确认退出码为 0。"}]}"#,
    )?;
    let added_whisper = add_file(&mut store, &whisper_path, &opts, None)?;
    let whisper_chunks = store.source_anchored_chunks(&added_whisper.source.source_id)?;
    assert_eq!(whisper_chunks.len(), 1);
    assert_eq!(whisper_chunks[0].anchor.start_ms, Some(12_400));
    assert_eq!(whisper_chunks[0].anchor.end_ms, Some(19_800));
    assert!(whisper_chunks[0].body.contains("[speaker:alice]"));

    let video_path = dir.path().join("walkthrough.mp4");
    fs::write(&video_path, b"FAKE_MP4_CONTAINER_HEADER")?;
    fs::write(
        dir.path().join("walkthrough.vtt"),
        "WEBVTT\n\n00:02:10.000 --> 00:02:24.500\n遇到数据库连接超时先检查连接池上限而不是盲目调大重试次数。\n",
    )?;
    let added_video = add_file(&mut store, &video_path, &opts, None)?;
    assert!(!added_video.needs_ocr);
    let video_chunks = store.source_anchored_chunks(&added_video.source.source_id)?;
    assert_eq!(video_chunks.len(), 1);
    assert_eq!(video_chunks[0].anchor.start_ms, Some(130_000));
    assert_eq!(video_chunks[0].anchor.end_ms, Some(144_500));
    assert_eq!(
        video_chunks[0].anchor.file_path.as_deref(),
        Some(video_path.to_string_lossy().as_ref())
    );

    let unannotated_audio = dir.path().join("raw_voice.m4a");
    fs::write(&unannotated_audio, b"RAW_M4A_BYTES")?;
    let added_raw_audio = add_file(&mut store, &unannotated_audio, &opts, None)?;
    assert!(added_raw_audio.needs_ocr);
    assert_eq!(added_raw_audio.chunks, 0);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new().route(
        "/guide.html",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                "<html><head><title>Ops Guide</title></head><body><h2>Migration Safety</h2><p>Always pass <code>--schema</code> to migration CLI.</p></body></html>",
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await });
    let url = format!("http://{addr}/guide.html");

    std::env::remove_var("KP_ALLOW_LOCAL_URL_INGEST");
    assert!(
        add_url(&mut store, &url, &opts, false, None).await.is_err(),
        "loopback URL must be blocked by SSRF guard by default"
    );

    std::env::set_var("KP_ALLOW_LOCAL_URL_INGEST", "1");
    let added_url = add_url(&mut store, &url, &opts, false, None).await?;
    std::env::remove_var("KP_ALLOW_LOCAL_URL_INGEST");
    srv.abort();
    assert_eq!(added_url.chunks, 1);

    let unmined = store.unmined_dream_sources(&opts.actor)?;
    assert!(unmined
        .iter()
        .any(|s| s.modality == SourceModality::WebLink));
    assert!(unmined
        .iter()
        .any(|s| s.modality == SourceModality::TimedMedia));

    Ok(())
}

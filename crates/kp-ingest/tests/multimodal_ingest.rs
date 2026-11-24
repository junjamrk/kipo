mod support;

use std::fs;
use std::path::Path;

use axum::{http::header, routing::get, Router};
use kp_ingest::doc_router::{add_file, add_url, handles, parse};
use kp_ingest::IngestError;

use support::{library, options, Result};

#[tokio::test]
async fn media_and_subtitle_formats_are_rejected_and_json_with_segments_parses_uniformly() -> Result
{
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

    fs::write(
        dir.path().join("walkthrough.vtt"),
        "WEBVTT\n\n00:02:10.000 --> 00:02:24.500\nsidecar text\n",
    )?;
    for rejected_name in [
        "walkthrough.mp4",
        "raw_voice.m4a",
        "podcast.mp3",
        "walkthrough.vtt",
        "captions.srt",
        "meeting.lrc",
    ] {
        let path = dir.path().join(rejected_name);
        if !path.exists() {
            fs::write(&path, b"UNSUPPORTED_MEDIA_OR_SUBTITLE_PAYLOAD")?;
        }
        assert!(
            !handles(Path::new(rejected_name)),
            "handles({rejected_name}) must be false"
        );
        assert!(
            matches!(
                parse(&path, b"UNSUPPORTED_MEDIA_OR_SUBTITLE_PAYLOAD"),
                Err(IngestError::Invalid("unsupported document format"))
            ),
            "parse({rejected_name}) must fail with unsupported document format"
        );
        assert!(
            matches!(
                add_file(&mut store, &path, &opts, None),
                Err(IngestError::Invalid("unsupported document format"))
            ),
            "add_file({rejected_name}) must fail with unsupported document format"
        );
    }

    let json_payload = r#"{"segments":[{"start":12.4,"end":19.8,"speaker":"alice","text":"部署前必须执行 pnpm test 并确认退出码为 0。"}]}"#;
    let json_path = dir.path().join("recording.json");
    fs::write(&json_path, json_payload)?;
    let parsed_json = parse(&json_path, json_payload.as_bytes())?;
    assert_eq!(parsed_json.media_type, "application/json");
    assert_eq!(parsed_json.chunks.len(), 1);
    assert_eq!(parsed_json.chunks[0].body, json_payload);
    assert_eq!(parsed_json.chunks[0].anchor.start_ms, None);
    assert_eq!(parsed_json.chunks[0].anchor.end_ms, None);

    let added_json = add_file(&mut store, &json_path, &opts, None)?;
    let stored_json_chunks = store.source_anchored_chunks(&added_json.source.source_id)?;
    assert_eq!(stored_json_chunks.len(), 1);
    assert_eq!(stored_json_chunks[0].body, json_payload);
    assert!(!stored_json_chunks[0].body.contains("[speaker:alice]"));
    assert_eq!(stored_json_chunks[0].anchor.start_ms, None);
    assert_eq!(stored_json_chunks[0].anchor.end_ms, None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let json_for_srv = json_payload.to_owned();
    let app = Router::new()
        .route(
            "/guide.html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<html><head><title>Ops Guide</title></head><body><h2>Migration Safety</h2><p>Always pass <code>--schema</code> to migration CLI.</p></body></html>",
                )
            }),
        )
        .route(
            "/segments.json",
            get(move || {
                let body = json_for_srv.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], body) }
            }),
        )
        .route(
            "/captions.vtt",
            get(|| async { ([(header::CONTENT_TYPE, "text/vtt")], "WEBVTT\n") }),
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
    assert_eq!(added_url.chunks, 1);

    let remote_json = add_url(
        &mut store,
        &format!("http://{addr}/segments.json"),
        &opts,
        false,
        None,
    )
    .await?;
    let remote_json_chunks = store.source_anchored_chunks(&remote_json.source.source_id)?;
    assert_eq!(remote_json_chunks.len(), 1);
    assert_eq!(remote_json_chunks[0].body, json_payload);
    assert_eq!(remote_json_chunks[0].anchor.start_ms, None);

    assert!(matches!(
        add_url(
            &mut store,
            &format!("http://{addr}/captions.vtt"),
            &opts,
            false,
            None,
        )
        .await,
        Err(IngestError::Invalid("unsupported document format"))
    ));

    std::env::remove_var("KP_ALLOW_LOCAL_URL_INGEST");
    srv.abort();

    assert!(store.unmined_session_source_ids(&opts.actor)?.is_empty());
    Ok(())
}

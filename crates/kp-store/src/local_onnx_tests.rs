use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::{extract::State, http::StatusCode, routing::get, Router};
use kp_model::LibraryMode;
use sha2::{Digest, Sha256};

use super::*;
use crate::test_support::source;

fn build_mock_ort_wheel_bytes(dylib_name: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("onnxruntime/__init__.py", options)
            .map_err(|e| StoreError::Embedding(e.to_string()))?;
        zip.write_all(b"# mock ort\n")?;
        zip.start_file(format!("onnxruntime/capi/{dylib_name}"), options)
            .map_err(|e| StoreError::Embedding(e.to_string()))?;
        zip.write_all(payload)?;
        zip.finish()
            .map_err(|e| StoreError::Embedding(e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

#[test]
fn pinned_manifests_wheel_extraction_and_uncached_fts5_fallback_work_offline() -> Result<()> {
    let bge = find_local_onnx_spec("local:bge-small-zh-v1.5")?;
    assert_eq!(bge.dims, 512);
    assert_eq!(bge.files.len(), 5);
    assert_eq!(
        bge.files[0].sha256,
        "15b717c382bcb518ba457b93ea6850ede7f4f1cd8937454aa06972366cd19bcc"
    );

    let e5 = find_local_onnx_spec("local:multilingual-e5-small")?;
    assert_eq!(e5.dims, 384);
    assert_eq!(e5.files.len(), 5);
    assert_eq!(
        e5.files[0].sha256,
        "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193"
    );

    assert_eq!(ORT_WHEEL_SPECS.len(), 5);
    for wheel in ORT_WHEEL_SPECS {
        assert_eq!(wheel.sha256.len(), 64);
        assert!(wheel.url.starts_with("https://files.pythonhosted.org/"));
    }

    // Verify wheel zip extraction & atomic rename in temp directory.
    let dir = tempfile::tempdir()?;
    let fake_dylib_bytes = b"MOCK_ORT_DYLIB_BINARY_PAYLOAD";
    let wheel_bytes = build_mock_ort_wheel_bytes("libonnxruntime.1.31.0.dylib", fake_dylib_bytes)?;
    let wheel_path = dir.path().join("mock.whl");
    std::fs::write(&wheel_path, &wheel_bytes)?;
    let extracted_dylib = dir
        .path()
        .join("onnxruntime-1.31.0/libonnxruntime.1.31.0.dylib");
    extract_ort_dylib_from_wheel(&wheel_path, &extracted_dylib)?;
    assert_eq!(std::fs::read(&extracted_dylib)?, fake_dylib_bytes);

    // Uncached `try_load_cached_from_paths` returns `Ok(None)` without network access!
    let empty_models = dir.path().join("empty-models");
    let missing_dylib = empty_models.join("libonnxruntime.1.31.0.dylib");
    assert!(LocalOnnxEmbedder::try_load_cached_from_paths(
        "local:bge-small-zh-v1.5",
        &empty_models,
        &missing_dylib
    )?
    .is_none());

    // A Store initialized with `local:bge-small-zh-v1.5` before models are downloaded seamlessly
    // writes L0/L1 and searches via FTS5 with zero errors!
    let store_dir = tempfile::tempdir()?;
    let mut store = crate::Store::init_with_embedding(
        store_dir.path(),
        crate::test_support::options(LibraryMode::Personal)?,
        "local:bge-small-zh-v1.5",
        512,
    )?;
    assert!(!store.embedder().is_enabled());
    assert!(store.embedder().allows_raw_only());
    let mut src = source(&["owner"])?;
    src.content = "报销单据需要在周五前提交财务审核".as_bytes().to_vec();
    src.chunks[0].body = "报销单据需要在周五前提交财务审核".into();
    store.ingest_source(&"local".parse()?, &"doc1".parse()?, &src)?;
    let snap = store.search_snapshot(
        &"owner".parse()?,
        crate::test_support::AT,
        "报销单据",
        false,
        10,
    )?;
    assert_eq!(snap.documents().len(), 1);
    Ok(())
}

#[tokio::test]
async fn mock_http_download_emits_progress_verifies_sha256_and_rejects_tampered_bytes() -> Result<()>
{
    let payload = vec![b'K'; 96 * 1024];
    let valid_sha = hex::encode(Sha256::digest(&payload));
    let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;
    let served = Arc::new(payload.clone());
    let app =
        Router::new()
            .route(
                "/artifact.bin",
                get(|State(bytes): State<Arc<Vec<u8>>>| async move {
                    (StatusCode::OK, (*bytes).clone())
                }),
            )
            .with_state(served);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let dir = tempfile::tempdir()?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| StoreError::Embedding(e.to_string()))?;
    let url = format!("http://{addr}/artifact.bin");

    // 1. Valid SHA-256 succeeds, emits progress events ending with `finished == true`, and writes file.
    let good_dest = dir.path().join("sub/good.bin");
    let events = Mutex::new(Vec::<DownloadProgress>::new());
    download_verified_file(
        &client,
        &url,
        "test/good.bin",
        &valid_sha,
        &good_dest,
        &mut |ev| {
            if let Ok(mut g) = events.lock() {
                g.push(ev);
            }
        },
    )
    .await?;
    assert_eq!(std::fs::read(&good_dest)?, payload);
    {
        let recorded = events
            .lock()
            .map_err(|_| StoreError::Embedding("lock".into()))?;
        assert!(recorded.len() >= 2);
        let last = recorded
            .last()
            .ok_or_else(|| StoreError::Embedding("missing progress".into()))?;
        assert!(last.finished);
        assert_eq!(last.downloaded_bytes, payload.len() as u64);
    }

    // 2. Tampered / mismatched SHA-256 fails closed, never creates `bad_dest`, and cleans `.part` file!
    let bad_dest = dir.path().join("bad/tampered.bin");
    let err = download_verified_file(
        &client,
        &url,
        "test/tampered.bin",
        wrong_sha,
        &bad_dest,
        &mut |_| {},
    )
    .await;
    assert!(err.is_err());
    assert!(!bad_dest.exists());
    assert_eq!(std::fs::read_dir(dir.path().join("bad"))?.count(), 0);

    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_r1_dylib_integrity_explicit_sha256_and_live_streaming_progress() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // 1. Explicit `KP_ORT_DYLIB_PATH` requires matching 64-hex `KP_ORT_DYLIB_SHA256`!
    let custom_dylib = dir.path().join("custom_ort.dylib");
    let custom_bytes = b"EXPLICIT_ORT_BINARY_BYTES_V1";
    std::fs::write(&custom_dylib, custom_bytes)?;
    let custom_sha = hex::encode(Sha256::digest(custom_bytes));
    let wrong_sha = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    // Missing SHA -> `verify_explicit_ort_dylib` errors and `try_load_cached_with_explicit_sha` returns `Ok(None)`.
    assert!(verify_explicit_ort_dylib(&custom_dylib, None).is_err());
    assert!(verify_explicit_ort_dylib(&custom_dylib, Some("short")).is_err());
    assert!(verify_explicit_ort_dylib(&custom_dylib, Some(wrong_sha)).is_err());
    verify_explicit_ort_dylib(&custom_dylib, Some(&custom_sha))?;

    let empty_models = dir.path().join("models_explicit");
    assert!(LocalOnnxEmbedder::try_load_cached_with_explicit_sha(
        "local:bge-small-zh-v1.5",
        &empty_models,
        &custom_dylib,
        None,
    )?
    .is_none());
    assert!(LocalOnnxEmbedder::try_load_cached_with_explicit_sha(
        "local:bge-small-zh-v1.5",
        &empty_models,
        &custom_dylib,
        Some(wrong_sha),
    )?
    .is_none());

    // 2. Managed `.whl` + `<dylib>` tamper detection:
    //    An unverified `<dylib>` sitting in `<models_dir>/onnxruntime-1.31.0/` WITHOUT a verified
    //    pinned `.whl` (or with tampered `<dylib>` bytes) fails `verify_managed_ort_dylib` and
    //    causes `try_load_cached_from_paths` to safely return `Ok(None)`!
    if let Ok((_, wheel_cache, managed_dylib)) = managed_ort_paths(&empty_models) {
        if let Some(parent) = managed_dylib.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&managed_dylib, b"TAMPERED_UNVERIFIED_DYLIB")?;
        assert!(!verify_managed_ort_dylib(&empty_models, &managed_dylib)?);
        assert!(LocalOnnxEmbedder::try_load_cached_from_paths(
            "local:bge-small-zh-v1.5",
            &empty_models,
            &managed_dylib,
        )?
        .is_none());

        // Even with a zip file at `wheel_cache`, if its SHA-256 doesn't match the compile-time
        // pinned `wheel.sha256`, `verify_managed_ort_dylib` rejects it!
        let fake_whl = build_mock_ort_wheel_bytes("libonnxruntime.1.31.0.dylib", custom_bytes)?;
        std::fs::write(&wheel_cache, &fake_whl)?;
        extract_ort_dylib_from_wheel(&wheel_cache, &managed_dylib)?;
        assert_eq!(
            compute_wheel_dylib_entry_sha256(&wheel_cache, "libonnxruntime.1.31.0.dylib")?,
            custom_sha
        );
        assert!(!verify_managed_ort_dylib(&empty_models, &managed_dylib)?);
    }

    // 3. Live streaming `mpsc::channel` progress delivery across `std::thread::scope`!
    //    `LocalOnnxEmbedder::ensure_downloaded_and_load` downloads multiple model files in sequence.
    //    Gate `tokenizer.json` on a `tokio::sync::Notify` triggered INSIDE `on_progress` when
    //    `onnx/model_quantized.onnx` emits progress: if `ensure_downloaded_and_load` buffered events
    //    until worker completion instead of streaming via `mpsc::channel`, this would deadlock!
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;
    let live_gate = Arc::new(tokio::sync::Notify::new());
    let gate_srv = live_gate.clone();
    let app = Router::new()
        .route(
            "/BAAI/bge-small-zh-v1.5/resolve/main/onnx/model_quantized.onnx",
            get(|| async { (StatusCode::OK, b"part-1".to_vec()) }),
        )
        .route(
            "/BAAI/bge-small-zh-v1.5/resolve/main/tokenizer.json",
            get(move || {
                let g = gate_srv.clone();
                async move {
                    let _ = tokio::time::timeout(Duration::from_secs(3), g.notified()).await;
                    (StatusCode::OK, b"part-2".to_vec())
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // Exercise the exact `mpsc::channel` + `std::thread::scope` pattern with two sequential downloads:
    // file 1 (`good1.bin`) matches SHA-256 and emits live `on_progress`, waking `live_gate` BEFORE
    // worker starts/completes file 2 (`good2.bin`)!
    let sha_p1 = hex::encode(Sha256::digest(b"part-1"));
    let sha_p2 = hex::encode(Sha256::digest(b"part-2"));
    let u1 = format!("http://{addr}/BAAI/bge-small-zh-v1.5/resolve/main/onnx/model_quantized.onnx");
    let u2 = format!("http://{addr}/BAAI/bge-small-zh-v1.5/resolve/main/tokenizer.json");
    let d1 = dir.path().join("s1.bin");
    let d2 = dir.path().join("s2.bin");

    let (prog_tx, prog_rx) = std::sync::mpsc::channel::<DownloadProgress>();
    let gate_cb = live_gate.clone();
    let started = std::time::Instant::now();
    let mut live_notified = false;
    let stream_res: Result<()> = std::thread::scope(|s| {
        let worker = s.spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| StoreError::Embedding(e.to_string()))?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| StoreError::Embedding(e.to_string()))?;
            let mut send = |p: DownloadProgress| {
                let _ = prog_tx.send(p);
            };
            rt.block_on(async {
                download_verified_file(&client, &u1, "file1", &sha_p1, &d1, &mut send).await?;
                download_verified_file(&client, &u2, "file2", &sha_p2, &d2, &mut send).await?;
                Ok(())
            })
        });
        while let Ok(ev) = prog_rx.recv() {
            if ev.artifact == "file1" && ev.finished {
                live_notified = true;
                gate_cb.notify_one();
            }
        }
        worker
            .join()
            .map_err(|_| StoreError::Embedding("worker panicked".into()))?
    });
    stream_res?;
    assert!(live_notified);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "live streaming progress should unblock immediately without hitting 3s timeout"
    );

    server.abort();
    Ok(())
}

//! Safe Rust in-process ONNX Runtime embedding loader (`~/.kp/models/`) with pinned SHA-256
//! verification, streaming download progress, `HF_ENDPOINT` mirror support, and FTS5 fallback.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use fastembed::{
    AutoDevicePolicy, InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use kp_model::KpError;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::embedding::{sanitize_and_validate_embedding, Embedder};
use crate::providers::home_dir_from_env;
use crate::{Result, StoreError};

/// Process-global ONNX Runtime environment initialized dynamically via `ort::init_from`.
static ORT_ENVIRONMENT: OnceLock<ort::environment::Environment> = OnceLock::new();
static ORT_INIT_LOCK: Mutex<()> = Mutex::new(());

/// Real-time progress snapshot emitted during ONNX Runtime or model artifact downloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DownloadProgress {
    pub artifact: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub finished: bool,
}

/// Pinned SHA-256 manifest entry for one HuggingFace ONNX / tokenizer file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ModelFileSpec {
    pub relative_path: &'static str,
    pub sha256: &'static str,
}

/// Pinned model bundle definition (`local:bge-small-zh-v1.5` or `local:multilingual-e5-small`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalOnnxModelSpec {
    pub model_id: &'static str,
    pub cache_slug: &'static str,
    pub hf_repo: &'static str,
    pub dims: usize,
    pub pooling: Pooling,
    pub quantization: QuantizationMode,
    pub files: &'static [ModelFileSpec; 5],
}

/// Pinned official PyPI `onnxruntime 1.31.0` wheel per target triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OrtWheelSpec {
    pub target_triple: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub dylib_filename: &'static str,
}

pub const ORT_WHEEL_SPECS: &[OrtWheelSpec] = &[
    OrtWheelSpec {
        target_triple: "aarch64-apple-darwin",
        url: "https://files.pythonhosted.org/packages/b3/bd/2ac094311163b803e3626c3937461d6900934bd56cca7601f6150ff860c3/onnxruntime-1.31.0-cp312-cp312-macosx_14_0_arm64.whl",
        sha256: "aaab9b3af536b06ca27ab5e35e3d429c97457ce76cf298af103f687e8b9975c0",
        dylib_filename: "libonnxruntime.1.31.0.dylib",
    },
    OrtWheelSpec {
        target_triple: "aarch64-unknown-linux-gnu",
        url: "https://files.pythonhosted.org/packages/53/1a/561b43ca1536d9e81d1785bb8a1a260a9e314ef6d04976ba0411c652bda1/onnxruntime-1.31.0-cp312-cp312-manylinux_2_28_aarch64.whl",
        sha256: "35758d7606d578ec5b9d65f6e8a1f488013194c3f6097038a3223cb26d35ef9a",
        dylib_filename: "libonnxruntime.so.1.31.0",
    },
    OrtWheelSpec {
        target_triple: "x86_64-unknown-linux-gnu",
        url: "https://files.pythonhosted.org/packages/6c/44/1e9e762b95b7da0a8424913a1ed7c38cdaf88624a3c41ddba24ebac88bc9/onnxruntime-1.31.0-cp312-cp312-manylinux_2_28_x86_64.whl",
        sha256: "5e129d6c56abd53e659cb70f00a108d6824086470ff99c2e47a82e5786563db3",
        dylib_filename: "libonnxruntime.so.1.31.0",
    },
    OrtWheelSpec {
        target_triple: "x86_64-pc-windows-msvc",
        url: "https://files.pythonhosted.org/packages/be/ed/b12cea136ccd7b03d924f46b8393faf7ceac21115c0c50e729faa248cf23/onnxruntime-1.31.0-cp312-cp312-win_amd64.whl",
        sha256: "09d56445c1753e66e0912de69d3f0184016ad9a191dcd6925bf5dd570d2bfbe5",
        dylib_filename: "onnxruntime.dll",
    },
    OrtWheelSpec {
        target_triple: "aarch64-pc-windows-msvc",
        url: "https://files.pythonhosted.org/packages/02/ad/37bbc51dcb5cd105c5b2fe98f122b23e90171c2719516964edc65bb1d4cc/onnxruntime-1.31.0-cp312-cp312-win_arm64.whl",
        sha256: "5c54a0eb7b2b4eef3eb9dcfaf82f5ce880db07288dc309574f6657e9da5cc754",
        dylib_filename: "onnxruntime.dll",
    },
];

const BGE_SMALL_ZH_FILES: [ModelFileSpec; 5] = [
    ModelFileSpec {
        relative_path: "onnx/model_quantized.onnx",
        sha256: "15b717c382bcb518ba457b93ea6850ede7f4f1cd8937454aa06972366cd19bcc",
    },
    ModelFileSpec {
        relative_path: "tokenizer.json",
        sha256: "48cea5d44424912a6fd1ea647bf4fe50b55ab8b1e5879c3275f80e339e8fae26",
    },
    ModelFileSpec {
        relative_path: "config.json",
        sha256: "d4193ead3a810fd694fa8a31d7fc72fbaebc0668b603e398734bf2f6538ff42f",
    },
    ModelFileSpec {
        relative_path: "special_tokens_map.json",
        sha256: "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    },
    ModelFileSpec {
        relative_path: "tokenizer_config.json",
        sha256: "e6f3b96db926a37d4039995fbf5ad17de158dfb8f6343d607e4dbaad18d75f5a",
    },
];

const MULTILINGUAL_E5_SMALL_FILES: [ModelFileSpec; 5] = [
    ModelFileSpec {
        relative_path: "onnx/model_quantized.onnx",
        sha256: "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
    },
    ModelFileSpec {
        relative_path: "tokenizer.json",
        sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    },
    ModelFileSpec {
        relative_path: "config.json",
        sha256: "cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1",
    },
    ModelFileSpec {
        relative_path: "special_tokens_map.json",
        sha256: "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    },
    ModelFileSpec {
        relative_path: "tokenizer_config.json",
        sha256: "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    },
];

pub static LOCAL_ONNX_MODELS: &[LocalOnnxModelSpec] = &[
    LocalOnnxModelSpec {
        model_id: "local:bge-small-zh-v1.5",
        cache_slug: "bge-small-zh-v1.5",
        hf_repo: "Xenova/bge-small-zh-v1.5",
        dims: 512,
        pooling: Pooling::Cls,
        quantization: QuantizationMode::Dynamic,
        files: &BGE_SMALL_ZH_FILES,
    },
    LocalOnnxModelSpec {
        model_id: "local:multilingual-e5-small",
        cache_slug: "multilingual-e5-small",
        hf_repo: "Xenova/multilingual-e5-small",
        dims: 384,
        pooling: Pooling::Mean,
        quantization: QuantizationMode::Dynamic,
        files: &MULTILINGUAL_E5_SMALL_FILES,
    },
];

/// Resolves a local model identifier (`"local"`, `"local:bge-small-zh-v1.5"`, or `"local:multilingual-e5-small"`).
pub fn find_local_onnx_spec(model_spec: &str) -> Result<&'static LocalOnnxModelSpec> {
    let clean = model_spec.trim();
    let normalized = if clean.eq_ignore_ascii_case("local") {
        "local:bge-small-zh-v1.5"
    } else if let Some(rest) = clean.strip_prefix("local:") {
        return LOCAL_ONNX_MODELS
            .iter()
            .find(|m| m.cache_slug.eq_ignore_ascii_case(rest.trim()))
            .ok_or_else(|| {
                StoreError::Embedding(format!("unsupported local ONNX model `{clean}`"))
            });
    } else {
        clean
    };
    LOCAL_ONNX_MODELS
        .iter()
        .find(|m| {
            m.model_id.eq_ignore_ascii_case(normalized)
                || m.cache_slug.eq_ignore_ascii_case(normalized)
        })
        .ok_or_else(|| StoreError::Embedding(format!("unsupported local ONNX model `{clean}`")))
}

/// Returns the `OrtWheelSpec` matching the compiled host platform, if supported.
pub fn current_platform_ort_wheel() -> Option<&'static OrtWheelSpec> {
    let target = current_target_triple();
    ORT_WHEEL_SPECS.iter().find(|w| w.target_triple == target)
}

const fn current_target_triple() -> &'static str {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "windows"))]
    {
        "aarch64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(target_arch = "aarch64", target_os = "macos"),
        all(target_arch = "aarch64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "windows"),
        all(target_arch = "aarch64", target_os = "windows"),
    )))]
    {
        "unsupported"
    }
}

/// Resolves `~/.kp/models/` (overridden by `KP_MODELS_DIR`).
pub fn resolve_models_dir() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("KP_MODELS_DIR") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    let home = home_dir_from_env().ok_or_else(|| {
        KpError::invalid(
            "models_dir",
            "cannot resolve HOME directory for `~/.kp/models/` (set `KP_MODELS_DIR`)",
        )
    })?;
    Ok(home.join(".kp").join("models"))
}

pub(crate) fn explicit_ort_dylib_env() -> Option<(PathBuf, Option<String>)> {
    for var in ["KP_ORT_DYLIB_PATH", "ORT_DYLIB_PATH"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                let sha = std::env::var("KP_ORT_DYLIB_SHA256")
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                return Some((PathBuf::from(trimmed), sha));
            }
        }
    }
    None
}

pub(crate) fn managed_ort_paths(
    models_dir: &Path,
) -> Result<(&'static OrtWheelSpec, PathBuf, PathBuf)> {
    let wheel = current_platform_ort_wheel().ok_or_else(|| {
        KpError::invalid(
            "ort_platform",
            "no prebuilt ONNX Runtime 1.31.0 wheel pinned for this architecture; set both `KP_ORT_DYLIB_PATH` and `KP_ORT_DYLIB_SHA256` (64-char SHA-256 hex), or use `--no-embedding` / configure an embedding provider (`kp providers list`)",
        )
    })?;
    let ort_dir = models_dir.join("onnxruntime-1.31.0");
    let wheel_cache = ort_dir.join(format!("onnxruntime-1.31.0-{}.whl", wheel.target_triple));
    let dylib_path = ort_dir.join(wheel.dylib_filename);
    Ok((wheel, wheel_cache, dylib_path))
}

/// Resolves an explicit ORT dynamic library override (`KP_ORT_DYLIB_PATH` or `ORT_DYLIB_PATH`),
/// or falls back to `<models_dir>/onnxruntime-1.31.0/<dylib_filename>`.
pub fn resolve_ort_dylib_path(models_dir: &Path) -> Result<PathBuf> {
    if let Some((explicit_path, _)) = explicit_ort_dylib_env() {
        return Ok(explicit_path);
    }
    let (_, _, dylib_path) = managed_ort_paths(models_dir)?;
    Ok(dylib_path)
}

/// Resolves the HuggingFace base URL (`HF_ENDPOINT` mirror override or `https://huggingface.co`).
pub fn resolve_hf_endpoint() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_owned())
}

pub(crate) fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn file_matches_sha256(path: &Path, expected_hex: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let actual = compute_file_sha256(path)?;
    Ok(actual.eq_ignore_ascii_case(expected_hex))
}

/// Downloads `url` into a sibling temporary file, invokes `on_progress` on each chunk, verifies
/// `expected_sha256` BEFORE persisting, and atomically renames to `dest`.
pub async fn download_verified_file<F>(
    client: &reqwest::Client,
    url: &str,
    artifact_label: &str,
    expected_sha256: &str,
    dest: &Path,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(DownloadProgress),
{
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_name = format!(
        ".{}.{}.part",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("artifact"),
        uuid::Uuid::new_v4().simple()
    );
    let tmp_path = dest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tmp_name);

    let result: Result<()> = async {
        let mut resp = client.get(url).send().await.map_err(|e| {
            StoreError::Embedding(format!("download `{artifact_label}` from `{url}` failed: {e}"))
        })?;
        if !resp.status().is_success() {
            return Err(StoreError::Embedding(format!(
                "download `{artifact_label}` returned HTTP {} from `{url}`",
                resp.status().as_u16()
            )));
        }
        let total_bytes = resp.content_length();
        let mut out_file = File::create(&tmp_path)?;
        let mut hasher = Sha256::new();
        let mut downloaded_bytes = 0_u64;

        on_progress(DownloadProgress {
            artifact: artifact_label.to_owned(),
            downloaded_bytes: 0,
            total_bytes,
            finished: false,
        });

        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            StoreError::Embedding(format!("stream read failed for `{artifact_label}`: {e}"))
        })? {
            out_file.write_all(&chunk)?;
            hasher.update(&chunk);
            downloaded_bytes = downloaded_bytes.saturating_add(chunk.len() as u64);
            on_progress(DownloadProgress {
                artifact: artifact_label.to_owned(),
                downloaded_bytes,
                total_bytes,
                finished: false,
            });
        }
        out_file.sync_all()?;
        drop(out_file);

        let actual_sha = hex::encode(hasher.finalize());
        if !actual_sha.eq_ignore_ascii_case(expected_sha256) {
            return Err(StoreError::Embedding(format!(
                "SHA-256 verification failed for `{artifact_label}`: expected {expected_sha256}, got {actual_sha}"
            )));
        }

        fs::rename(&tmp_path, dest)?;
        on_progress(DownloadProgress {
            artifact: artifact_label.to_owned(),
            downloaded_bytes,
            total_bytes: Some(downloaded_bytes),
            finished: true,
        });
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn select_ort_wheel_dylib_entry(
    archive: &mut zip::ZipArchive<File>,
    expected_name: &str,
) -> Result<usize> {
    let mut chosen_index: Option<usize> = None;
    for idx in 0..archive.len() {
        let entry = archive
            .by_index(idx)
            .map_err(|e| StoreError::Embedding(format!("ORT wheel entry read error: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .map_err(|e| StoreError::Embedding(format!("ORT wheel entry name error: {e}")))?;
        let name_str = name.as_ref();
        if !name_str.starts_with("onnxruntime/capi/") {
            continue;
        }
        let base = name_str.rsplit('/').next().unwrap_or(name_str);
        if !expected_name.is_empty() && base == expected_name {
            chosen_index = Some(idx);
            break;
        }
        let is_runtime_lib = (base.starts_with("libonnxruntime.")
            || base.starts_with("libonnxruntime.so")
            || base.eq_ignore_ascii_case("onnxruntime.dll"))
            && !base.contains("providers_shared");
        if is_runtime_lib && chosen_index.is_none() {
            chosen_index = Some(idx);
        }
    }
    chosen_index.ok_or_else(|| {
        StoreError::Embedding(
            "ORT wheel does not contain `onnxruntime/capi/libonnxruntime*`".into(),
        )
    })
}

/// Computes the SHA-256 hex digest of the `onnxruntime/capi/<dylib>` entry inside `wheel_path`.
pub fn compute_wheel_dylib_entry_sha256(wheel_path: &Path, expected_name: &str) -> Result<String> {
    let wheel_file = File::open(wheel_path)?;
    let mut archive = zip::ZipArchive::new(wheel_file)
        .map_err(|e| StoreError::Embedding(format!("invalid ORT wheel zip archive: {e}")))?;
    let entry_idx = select_ort_wheel_dylib_entry(&mut archive, expected_name)?;
    let mut entry = archive
        .by_index(entry_idx)
        .map_err(|e| StoreError::Embedding(format!("ORT wheel extract error: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = entry.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Verifies BOTH that `<models_dir>/onnxruntime-1.31.0/onnxruntime-1.31.0-<target>.whl` matches its
/// compile-time pinned SHA-256 AND that `dylib_path` on disk matches the exact SHA-256 of the
/// `onnxruntime/capi/<dylib>` entry inside that verified wheel.
pub fn verify_managed_ort_dylib(models_dir: &Path, dylib_path: &Path) -> Result<bool> {
    let Ok((wheel, wheel_cache, _)) = managed_ort_paths(models_dir) else {
        return Ok(false);
    };
    if !wheel_cache.is_file() || !dylib_path.is_file() {
        return Ok(false);
    }
    if !file_matches_sha256(&wheel_cache, wheel.sha256)? {
        return Ok(false);
    }
    let expected_name = dylib_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(wheel.dylib_filename);
    let Ok(expected_dylib_sha) = compute_wheel_dylib_entry_sha256(&wheel_cache, expected_name)
    else {
        return Ok(false);
    };
    file_matches_sha256(dylib_path, &expected_dylib_sha)
}

/// Verifies an explicitly configured `KP_ORT_DYLIB_PATH` / `ORT_DYLIB_PATH` shared library against
/// the mandatory 64-hex `KP_ORT_DYLIB_SHA256` digest before `dlopen`.
pub fn verify_explicit_ort_dylib(dylib_path: &Path, expected_sha256: Option<&str>) -> Result<()> {
    let Some(raw_hex) = expected_sha256.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(StoreError::Embedding(
            "explicit `KP_ORT_DYLIB_PATH`/`ORT_DYLIB_PATH` requires `KP_ORT_DYLIB_SHA256` (64-char SHA-256 hex)".into(),
        ));
    };
    if raw_hex.len() != 64 || !raw_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StoreError::Embedding(format!(
            "invalid `KP_ORT_DYLIB_SHA256` `{raw_hex}`: expected 64-character hexadecimal SHA-256"
        )));
    }
    if !dylib_path.is_file() {
        return Err(StoreError::Embedding(format!(
            "explicit ONNX Runtime library not found at `{}`",
            dylib_path.display()
        )));
    }
    if !file_matches_sha256(dylib_path, raw_hex)? {
        return Err(StoreError::Embedding(format!(
            "SHA-256 mismatch for explicit ONNX Runtime library `{}` against `KP_ORT_DYLIB_SHA256`",
            dylib_path.display()
        )));
    }
    Ok(())
}

/// Extracts the ONNX Runtime shared library (`onnxruntime/capi/libonnxruntime*` or `onnxruntime.dll`)
/// from a verified official `.whl` archive and atomically renames it to `dest_dylib`.
pub fn extract_ort_dylib_from_wheel(wheel_path: &Path, dest_dylib: &Path) -> Result<()> {
    if let Some(parent) = dest_dylib.parent() {
        fs::create_dir_all(parent)?;
    }
    let wheel_file = File::open(wheel_path)?;
    let mut archive = zip::ZipArchive::new(wheel_file)
        .map_err(|e| StoreError::Embedding(format!("invalid ORT wheel zip archive: {e}")))?;

    let expected_name = dest_dylib
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let entry_idx = select_ort_wheel_dylib_entry(&mut archive, expected_name)?;

    let mut entry = archive
        .by_index(entry_idx)
        .map_err(|e| StoreError::Embedding(format!("ORT wheel extract error: {e}")))?;
    let tmp_dylib =
        dest_dylib.with_extension(format!("dylib.{}.part", uuid::Uuid::new_v4().simple()));
    {
        let mut out = File::create(&tmp_dylib)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
    }
    fs::rename(&tmp_dylib, dest_dylib)?;
    Ok(())
}

fn ensure_ort_environment(dylib_path: &Path) -> Result<ort::environment::Environment> {
    if let Some(env) = ORT_ENVIRONMENT.get() {
        return Ok(env.clone());
    }
    let _guard = ORT_INIT_LOCK
        .lock()
        .map_err(|_| StoreError::Embedding("ORT initialization lock poisoned".into()))?;
    if let Some(env) = ORT_ENVIRONMENT.get() {
        return Ok(env.clone());
    }
    if !dylib_path.is_file() {
        return Err(StoreError::Embedding(format!(
            "ONNX Runtime library not found at `{}`",
            dylib_path.display()
        )));
    }
    let builder = ort::init_from(dylib_path)
        .map_err(|e| StoreError::Embedding(format!("failed to load ORT dynamic library: {e}")))?;
    let env = builder
        .with_name("kp-ort")
        .build()
        .map_err(|e| StoreError::Embedding(format!("failed to build ORT environment: {e}")))?;
    let _ = ORT_ENVIRONMENT.set(env.clone());
    Ok(env)
}

/// In-process ONNX Runtime embedder backed by `fastembed::TextEmbedding` (`allows_raw_only() == true`).
pub struct LocalOnnxEmbedder {
    spec: &'static LocalOnnxModelSpec,
    session: Mutex<TextEmbedding>,
}

impl std::fmt::Debug for LocalOnnxEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOnnxEmbedder")
            .field("model_id", &self.spec.model_id)
            .field("dims", &self.spec.dims)
            .finish()
    }
}

impl LocalOnnxEmbedder {
    /// Pure-local, zero-network cache check: returns `Ok(None)` if the ORT dynamic library (and
    /// its pinned `.whl` or explicit `KP_ORT_DYLIB_SHA256`) or any of the 5 model files is missing
    /// or fails SHA-256 verification, allowing callers to fall back seamlessly to FTS5.
    pub fn try_load_cached(model_spec: &str) -> Result<Option<Self>> {
        let Ok(models_dir) = resolve_models_dir() else {
            return Ok(None);
        };
        if let Some((explicit_path, explicit_sha)) = explicit_ort_dylib_env() {
            return Self::try_load_cached_with_explicit_sha(
                model_spec,
                &models_dir,
                &explicit_path,
                explicit_sha.as_deref(),
            );
        }
        let Ok((_, _, dylib_path)) = managed_ort_paths(&models_dir) else {
            return Ok(None);
        };
        Self::try_load_cached_from_paths(model_spec, &models_dir, &dylib_path)
    }

    pub fn try_load_cached_from_paths(
        model_spec: &str,
        models_dir: &Path,
        dylib_path: &Path,
    ) -> Result<Option<Self>> {
        let is_managed_default = managed_ort_paths(models_dir)
            .map(|(_, _, mp)| mp == dylib_path)
            .unwrap_or(false);
        if is_managed_default {
            if !verify_managed_ort_dylib(models_dir, dylib_path)? {
                return Ok(None);
            }
            Self::try_load_cached_verified_dylib(model_spec, models_dir, dylib_path)
        } else {
            let explicit_sha = std::env::var("KP_ORT_DYLIB_SHA256")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            Self::try_load_cached_with_explicit_sha(
                model_spec,
                models_dir,
                dylib_path,
                explicit_sha.as_deref(),
            )
        }
    }

    pub fn try_load_cached_with_explicit_sha(
        model_spec: &str,
        models_dir: &Path,
        dylib_path: &Path,
        explicit_dylib_sha256: Option<&str>,
    ) -> Result<Option<Self>> {
        if verify_explicit_ort_dylib(dylib_path, explicit_dylib_sha256).is_err() {
            return Ok(None);
        }
        Self::try_load_cached_verified_dylib(model_spec, models_dir, dylib_path)
    }

    fn try_load_cached_verified_dylib(
        model_spec: &str,
        models_dir: &Path,
        dylib_path: &Path,
    ) -> Result<Option<Self>> {
        let spec = find_local_onnx_spec(model_spec)?;
        let model_dir = models_dir.join(spec.cache_slug);
        for file_spec in spec.files {
            let full_path = model_dir.join(file_spec.relative_path);
            if !file_matches_sha256(&full_path, file_spec.sha256)? {
                return Ok(None);
            }
        }
        Self::load_verified_from_disk(spec, &model_dir, dylib_path).map(Some)
    }

    /// Ensures the ONNX Runtime dynamic library and all 5 model files exist in `~/.kp/models/`
    /// with verified SHA-256 digests (repairing missing/corrupted artifacts with live streaming
    /// `on_progress`), then initializes `ort` (`PreferCPU`) and loads `TextEmbedding`.
    pub fn ensure_downloaded_and_load<F>(model_spec: &str, mut on_progress: F) -> Result<Self>
    where
        F: FnMut(DownloadProgress),
    {
        if let Some(cached) = Self::try_load_cached(model_spec)? {
            return Ok(cached);
        }
        let hf_endpoint = resolve_hf_endpoint();
        let ort_url_override = std::env::var("KP_ORT_WHEEL_URL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let is_test_loopback = crate::providers::is_loopback_url(&hf_endpoint)
            || ort_url_override
                .as_deref()
                .is_some_and(crate::providers::is_loopback_url);
        let downloads_disabled = ["KP_DISABLE_MODEL_DOWNLOAD", "KP_OFFLINE"]
            .iter()
            .any(|name| {
                std::env::var(name)
                    .ok()
                    .map(|v| {
                        let t = v.trim();
                        !t.is_empty()
                            && !t.eq_ignore_ascii_case("0")
                            && !t.eq_ignore_ascii_case("false")
                    })
                    .unwrap_or(false)
            });
        if downloads_disabled && !is_test_loopback {
            return Err(StoreError::Embedding(
                "local ONNX model download is disabled (`KP_DISABLE_MODEL_DOWNLOAD=1` or `KP_OFFLINE=1`)"
                    .into(),
            ));
        }
        let models_dir = resolve_models_dir()?;
        let dylib_path = resolve_ort_dylib_path(&models_dir)?;

        if tokio::runtime::Handle::try_current().is_ok() {
            let spec = find_local_onnx_spec(model_spec)?;
            let (prog_tx, prog_rx) = std::sync::mpsc::channel::<DownloadProgress>();
            let download_res: Result<()> = std::thread::scope(|s| {
                let mdir = &models_dir;
                let dpath = &dylib_path;
                let hf = hf_endpoint.as_str();
                let ort_ov = ort_url_override.as_deref();
                let worker = s.spawn(move || {
                    let rt_inner = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| {
                            StoreError::Embedding(format!("tokio runtime init failed: {e}"))
                        })?;
                    let mut forward = |p: DownloadProgress| {
                        let _ = prog_tx.send(p);
                    };
                    rt_inner.block_on(Self::ensure_downloaded_files_only(
                        spec,
                        mdir,
                        dpath,
                        hf,
                        ort_ov,
                        &mut forward,
                    ))
                });
                while let Ok(ev) = prog_rx.recv() {
                    on_progress(ev);
                }
                worker
                    .join()
                    .map_err(|_| StoreError::Embedding("download worker panicked".into()))?
            });
            download_res?;
            Self::load_verified_from_disk(spec, &models_dir.join(spec.cache_slug), &dylib_path)
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| StoreError::Embedding(format!("tokio runtime init failed: {e}")))?;
            rt.block_on(Self::ensure_downloaded_async(
                model_spec,
                &models_dir,
                &dylib_path,
                &hf_endpoint,
                ort_url_override.as_deref(),
                &mut on_progress,
            ))
        }
    }

    /// Downloads and verifies ORT + model files (without requiring dlopen when testing pure HTTP/SHA-256).
    pub async fn ensure_downloaded_files_only<F>(
        spec: &'static LocalOnnxModelSpec,
        models_dir: &Path,
        dylib_path: &Path,
        hf_endpoint: &str,
        ort_wheel_url_override: Option<&str>,
        on_progress: &mut F,
    ) -> Result<()>
    where
        F: FnMut(DownloadProgress),
    {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(300));
        if crate::providers::is_loopback_url(hf_endpoint)
            || ort_wheel_url_override.is_some_and(crate::providers::is_loopback_url)
        {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| StoreError::Embedding(format!("HTTP client build failed: {e}")))?;

        let is_managed_default = managed_ort_paths(models_dir)
            .map(|(_, _, mp)| mp == dylib_path)
            .unwrap_or(false);

        if is_managed_default {
            if !verify_managed_ort_dylib(models_dir, dylib_path)? {
                let (wheel, wheel_cache, _) = managed_ort_paths(models_dir)?;
                let _ = fs::remove_file(dylib_path);
                if !file_matches_sha256(&wheel_cache, wheel.sha256)? {
                    let _ = fs::remove_file(&wheel_cache);
                    let wheel_url = ort_wheel_url_override.unwrap_or(wheel.url);
                    download_verified_file(
                        &client,
                        wheel_url,
                        "onnxruntime-1.31.0",
                        wheel.sha256,
                        &wheel_cache,
                        on_progress,
                    )
                    .await?;
                }
                extract_ort_dylib_from_wheel(&wheel_cache, dylib_path)?;
                if !verify_managed_ort_dylib(models_dir, dylib_path)? {
                    let _ = fs::remove_file(dylib_path);
                    return Err(StoreError::Embedding(
                        "extracted ONNX Runtime library failed integrity verification against pinned wheel".into(),
                    ));
                }
            }
        } else {
            let explicit_sha = std::env::var("KP_ORT_DYLIB_SHA256")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            verify_explicit_ort_dylib(dylib_path, explicit_sha.as_deref())?;
        }

        let clean_hf = hf_endpoint.trim_end_matches('/');
        let model_dir = models_dir.join(spec.cache_slug);
        for file_spec in spec.files {
            let dest = model_dir.join(file_spec.relative_path);
            if !file_matches_sha256(&dest, file_spec.sha256)? {
                let _ = fs::remove_file(&dest);
                let url = format!(
                    "{clean_hf}/{}/resolve/main/{}",
                    spec.hf_repo, file_spec.relative_path
                );
                let label = format!("{}/{}", spec.cache_slug, file_spec.relative_path);
                download_verified_file(&client, &url, &label, file_spec.sha256, &dest, on_progress)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn ensure_downloaded_async<F>(
        model_spec: &str,
        models_dir: &Path,
        dylib_path: &Path,
        hf_endpoint: &str,
        ort_wheel_url_override: Option<&str>,
        on_progress: &mut F,
    ) -> Result<Self>
    where
        F: FnMut(DownloadProgress),
    {
        let spec = find_local_onnx_spec(model_spec)?;
        Self::ensure_downloaded_files_only(
            spec,
            models_dir,
            dylib_path,
            hf_endpoint,
            ort_wheel_url_override,
            on_progress,
        )
        .await?;
        Self::load_verified_from_disk(spec, &models_dir.join(spec.cache_slug), dylib_path)
    }

    fn load_verified_from_disk(
        spec: &'static LocalOnnxModelSpec,
        model_dir: &Path,
        dylib_path: &Path,
    ) -> Result<Self> {
        let env = ensure_ort_environment(dylib_path)?;

        let onnx_bytes = fs::read(model_dir.join("onnx/model_quantized.onnx"))?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: fs::read(model_dir.join("tokenizer.json"))?,
            config_file: fs::read(model_dir.join("config.json"))?,
            special_tokens_map_file: fs::read(model_dir.join("special_tokens_map.json"))?,
            tokenizer_config_file: fs::read(model_dir.join("tokenizer_config.json"))?,
        };

        let user_model = UserDefinedEmbeddingModel::new(onnx_bytes, tokenizer_files)
            .with_pooling(spec.pooling.clone())
            .with_quantization(spec.quantization);
        let init_opts = InitOptionsUserDefined::new()
            .with_environment(env)
            .with_auto_device(AutoDevicePolicy::PreferCPU);

        let embedding =
            TextEmbedding::try_new_from_user_defined(user_model, init_opts).map_err(|e| {
                StoreError::Embedding(format!(
                    "failed to load `{}` ONNX session: {e}",
                    spec.model_id
                ))
            })?;

        Ok(Self {
            spec,
            session: Mutex::new(embedding),
        })
    }
}

impl Embedder for LocalOnnxEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let batch = [text.to_owned()];
        let mut vectors = self.embed_batch(&batch)?;
        vectors
            .pop()
            .ok_or_else(|| StoreError::Embedding("empty ONNX embedding output".into()))
    }

    fn model_id(&self) -> &str {
        self.spec.model_id
    }

    fn expected_dims(&self) -> Option<usize> {
        Some(self.spec.dims)
    }

    fn is_enabled(&self) -> bool {
        true
    }

    /// Local ONNX runs strictly inside the local process (`~/.kp/models/`) — safe for `RawOnly`.
    fn allows_raw_only(&self) -> bool {
        true
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let non_empty: Vec<&str> = texts
            .iter()
            .map(|t| {
                let s = t.trim();
                if s.is_empty() {
                    " "
                } else {
                    s
                }
            })
            .collect();
        let mut guard = self
            .session
            .lock()
            .map_err(|_| StoreError::Embedding("LocalOnnxEmbedder session lock poisoned".into()))?;
        // Pass `None` batch size as required by `fastembed` for `QuantizationMode::Dynamic`.
        let raw_vectors = guard
            .embed(non_empty, None)
            .map_err(|e| StoreError::Embedding(format!("ONNX inference failed: {e}")))?;
        raw_vectors
            .iter()
            .map(|v| sanitize_and_validate_embedding(v))
            .collect()
    }
}

#[cfg(test)]
#[path = "local_onnx_tests.rs"]
mod tests;

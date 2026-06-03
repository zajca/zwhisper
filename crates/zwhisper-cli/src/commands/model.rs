//! `zwhisper model …` — manage local models across every backend.
//!
//! Registry-driven (RFC: Model Source Model). The embedded
//! `ModelRegistry` enumerates single-file (whisper.cpp), directory-
//! bundle (Parakeet), and remote (Deepgram) models; this command lists
//! their install status, resolves paths, and installs them — single
//! files via the legacy whisper downloader, directory bundles via the
//! hardened `BundleInstaller`.

use color_eyre::eyre::{WrapErr, eyre};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zwhisper_core::transcribe::bundle_download::{BundleInstaller, BundleProgress};
use zwhisper_core::transcribe::model::{ModelKind, ModelSpec, ModelStatus};
use zwhisper_core::transcribe::model_management::{
    DownloadState, FailReason, ModelDownloader, ModelManifest, ModelSourceConfig, verify_model,
};
use zwhisper_core::transcribe::models::models_dir;
use zwhisper_core::transcribe::registry;

use crate::cli::ModelCmd;

use super::build_runtime;

pub(crate) fn run(cmd: &ModelCmd) -> color_eyre::Result<()> {
    match cmd {
        ModelCmd::List => list(),
        ModelCmd::Path { model } => path(model.as_deref()),
        ModelCmd::Install { model } => install(model),
        ModelCmd::Verify { model } => verify(model),
    }
}

fn list() -> color_eyre::Result<()> {
    let reg = registry::embedded();
    let dir = models_dir().map_err(|e| eyre!("{e}"))?;

    println!(
        "{:<26}  {:<11}  {:<16}  {:<10}  path",
        "model", "backend", "kind", "status"
    );
    println!("{}", "-".repeat(100));
    for spec in reg.specs() {
        let status = reg.status(spec, &dir);
        let path = describe_path(spec, &dir);
        println!(
            "{:<26}  {:<11}  {:<16}  {:<10}  {}",
            spec.id,
            spec.backend.as_str(),
            kind_label(&spec.kind),
            status_label(&status),
            path,
        );
    }
    Ok(())
}

fn path(model: Option<&str>) -> color_eyre::Result<()> {
    let dir = models_dir().map_err(|e| eyre!("{e}"))?;
    match model {
        None => {
            println!("{}", dir.display());
            Ok(())
        }
        Some(id) => {
            let reg = registry::embedded();
            let spec = reg
                .find_by_id(id)
                .ok_or_else(|| eyre!("unknown model `{id}`; run `zwhisper model list`"))?;
            println!("{}", describe_path(spec, &dir));
            Ok(())
        }
    }
}

fn install(model: &str) -> color_eyre::Result<()> {
    let reg = registry::embedded();
    let spec = reg
        .find_by_id(model)
        .ok_or_else(|| eyre!("unknown model `{model}`; run `zwhisper model list`"))?;
    match &spec.kind {
        ModelKind::SingleFile { .. } => install_single_file(&spec.id),
        ModelKind::DirectoryBundle { .. } => install_bundle(spec),
        ModelKind::Remote => {
            println!(
                "`{}` is a remote model managed by `{}`; nothing to install locally.",
                spec.id,
                spec.backend.as_str()
            );
            Ok(())
        }
    }
}

/// whisper.cpp single-file install via the legacy manifest downloader
/// (unchanged behaviour; the manifest is the SHA-256 source of truth).
fn install_single_file(model: &str) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(async {
        let manifest = ModelManifest::embedded();
        let config = ModelSourceConfig::load_or_default(None).map_err(|e| eyre!("{e}"))?;
        let dir = models_dir().map_err(|e| eyre!("{e}"))?;
        let cancel = CancellationToken::new();
        let mut downloader = ModelDownloader::new(model.to_owned(), &config, manifest, dir, cancel)
            .map_err(|e| eyre!("{e}"))?;
        let final_path = downloader.final_path().to_path_buf();

        let (tx, rx) = mpsc::unbounded_channel();
        let progress = tokio::spawn(report_progress(model.to_owned(), rx));
        let state = downloader
            .run(tx)
            .await
            .wrap_err("model installer failed")?;
        let _ = progress.await;

        match state {
            DownloadState::Installed => {
                info!(model, path = %final_path.display(), "model installed");
                println!("installed {model}: {}", final_path.display());
                Ok(())
            }
            DownloadState::Cancelled => Err(eyre!("model install cancelled")),
            DownloadState::Failed { reason } => {
                Err(eyre!("model install failed: {}", reason_text(&reason)))
            }
            other => Err(eyre!("model install ended before completion: {other:?}")),
        }
    })
}

/// Directory-bundle install via the hardened [`BundleInstaller`]
/// (verify-before-extract, atomic same-filesystem directory swap).
fn install_bundle(spec: &ModelSpec) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(async {
        let dir = models_dir().map_err(|e| eyre!("{e}"))?;
        let cancel = CancellationToken::new();
        let installer = BundleInstaller::for_spec(spec, dir, cancel).map_err(|e| eyre!("{e}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        let model_id = spec.id.clone();
        let progress = tokio::spawn(report_bundle_progress(model_id.clone(), rx));
        let result = installer.install(Some(tx)).await;
        let _ = progress.await;
        match result {
            Ok(final_dir) => {
                info!(model = %model_id, path = %final_dir.display(), "bundle installed");
                println!("installed {model_id}: {}", final_dir.display());
                Ok(())
            }
            Err(e) => Err(eyre!("bundle install failed: {e}")),
        }
    })
}

fn verify(model: &str) -> color_eyre::Result<()> {
    let reg = registry::embedded();
    let spec = reg
        .find_by_id(model)
        .ok_or_else(|| eyre!("unknown model `{model}`; run `zwhisper model list`"))?;
    match &spec.kind {
        ModelKind::SingleFile { .. } => {
            let verification = verify_model(&spec.id).map_err(|e| eyre!("{e}"))?;
            if verification.is_match() {
                println!("OK {model}: {}", verification.path.display());
                Ok(())
            } else {
                Err(eyre!(
                    "model verification failed for {model}: expected {} ({}), got {} ({}) at {}",
                    verification.expected_sha256,
                    format_size(verification.expected_size_bytes),
                    verification.actual_sha256,
                    format_size(verification.actual_size_bytes),
                    verification.path.display()
                ))
            }
        }
        ModelKind::DirectoryBundle { .. } => {
            let dir = models_dir().map_err(|e| eyre!("{e}"))?;
            match reg.status(spec, &dir) {
                ModelStatus::Installed => {
                    println!("OK {model}: {}", describe_path(spec, &dir));
                    Ok(())
                }
                ModelStatus::Missing => Err(eyre!("model `{model}` is not installed")),
                ModelStatus::Corrupt { detail } => {
                    Err(eyre!("model `{model}` is corrupt: {detail}"))
                }
                ModelStatus::RemoteManaged => Ok(()),
            }
        }
        ModelKind::Remote => {
            println!("`{model}` is a remote model; nothing to verify locally.");
            Ok(())
        }
    }
}

fn describe_path(spec: &ModelSpec, models_dir: &std::path::Path) -> String {
    match &spec.kind {
        ModelKind::SingleFile { file_name } => models_dir.join(file_name).display().to_string(),
        ModelKind::DirectoryBundle { dir_name, .. } => {
            models_dir.join(dir_name).display().to_string()
        }
        ModelKind::Remote => format!("(remote: {})", spec.backend.as_str()),
    }
}

fn kind_label(kind: &ModelKind) -> &'static str {
    match kind {
        ModelKind::SingleFile { .. } => "single-file",
        ModelKind::DirectoryBundle { .. } => "directory-bundle",
        ModelKind::Remote => "remote",
    }
}

fn status_label(status: &ModelStatus) -> &'static str {
    match status {
        ModelStatus::Installed => "installed",
        ModelStatus::Missing => "missing",
        ModelStatus::Corrupt { .. } => "corrupt",
        ModelStatus::RemoteManaged => "remote",
    }
}

#[allow(clippy::print_stderr)]
async fn report_progress(model: String, mut rx: mpsc::UnboundedReceiver<DownloadState>) {
    let mut last_bucket: Option<u64> = None;
    while let Some(state) = rx.recv().await {
        match state {
            DownloadState::Resolving => eprintln!("{model}: resolving source"),
            DownloadState::Fetching { bytes_done, total } => {
                let percent = if total == 0 {
                    0
                } else {
                    bytes_done.saturating_mul(100) / total
                };
                let bucket = percent / 5;
                if last_bucket != Some(bucket) || percent == 100 {
                    last_bucket = Some(bucket);
                    eprintln!(
                        "{model}: downloading {percent}% ({}/{})",
                        format_size(bytes_done),
                        format_size(total)
                    );
                }
            }
            DownloadState::Verifying => eprintln!("{model}: verifying SHA-256"),
            DownloadState::Installed => eprintln!("{model}: finalized"),
            DownloadState::Failed { reason } => {
                eprintln!("{model}: failed: {}", reason_text(&reason));
            }
            DownloadState::Cancelled => eprintln!("{model}: cancelled"),
        }
    }
}

#[allow(clippy::print_stderr)]
async fn report_bundle_progress(model: String, mut rx: mpsc::UnboundedReceiver<BundleProgress>) {
    let mut last_bucket: Option<(String, u64)> = None;
    while let Some(p) = rx.recv().await {
        match p {
            BundleProgress::Resolving => eprintln!("{model}: resolving bundle"),
            BundleProgress::Fetching {
                file,
                bytes_done,
                total,
            } => {
                let percent = if total == 0 {
                    0
                } else {
                    bytes_done.saturating_mul(100) / total
                };
                let bucket = (file.clone(), percent / 5);
                if last_bucket.as_ref() != Some(&bucket) || percent == 100 {
                    last_bucket = Some(bucket);
                    eprintln!(
                        "{model}: {file} {percent}% ({}/{})",
                        format_size(bytes_done),
                        format_size(total)
                    );
                }
            }
            BundleProgress::Verifying { file } => eprintln!("{model}: verifying {file}"),
            BundleProgress::Extracting => eprintln!("{model}: extracting"),
            BundleProgress::Installing => eprintln!("{model}: installing"),
            BundleProgress::Installed => eprintln!("{model}: finalized"),
            BundleProgress::Cancelled => eprintln!("{model}: cancelled"),
        }
    }
}

fn reason_text(reason: &FailReason) -> String {
    match reason {
        FailReason::ContentTypeMismatch(value) => {
            format!("unexpected content type {value:?}")
        }
        FailReason::ContentLengthMismatch { expected, actual } => {
            format!(
                "content length mismatch: expected {}, got {}",
                format_size(*expected),
                format_size(*actual)
            )
        }
        FailReason::Http(status) => format!("HTTP {status}"),
        FailReason::RateLimited { retry_after_secs } => {
            format!("rate limited; retry after {retry_after_secs}s")
        }
        FailReason::Network(reason) => format!("network error: {reason}"),
        FailReason::ChecksumMismatch => "SHA-256 mismatch".to_owned(),
        FailReason::Io(reason) => format!("filesystem error: {reason}"),
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_text_preserves_security_failures() {
        let rendered = reason_text(&FailReason::ChecksumMismatch);
        assert!(rendered.contains("SHA-256"));

        let rendered = reason_text(&FailReason::ContentLengthMismatch {
            expected: 10,
            actual: 12,
        });
        assert!(rendered.contains("content length mismatch"));
    }
}

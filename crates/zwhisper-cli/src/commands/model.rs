//! `zwhisper model …` — manage local whisper.cpp model files.

use color_eyre::eyre::{WrapErr, eyre};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zwhisper_core::transcribe::model_management::{
    DownloadState, FailReason, ModelDownloader, ModelManifest, ModelSourceConfig, models_dir,
    verify_model,
};

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
    let manifest = ModelManifest::embedded();
    let dir = models_dir().map_err(|e| eyre!("{e}"))?;

    println!("{:<10}  {:>10}  {:<10}  path", "model", "size", "status");
    println!("{}", "-".repeat(88));
    for (name, entry) in manifest.known_models() {
        let path = dir.join(format!("ggml-{name}.bin"));
        let status = if path.is_file() {
            "installed"
        } else {
            "missing"
        };
        println!(
            "{:<10}  {:>10}  {:<10}  {}",
            name,
            format_size(entry.size_bytes),
            status,
            path.display()
        );
    }
    Ok(())
}

fn path(model: Option<&str>) -> color_eyre::Result<()> {
    let path = match model {
        Some(model) => zwhisper_core::transcribe::model_management::model_path(model)
            .map_err(|e| eyre!("{e}"))?,
        None => models_dir().map_err(|e| eyre!("{e}"))?,
    };
    println!("{}", path.display());
    Ok(())
}

fn install(model: &str) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(install_async(model))
}

async fn install_async(model: &str) -> color_eyre::Result<()> {
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
}

fn verify(model: &str) -> color_eyre::Result<()> {
    let verification = verify_model(model).map_err(|e| eyre!("{e}"))?;
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

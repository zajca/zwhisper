//! Hardened archive extraction for directory-bundle models (RFC:
//! Archive Security). Pure, network-free, and heavily unit-tested with
//! malicious fixtures — the download orchestration lives in
//! [`super::bundle_download`].
//!
//! Every guard here maps to a known archive-extraction weakness and is
//! mandatory, not optional hardening:
//!
//! - **Verify-before-extract** is enforced by the caller: this module is
//!   only handed bytes whose SHA-256 already matched.
//! - **Path traversal (zip-slip), lexical only (CWE-22).** Entry paths
//!   are validated by [`sanitize_lexical`] using pure path arithmetic.
//!   It never calls `canonicalize` or any OS path-resolution syscall
//!   (those resolve symlinks against the live fs and are racy —
//!   check-then-use TOCTOU, CWE-367). Backslashes, Windows drive
//!   prefixes, and UNC prefixes are rejected regardless of host OS.
//! - **No symlinks, no hardlinks (CWE-59).** Both link types are
//!   rejected with equal strictness; the extractor never follows a link.
//! - **Decompression-bomb defense — byte AND entry caps (CWE-409).**
//!   A `LimitedReader` bounds cumulative decompressed bytes; per-entry
//!   size and a total entry count are bounded too.
//! - **No executable bit propagation.** Files are written data-only.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Hard caps governing one extraction. Sourced from the model spec's
/// `NonZeroU64` archive caps (so "disabled" is unrepresentable) plus a
/// per-entry bound.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// Cumulative uncompressed bytes across the whole archive.
    pub max_unpacked_bytes: u64,
    /// Maximum number of entries.
    pub max_entries: u64,
    /// Maximum uncompressed bytes for any single entry.
    pub max_entry_bytes: u64,
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("archive I/O error: {0}")]
    Io(String),
    #[error("archive open/parse error: {0}")]
    Archive(String),
    #[error("rejected entry `{0}`: path traversal / absolute / backslash / drive / UNC")]
    PathTraversal(String),
    #[error("rejected symlink entry `{0}` (symlinks are never extracted)")]
    Symlink(String),
    #[error("rejected hardlink entry `{0}` (hardlinks are never extracted)")]
    Hardlink(String),
    #[error("rejected unsupported entry type `{kind}` for `{name}`")]
    UnsupportedEntryType { name: String, kind: String },
    #[error("archive has too many entries (limit {limit})")]
    TooManyEntries { limit: u64 },
    #[error("entry `{name}` exceeds the per-entry size cap ({limit} bytes)")]
    EntryTooLarge { name: String, limit: u64 },
    #[error("archive exceeds the total unpacked-byte cap ({limit} bytes) — possible bomb")]
    TotalTooLarge { limit: u64 },
}

/// A `Read` wrapper that aborts once cumulative bytes exceed `cap`.
/// The decompression-bomb backstop: even if an entry header lies about
/// its size, the actual inflated byte stream is bounded.
struct LimitedReader<R> {
    inner: R,
    read_so_far: u64,
    cap: u64,
}

impl<R: Read> LimitedReader<R> {
    fn new(inner: R, cap: u64) -> Self {
        Self {
            inner,
            read_so_far: 0,
            cap,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read_so_far = self
            .read_so_far
            .checked_add(n as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "byte counter overflow"))?;
        if self.read_so_far > self.cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed size exceeds configured cap (possible decompression bomb)",
            ));
        }
        Ok(n)
    }
}

/// Lexically normalize an archive entry name into a safe relative path,
/// or `None` if it is unsafe. Pure path arithmetic — never touches the
/// filesystem (no `canonicalize`, so no TOCTOU symlink race).
///
/// Rejects: empty, NUL, any backslash (Windows/UNC), a Windows drive
/// prefix (`X:`), absolute paths (`RootDir`/`Prefix`), and any `..`
/// component.
pub fn sanitize_lexical(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return None;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None; // Windows drive prefix like `C:`
    }
    let mut out = PathBuf::new();
    for comp in Path::new(name).components() {
        match comp {
            // Absolute path (RootDir/Prefix) or `..` traversal → reject.
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
            Component::CurDir => {} // `.` — skip
            Component::Normal(c) => out.push(c),
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// Write `bytes_reader` into `dest_dir/rel`, creating parent dirs, with
/// data-only permissions (no executable bit) and bounded by `cap`.
fn write_entry<R: Read>(
    dest_dir: &Path,
    rel: &Path,
    mut bytes_reader: R,
    cap: u64,
) -> Result<u64, ExtractError> {
    let out = dest_dir.join(rel);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ExtractError::Io(e.to_string()))?;
    }
    let mut file = std::fs::File::create(&out).map_err(|e| ExtractError::Io(e.to_string()))?;
    let mut limited = LimitedReader::new(&mut bytes_reader, cap);
    let written = io::copy(&mut limited, &mut file).map_err(|e| ExtractError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Data-only: rw for owner, no execute, no propagation of any
        // executable bit the archive tried to set.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ExtractError::Io(e.to_string()))?;
    }
    Ok(written)
}

/// Extract a ZIP archive into `dest_dir` under the security guards.
/// `R: Read + Seek` because the ZIP central directory is read by seek.
pub fn extract_zip<R: Read + io::Seek>(
    reader: R,
    dest_dir: &Path,
    limits: &ExtractLimits,
) -> Result<(), ExtractError> {
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| ExtractError::Archive(e.to_string()))?;
    let count = zip.len() as u64;
    if count > limits.max_entries {
        return Err(ExtractError::TooManyEntries {
            limit: limits.max_entries,
        });
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| ExtractError::Archive(e.to_string()))?;
        let raw_name = entry.name().to_owned();

        // Reject symlinks two ways (ZIP has no hardlink type).
        let is_symlink = entry.is_symlink()
            || entry
                .unix_mode()
                .is_some_and(|m| m & 0o170_000 == 0o120_000);
        if is_symlink {
            return Err(ExtractError::Symlink(raw_name));
        }

        // Lexical-only safe path (defense-in-depth over zip's own
        // `enclosed_name`).
        let Some(rel) = sanitize_lexical(&raw_name) else {
            return Err(ExtractError::PathTraversal(raw_name));
        };

        if entry.is_dir() {
            std::fs::create_dir_all(dest_dir.join(&rel))
                .map_err(|e| ExtractError::Io(e.to_string()))?;
            continue;
        }

        let declared = entry.size();
        if declared > limits.max_entry_bytes {
            return Err(ExtractError::EntryTooLarge {
                name: raw_name,
                limit: limits.max_entry_bytes,
            });
        }
        // Bound the ACTUAL inflated bytes by the per-entry cap — a ZIP
        // entry whose header lies about its size cannot overrun
        // (the LimitedReader trips and write_entry returns Io).
        let written = write_entry(dest_dir, &rel, &mut entry, limits.max_entry_bytes)?;
        total = total
            .checked_add(written)
            .ok_or(ExtractError::TotalTooLarge {
                limit: limits.max_unpacked_bytes,
            })?;
        if total > limits.max_unpacked_bytes {
            return Err(ExtractError::TotalTooLarge {
                limit: limits.max_unpacked_bytes,
            });
        }
    }
    Ok(())
}

/// Extract a gzip-compressed TAR archive into `dest_dir` under the
/// security guards. The gzip stream is bounded by a `LimitedReader`
/// so a gzip bomb cannot inflate past the total cap.
pub fn extract_tar_gz<R: Read>(
    reader: R,
    dest_dir: &Path,
    limits: &ExtractLimits,
) -> Result<(), ExtractError> {
    use tar::EntryType;

    let gz = flate2::read::GzDecoder::new(reader);
    // Bound the raw INFLATED tar stream (the bomb backstop). The stream
    // is the file content plus tar framing overhead (a 512-byte header
    // and up to 512 bytes of block padding per entry, plus a 1024-byte
    // trailer), so allow `max_unpacked_bytes` of content + that
    // overhead. The precise *content* bound is enforced per-entry via
    // `TotalTooLarge` below; this cap only catches pathological inflate
    // ratios that would exhaust memory before extraction accounting runs.
    let tar_overhead = limits.max_entries.saturating_add(4).saturating_mul(1536);
    let inflate_cap = limits.max_unpacked_bytes.saturating_add(tar_overhead);
    let bounded = LimitedReader::new(gz, inflate_cap);
    let mut archive = tar::Archive::new(bounded);
    // Belt: never restore stored modes/owners even if a later refactor
    // used the built-in unpack.
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(false);

    let entries = archive
        .entries()
        .map_err(|e| ExtractError::Archive(e.to_string()))?;

    let mut count: u64 = 0;
    let mut total: u64 = 0;
    for entry in entries {
        let mut entry = entry.map_err(|e| ExtractError::Archive(e.to_string()))?;
        count += 1;
        if count > limits.max_entries {
            return Err(ExtractError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let raw_name = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<non-utf8>".to_owned());

        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {}
            EntryType::Directory => {
                let Some(rel) = sanitize_lexical(&raw_name) else {
                    return Err(ExtractError::PathTraversal(raw_name));
                };
                std::fs::create_dir_all(dest_dir.join(&rel))
                    .map_err(|e| ExtractError::Io(e.to_string()))?;
                continue;
            }
            EntryType::Symlink => return Err(ExtractError::Symlink(raw_name)),
            EntryType::Link => return Err(ExtractError::Hardlink(raw_name)),
            other => {
                return Err(ExtractError::UnsupportedEntryType {
                    name: raw_name,
                    kind: format!("{other:?}"),
                });
            }
        }

        let Some(rel) = sanitize_lexical(&raw_name) else {
            return Err(ExtractError::PathTraversal(raw_name));
        };
        let declared = entry.header().size().unwrap_or(0);
        if declared > limits.max_entry_bytes {
            return Err(ExtractError::EntryTooLarge {
                name: raw_name,
                limit: limits.max_entry_bytes,
            });
        }
        // tar framing reads exactly `declared` bytes for this entry; the
        // GzDecoder LimitedReader bounds the total inflate regardless.
        // Cross-entry accumulation is caught by the total check below.
        let written = write_entry(dest_dir, &rel, &mut entry, limits.max_entry_bytes)?;
        total = total
            .checked_add(written)
            .ok_or(ExtractError::TotalTooLarge {
                limit: limits.max_unpacked_bytes,
            })?;
        if total > limits.max_unpacked_bytes {
            return Err(ExtractError::TotalTooLarge {
                limit: limits.max_unpacked_bytes,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::io::{Cursor, Write};

    use tempfile::TempDir;

    use super::*;

    fn limits() -> ExtractLimits {
        ExtractLimits {
            max_unpacked_bytes: 1024 * 1024,
            max_entries: 100,
            max_entry_bytes: 512 * 1024,
        }
    }

    // ---------- sanitize_lexical ----------

    #[test]
    fn sanitize_accepts_plain_relative_paths() {
        assert_eq!(
            sanitize_lexical("encoder.int8.onnx"),
            Some(PathBuf::from("encoder.int8.onnx"))
        );
        assert_eq!(
            sanitize_lexical("./vocab.txt"),
            Some(PathBuf::from("vocab.txt"))
        );
    }

    #[test]
    fn sanitize_rejects_traversal_absolute_backslash_drive_unc() {
        for bad in [
            "../etc/passwd",
            "a/../../b",
            "/abs/path",
            "..",
            "",
            "a\\b",       // backslash
            "\\\\srv\\s", // UNC
            "C:/win",     // drive
            "x:\\y",
        ] {
            assert!(sanitize_lexical(bad).is_none(), "should reject {bad:?}");
        }
    }

    // ---------- ZIP fixtures ----------

    fn zip_with<F: FnOnce(&mut zip::ZipWriter<Cursor<Vec<u8>>>)>(build: F) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        build(&mut w);
        w.finish().unwrap().into_inner()
    }

    fn zip_file(w: &mut zip::ZipWriter<Cursor<Vec<u8>>>, name: &str, data: &[u8]) {
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file(name, opts).unwrap();
        w.write_all(data).unwrap();
    }

    #[test]
    fn zip_happy_path_extracts_files() {
        let bytes = zip_with(|w| {
            zip_file(w, "encoder.onnx", b"weights");
            zip_file(w, "vocab.txt", b"tokens");
        });
        let dir = TempDir::new().unwrap();
        extract_zip(Cursor::new(bytes), dir.path(), &limits()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("encoder.onnx")).unwrap(),
            b"weights"
        );
        assert_eq!(
            std::fs::read(dir.path().join("vocab.txt")).unwrap(),
            b"tokens"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("encoder.onnx"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "no executable bit may be propagated");
        }
    }

    #[test]
    fn zip_slip_traversal_rejected() {
        let bytes = zip_with(|w| zip_file(w, "../escape.txt", b"x"));
        let dir = TempDir::new().unwrap();
        let err = extract_zip(Cursor::new(bytes), dir.path(), &limits()).unwrap_err();
        assert!(matches!(err, ExtractError::PathTraversal(_)));
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn zip_symlink_rejected() {
        let bytes = zip_with(|w| {
            let opts = zip::write::SimpleFileOptions::default();
            w.add_symlink("link", "/etc/passwd", opts).unwrap();
        });
        let dir = TempDir::new().unwrap();
        let err = extract_zip(Cursor::new(bytes), dir.path(), &limits()).unwrap_err();
        assert!(matches!(err, ExtractError::Symlink(_)), "got {err:?}");
    }

    #[test]
    fn zip_entry_count_bomb_rejected() {
        let bytes = zip_with(|w| {
            for i in 0..50 {
                zip_file(w, &format!("f{i}.bin"), b"x");
            }
        });
        let dir = TempDir::new().unwrap();
        let tight = ExtractLimits {
            max_entries: 10,
            ..limits()
        };
        let err = extract_zip(Cursor::new(bytes), dir.path(), &tight).unwrap_err();
        assert!(matches!(err, ExtractError::TooManyEntries { .. }));
    }

    #[test]
    fn zip_total_byte_bomb_rejected() {
        let bytes = zip_with(|w| {
            zip_file(w, "big.bin", &vec![0u8; 4096]);
        });
        let dir = TempDir::new().unwrap();
        let tight = ExtractLimits {
            max_unpacked_bytes: 1024,
            max_entry_bytes: 1024,
            ..limits()
        };
        let err = extract_zip(Cursor::new(bytes), dir.path(), &tight).unwrap_err();
        assert!(
            matches!(
                err,
                ExtractError::EntryTooLarge { .. } | ExtractError::TotalTooLarge { .. }
            ),
            "got {err:?}"
        );
    }

    // ---------- TAR.GZ fixtures ----------

    fn targz_with<F: FnOnce(&mut tar::Builder<Vec<u8>>)>(build: F) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        let tar_bytes = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn tar_file(b: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append_data(&mut h, name, data).unwrap();
    }

    #[test]
    fn targz_happy_path_extracts() {
        let bytes = targz_with(|b| {
            tar_file(b, "encoder.onnx", b"weights");
            tar_file(b, "vocab.txt", b"tokens");
        });
        let dir = TempDir::new().unwrap();
        extract_tar_gz(Cursor::new(bytes), dir.path(), &limits()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("encoder.onnx")).unwrap(),
            b"weights"
        );
    }

    #[test]
    fn targz_traversal_rejected() {
        // `tar::Builder::append_data` refuses to write a `..` path, so
        // craft the malicious entry by writing the name field of a raw
        // header directly (an attacker controls the bytes on the wire).
        let bytes = targz_with(|b| {
            let data = b"x";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            let name = b"../escape.txt";
            h.as_mut_bytes()[..name.len()].copy_from_slice(name);
            h.set_cksum();
            b.append(&h, &data[..]).unwrap();
        });
        let dir = TempDir::new().unwrap();
        let err = extract_tar_gz(Cursor::new(bytes), dir.path(), &limits()).unwrap_err();
        assert!(matches!(err, ExtractError::PathTraversal(_)), "got {err:?}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn targz_symlink_rejected() {
        let bytes = targz_with(|b| {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_cksum();
            b.append_link(&mut h, "link", "/etc/passwd").unwrap();
        });
        let dir = TempDir::new().unwrap();
        let err = extract_tar_gz(Cursor::new(bytes), dir.path(), &limits()).unwrap_err();
        assert!(matches!(err, ExtractError::Symlink(_)), "got {err:?}");
    }

    #[test]
    fn targz_hardlink_rejected() {
        let bytes = targz_with(|b| {
            tar_file(b, "real.bin", b"data");
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_entry_type(tar::EntryType::Link);
            h.set_cksum();
            b.append_link(&mut h, "hard", "real.bin").unwrap();
        });
        let dir = TempDir::new().unwrap();
        let err = extract_tar_gz(Cursor::new(bytes), dir.path(), &limits()).unwrap_err();
        assert!(matches!(err, ExtractError::Hardlink(_)), "got {err:?}");
    }

    #[test]
    fn targz_entry_count_bomb_rejected() {
        let bytes = targz_with(|b| {
            for i in 0..50 {
                tar_file(b, &format!("f{i}.bin"), b"x");
            }
        });
        let dir = TempDir::new().unwrap();
        let tight = ExtractLimits {
            max_entries: 10,
            ..limits()
        };
        let err = extract_tar_gz(Cursor::new(bytes), dir.path(), &tight).unwrap_err();
        assert!(matches!(err, ExtractError::TooManyEntries { .. }));
    }

    #[test]
    fn targz_total_byte_bomb_rejected() {
        // Two entries that each fit the per-entry cap but together
        // exceed the total cap → clean TotalTooLarge (accumulation).
        let bytes = targz_with(|b| {
            tar_file(b, "a.bin", &vec![3u8; 3000]);
            tar_file(b, "b.bin", &vec![4u8; 3000]);
        });
        let dir = TempDir::new().unwrap();
        let tight = ExtractLimits {
            max_unpacked_bytes: 5000,
            max_entry_bytes: 4096,
            max_entries: 100,
        };
        let err = extract_tar_gz(Cursor::new(bytes), dir.path(), &tight).unwrap_err();
        assert!(
            matches!(err, ExtractError::TotalTooLarge { .. }),
            "got {err:?}"
        );
    }
}

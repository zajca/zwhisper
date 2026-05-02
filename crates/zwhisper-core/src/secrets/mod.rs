//! Per-backend API-key resolution for cloud transcribers.
//!
//! M5 ships **two** lookup sources, in order:
//!
//! 1. Environment variable `ZWHISPER_<BACKEND>_API_KEY` (uppercased,
//!    backend id with `-` rewritten to `_`).
//! 2. `~/.config/zwhisper/secrets.toml`, mode `0o600` or `0o400`,
//!    owner `geteuid()`, parent directory not group/other-writable.
//!
//! There is **no** keyring / `secret-service` integration in M5 —
//! see `docs/M5-plan.md` § "Architecture for M5 / API key flow".
//! The keyring path was deliberately deferred (Q2-c, 2026-05-02).
//!
//! All file I/O uses `O_NOFOLLOW`-open then `fstat` against the
//! returned descriptor (M5-plan § C3) to close the TOCTOU race
//! between `stat` and `open`. The parent-directory check happens
//! after the fstat to keep the overall failure mode "fail-fast,
//! fail-clear".

pub mod resolver;

pub use resolver::{
    DEFAULT_SECRETS_RELATIVE_PATH, ResolveSource, ResolverConfig, SecretString, SecretsError,
    resolve_api_key, resolve_api_key_with_config,
};

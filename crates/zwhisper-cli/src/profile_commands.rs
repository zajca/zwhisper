//! CLI-facing pretty-printers for `zwhisper profile {list, show, clone, migrate}`.
//!
//! The data-producing operations live in `zwhisper_core::profile::listing`;
//! this module owns the stdout shape so the daemon (which speaks D-Bus,
//! not stdout) can reuse the same backend without dragging printing
//! along. Phase 1 of M3 carved this split out of the original
//! `profile::commands` module.

use color_eyre::eyre::eyre;

use zwhisper_core::profile;
use zwhisper_core::profile::ProfileSource;
use zwhisper_core::profile::error::ProfileError;
use zwhisper_core::profile::listing;

/// `zwhisper profile list` — print a name+source+ver+description table.
pub(crate) fn list() -> color_eyre::Result<()> {
    let entries = listing::list_entries().map_err(eyre_from)?;
    if entries.is_empty() {
        println!("(no profiles found)");
        return Ok(());
    }

    println!(
        "{:<24}  {:<10}  {:<6}  description",
        "name", "source", "ver"
    );
    println!("{}", "-".repeat(72));
    for entry in entries {
        let version_label = entry
            .schema_version
            .map_or_else(|| "?".to_owned(), |v| v.to_string());
        println!(
            "{:<24}  {:<10}  {version_label:<6}  {}",
            entry.name,
            entry.source,
            entry.description.unwrap_or_default()
        );
    }
    Ok(())
}

/// `zwhisper profile show <name>` — print resolved source + canonical
/// TOML body (post-migration if migration ran).
pub(crate) fn show(name: &str) -> color_eyre::Result<()> {
    let source = listing::resolved_source(name).map_err(eyre_from)?;
    println!(
        "source: {} ({})",
        source.label(),
        source_path_label(&source)
    );

    let profile = profile::load(name).map_err(eyre_from)?;
    println!("---");
    println!(
        "{}",
        toml_edit::ser::to_string_pretty(&profile)
            .map_err(|e| eyre!("could not serialize profile: {e}"))?
    );
    Ok(())
}

/// `zwhisper profile clone <src> <dst>` — copy a resolved profile
/// into the user override dir, rewriting the `name` field to `<dst>`.
pub(crate) fn clone(src: &str, dst: &str) -> color_eyre::Result<()> {
    let target = listing::clone_to_user(src, dst).map_err(eyre_from)?;
    println!("cloned {src} -> {}", target.display());
    Ok(())
}

/// `zwhisper profile migrate <name>` — force the migration chain on a
/// user override. No-op when already at `CURRENT_SCHEMA_VERSION`.
pub(crate) fn migrate(name: &str) -> color_eyre::Result<()> {
    let profile = listing::migrate_user(name).map_err(|err| match err {
        ProfileError::NotFound { searched, .. } => {
            // The CLI surface gave a more specific message in M2 — preserve it.
            let path = searched.first().cloned().unwrap_or_default();
            eyre!(
                "profile migrate operates on user overrides; {path} not found. \
                 Run `zwhisper profile clone {name} <name>` first."
            )
        }
        other => eyre!("{other}"),
    })?;
    // `listing::migrate_user` returns the loaded profile, but we want
    // the on-disk path for the message — recompute it via the same
    // public API that `migrate_user` consulted.
    let user_path = match listing::resolved_source(name).map_err(eyre_from)? {
        ProfileSource::UserOverride(p) => p,
        other => {
            // Should not happen — `migrate_user` requires a user
            // override and would have returned NotFound otherwise.
            return Err(eyre!(
                "internal: migrate succeeded but source resolved to {other:?}"
            ));
        }
    };
    println!(
        "{name} at {} is now schema_version = {}",
        user_path.display(),
        profile.schema_version
    );
    Ok(())
}

fn source_path_label(src: &ProfileSource) -> String {
    match src {
        ProfileSource::UserOverride(p) | ProfileSource::Shipped(p) => p.display().to_string(),
        ProfileSource::Embedded(name) => format!("<embedded:{name}>"),
    }
}

#[allow(clippy::needless_pass_by_value)] // intentional point-free use in `.map_err(eyre_from)`
fn eyre_from(err: ProfileError) -> color_eyre::Report {
    eyre!("{err}")
}

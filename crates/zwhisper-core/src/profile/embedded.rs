// Phase 4 — embedded shipped templates.
//
// `include_dir!` resolves at compile time relative to
// `$CARGO_MANIFEST_DIR`, so the templates directory at the crate
// root is what ships inside every binary. Users on `cargo install`
// hosts (no `/usr/share/zwhisper/`) still get a working `default`,
// `meeting`, and `voicememo` because the loader falls back to this
// embedded copy when both filesystem locations miss.

use include_dir::{Dir, include_dir};

static PROFILES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/profiles");

/// Look up an embedded profile by basename (no `.toml` suffix). Used
/// by `paths::resolve_source` after both filesystem candidates miss.
pub(crate) fn lookup(name: &str) -> Option<&'static str> {
    let filename = format!("{name}.toml");
    PROFILES.get_file(&filename)?.contents_utf8()
}

/// All embedded profile names (without `.toml`). Surfaces in
/// `profile list` and the validity tests in Phase 5.
pub(crate) fn names() -> Vec<&'static str> {
    PROFILES
        .files()
        .filter_map(|f| {
            let path = f.path();
            let name = path.file_name()?.to_str()?;
            name.strip_suffix(".toml")
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::loader::load_from_str;
    use super::*;

    #[test]
    fn names_contains_shipped_profiles() {
        let names = names();
        for required in ["default", "meeting", "voicememo"] {
            assert!(
                names.contains(&required),
                "embedded names missing {required}: {names:?}"
            );
        }
    }

    #[test]
    fn every_embedded_profile_loads_and_validates() {
        for name in names() {
            let body = lookup(name).unwrap_or_else(|| panic!("lookup {name}"));
            let profile = load_from_str(body, name)
                .unwrap_or_else(|e| panic!("embedded {name} failed: {e:?}"));
            assert_eq!(profile.name, name, "name field must match filename");
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("no-such-profile").is_none());
    }
}

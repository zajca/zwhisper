//! Parsing and formatting of `wpctl` device volume.
//!
//! `wpctl get-volume <id>` prints a single line `Volume: 0.45` with an
//! optional ` [MUTED]` suffix (verified on the ALC1220 box, 2026-06-03).
//! `wpctl set-volume <id> <linear>` takes a linear amplitude factor.
//! This module turns that text into a typed [`Volume`] and back, with no
//! silent coercion — a malformed line is a typed [`SetupError::Parse`].

use super::SetupError;

/// A device's current volume as reported by `wpctl`. `linear` is the
/// raw linear amplitude factor (0.0 = silent, 1.0 = unity, > 1.0 =
/// boosted); `muted` reflects the `[MUTED]` flag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    /// Linear amplitude factor (not dB). `0.5 ≈ −6 dB`.
    pub linear: f32,
    /// Whether the node is currently muted (the `[MUTED]` suffix).
    pub muted: bool,
}

/// Parse a `wpctl get-volume` body into a [`Volume`].
///
/// Accepts `Volume: 0.45` and `Volume: 0.45 [MUTED]` (case-insensitive
/// `[MUTED]`), tolerating surrounding whitespace. Rejects an empty body,
/// a missing `Volume:` prefix, a non-numeric / non-finite / negative
/// value, and an unexpected trailing token — every failure is a typed
/// [`SetupError::Parse`] so a hostile or unexpected `wpctl` output can
/// never panic or produce a bogus level.
pub fn parse_volume(stdout: &str) -> Result<Volume, SetupError> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| SetupError::Parse {
            what: "wpctl volume",
            message: "empty output".to_owned(),
        })?;

    let rest = line
        .strip_prefix("Volume:")
        .ok_or_else(|| SetupError::Parse {
            what: "wpctl volume",
            message: format!("missing `Volume:` prefix in {line:?}"),
        })?
        .trim();

    let mut tokens = rest.split_whitespace();
    let value = tokens.next().ok_or_else(|| SetupError::Parse {
        what: "wpctl volume",
        message: format!("no numeric value in {line:?}"),
    })?;

    let linear: f32 = value.parse().map_err(|_| SetupError::Parse {
        what: "wpctl volume",
        message: format!("value {value:?} is not a number"),
    })?;
    if !linear.is_finite() || linear < 0.0 {
        return Err(SetupError::Parse {
            what: "wpctl volume",
            message: format!("value {linear} is not a finite, non-negative amplitude"),
        });
    }

    // Anything after the number must be exactly the mute flag. An
    // unexpected token means the format changed under us — fail loudly
    // rather than silently ignoring it.
    let mut muted = false;
    for tok in tokens {
        if tok.eq_ignore_ascii_case("[MUTED]") {
            muted = true;
        } else {
            return Err(SetupError::Parse {
                what: "wpctl volume",
                message: format!("unexpected trailing token {tok:?} in {line:?}"),
            });
        }
    }

    Ok(Volume { linear, muted })
}

/// Format a linear volume for `wpctl set-volume`.
///
/// Renders with four decimals (e.g. `0.4500`). The caller is responsible
/// for clamping and finiteness — `set_volume` does that before calling
/// this, so this stays a pure formatter. A non-finite input would render
/// as `inf`/`NaN`, which is exactly why the clamp lives upstream.
pub fn format_linear(linear: f32) -> String {
    format!("{linear:.4}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_volume() {
        let v = parse_volume("Volume: 0.45\n").unwrap();
        assert!((v.linear - 0.45).abs() < 1e-6);
        assert!(!v.muted);
    }

    #[test]
    fn parses_muted_volume() {
        let v = parse_volume("Volume: 0.45 [MUTED]").unwrap();
        assert!((v.linear - 0.45).abs() < 1e-6);
        assert!(v.muted);
    }

    #[test]
    fn parses_muted_case_insensitively() {
        let v = parse_volume("Volume: 0.25 [muted]").unwrap();
        assert!(v.muted);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let v = parse_volume("   Volume:   0.80   \n").unwrap();
        assert!((v.linear - 0.80).abs() < 1e-6);
        assert!(!v.muted);
    }

    #[test]
    fn parses_zero_and_unity_and_boost() {
        assert!((parse_volume("Volume: 0.0").unwrap().linear).abs() < 1e-6);
        assert!((parse_volume("Volume: 1.0").unwrap().linear - 1.0).abs() < 1e-6);
        assert!((parse_volume("Volume: 1.5").unwrap().linear - 1.5).abs() < 1e-6);
    }

    #[test]
    fn rejects_empty() {
        let err = parse_volume("").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn rejects_whitespace_only() {
        let err = parse_volume("   \n  \n").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn rejects_missing_prefix() {
        let err = parse_volume("0.45").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn rejects_non_numeric_value() {
        let err = parse_volume("Volume: loud").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn rejects_negative_and_non_finite() {
        for bad in ["Volume: -0.1", "Volume: inf", "Volume: NaN"] {
            let err = parse_volume(bad).unwrap_err();
            assert!(matches!(err, SetupError::Parse { .. }), "{bad}");
        }
    }

    #[test]
    fn rejects_unexpected_trailing_token() {
        let err = parse_volume("Volume: 0.45 [LOCKED]").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn format_linear_uses_four_decimals() {
        assert_eq!(format_linear(0.45), "0.4500");
        assert_eq!(format_linear(1.0), "1.0000");
        assert_eq!(format_linear(0.0), "0.0000");
    }

    #[test]
    fn format_round_trips_through_parse() {
        for linear in [0.0_f32, 0.25, 0.45, 0.8, 1.0] {
            let s = format!("Volume: {}", format_linear(linear));
            let parsed = parse_volume(&s).unwrap();
            assert!((parsed.linear - linear).abs() < 1e-4, "{linear}");
        }
    }
}

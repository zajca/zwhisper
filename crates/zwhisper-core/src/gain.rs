//! Single source of truth for dB↔linear input-gain conversion.
//!
//! zwhisper exposes input gain to users in **decibels** (human-readable:
//! `input_gain_db = -2.0`), but the GStreamer `volume` element on the
//! mic branch wants a **linear amplitude factor**. The conversion and
//! the accepted range live here so three call sites cannot drift apart:
//!
//! - [`crate::profile::schema::Profile::validate`] range-checks
//!   `sources.input_gain_db` against [`MIN_INPUT_GAIN_DB`] /
//!   [`MAX_INPUT_GAIN_DB`].
//! - the profile writer (`profile::listing::update_sources`) validates
//!   the same bounds before persisting.
//! - the audio pipeline clamps `input_gain_db` and converts it to the
//!   `volume` factor via `db_to_linear`.
//!
//! The `setup` calibration math (`crate::setup::level`) also uses
//! `linear_to_db` for the dBFS metering — keeping the silence guard
//! (`linear <= 0 → f32::NEG_INFINITY`, never `NaN`) in one place.
//!
//! [`db_to_linear`] is compiled under either the `audio` or the `setup`
//! feature: the `audio` pipeline converts `sources.input_gain_db` into
//! the `volume` element factor, and the `setup` calibration uses it for
//! its recommendation. [`linear_to_db`] stays `setup`-only — the dBFS
//! metering is its sole consumer (the pipeline needs only the forward
//! conversion). The `profile` feature pulls just [`MIN_INPUT_GAIN_DB`] /
//! [`MAX_INPUT_GAIN_DB`] for range validation.

/// Lower bound for a profile's `input_gain_db`. A −30 dB trim attenuates
/// to ~3 % amplitude, well past any sane mic level reduction; anything
/// below is almost certainly a typo and is rejected up front.
pub(crate) const MIN_INPUT_GAIN_DB: f32 = -30.0;

/// Upper bound for a profile's `input_gain_db`. +30 dB is a ~31.6×
/// amplification — already extreme; a larger boost would clip on any
/// real signal, so it is rejected rather than silently honoured.
pub(crate) const MAX_INPUT_GAIN_DB: f32 = 30.0;

/// Convert a gain in decibels to a linear amplitude factor: `10^(db/20)`.
///
/// This is the factor the GStreamer `volume` element multiplies samples
/// by. `0 dB → 1.0`, `−6 dB → ~0.501`, `+6 dB → ~1.995`. A non-finite
/// input (NaN/±inf) propagates through `powf` per IEEE-754; callers are
/// expected to have already range-/finite-checked the dB value (the
/// schema and writer do), so this stays a pure math helper with no
/// hidden clamping.
///
/// Gated to `audio` **or** `setup` — both consume the forward
/// conversion: the `audio` pipeline turns `sources.input_gain_db` into
/// the `volume` element factor, the `setup` calibration uses it for its
/// recommendation. The `profile` feature needs only the range
/// constants, so compiling the conversion there would be dead code
/// under `-D warnings`.
#[cfg(any(feature = "audio", feature = "setup"))]
pub(crate) fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Convert a linear amplitude to decibels: `20·log10(linear)`.
///
/// Guards the silence case explicitly: a non-positive linear value
/// (`<= 0.0`, including `-0.0`) returns [`f32::NEG_INFINITY`] instead of
/// `NaN` (`log10(0) = -inf`, `log10(neg) = NaN`). This is the floor the
/// dBFS metering relies on — an all-zero capture must report a clean
/// `-inf`/floor sentinel, never a `NaN` that would poison later
/// `clamp`/comparison logic.
///
/// Gated to `setup` like [`db_to_linear`] (the dBFS metering is its only
/// Wave-1 consumer); `profile` validation needs just the range constants.
#[cfg(feature = "setup")]
pub(crate) fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}

// The dB↔linear function tests need the functions, which are only
// compiled under `setup` (Wave 1); gate the suite to match so the other
// feature combos stay clean. The range-constant invariant is a
// compile-time assertion above, not a runtime test.
#[cfg(all(test, feature = "setup"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Tolerance for the dB↔linear float comparisons below.
    const EPS: f32 = 1e-4;

    #[test]
    fn zero_db_is_unity() {
        assert!((db_to_linear(0.0) - 1.0).abs() < EPS);
    }

    #[test]
    fn minus_six_db_is_half_amplitude() {
        // −6.0206 dB is exactly 0.5; −6 dB ≈ 0.5012.
        assert!((db_to_linear(-6.0) - 0.501_19).abs() < EPS);
    }

    #[test]
    fn plus_six_db_is_double_amplitude() {
        assert!((db_to_linear(6.0) - 1.995_26).abs() < EPS);
    }

    #[test]
    fn linear_unity_is_zero_db() {
        assert!(linear_to_db(1.0).abs() < EPS);
    }

    #[test]
    fn round_trip_db_through_linear() {
        for db in [-30.0_f32, -12.0, -6.0, -1.5, 0.0, 3.0, 6.0, 30.0] {
            let back = linear_to_db(db_to_linear(db));
            assert!(
                (back - db).abs() < EPS,
                "round-trip failed for {db} dB: {back}"
            );
        }
    }

    #[test]
    fn non_positive_linear_yields_neg_infinity_not_nan() {
        for linear in [0.0_f32, -0.0, -1.0, -123.4] {
            let db = linear_to_db(linear);
            assert!(
                db.is_infinite() && db.is_sign_negative(),
                "{linear} -> {db}"
            );
            assert!(!db.is_nan(), "{linear} produced NaN");
        }
    }

    #[test]
    fn half_amplitude_is_about_minus_six_db() {
        assert!((linear_to_db(0.5) - (-6.0206)).abs() < 1e-3);
    }
}

// Range-constant invariant as a compile-time assertion: the bounds must
// be finite and ordered. A `const` assert is the idiomatic check for an
// invariant over constants (a runtime `#[test]` would just trip clippy's
// `assertions_on_constants`); a violation fails the build, not a test.
const _: () = assert!(MIN_INPUT_GAIN_DB.is_finite());
const _: () = assert!(MAX_INPUT_GAIN_DB.is_finite());
const _: () = assert!(MIN_INPUT_GAIN_DB < MAX_INPUT_GAIN_DB);

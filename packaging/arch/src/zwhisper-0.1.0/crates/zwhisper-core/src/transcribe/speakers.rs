//! Speaker-diarization shape shared by all backends that emit it.
//!
//! M5 ships exactly one producer (`Deepgram`) and zero consumers
//! beyond `transcript.json` serialization. The shape is deliberately
//! flat — `speaker_id` (u32) plus utterance bounds — so a future
//! consumer (settings GUI, M7) can render speaker columns without
//! re-deriving anything from the raw provider response.
//!
//! `whisper.cpp` does not produce speaker labels and therefore sets
//! `TranscriptArtifacts.speakers = None`; its `transcript.json` omits
//! the `speakers` key entirely (`serde(skip_serializing_if =
//! "Option::is_none")`).

use serde::{Deserialize, Serialize};

/// One contiguous run of words attributed to a single speaker. The
/// upstream provider may emit speaker labels at the word level
/// (Deepgram does); the grouping into `SpeakerSegment` collapses
/// adjacent same-speaker words and concatenates their text.
///
/// Field semantics:
/// * `speaker_id` — opaque integer assigned by the backend (typically
///   0, 1, 2…). Stable across the same response only; do not assume
///   "speaker 0" in two different recordings is the same physical
///   person.
/// * `start_s`, `end_s` — seconds from the start of the recording.
///   `end_s >= start_s`. A zero-length segment is unusual but legal.
/// * `text` — punctuation-preserving concatenation of the contained
///   words, with a single space between adjacent words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerSegment {
    pub speaker_id: u32,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serde_json() {
        let original = SpeakerSegment {
            speaker_id: 3,
            start_s: 1.25,
            end_s: 4.75,
            text: "Hello there.".to_owned(),
        };
        let s = serde_json::to_string(&original).unwrap();
        let back: SpeakerSegment = serde_json::from_str(&s).unwrap();
        assert_eq!(original, back);
    }
}

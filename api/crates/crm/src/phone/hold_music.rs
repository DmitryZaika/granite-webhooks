//! Hold music: what callers hear while a user has them on hold.
//!
//! The app's hold is the Telnyx SDK's (a Verto `modify` hold); Telnyx then
//! sends the other party silence, not music, and has no music-on-hold
//! setting. So the app tells us when it holds and resumes a call
//! (`PUT /v1/phone/calls/hold`) and we play the user's track to the other
//! party with Call Control `playback_start` (`loop: infinity`), and stop it
//! with `playback_stop`:
//!
//! - the app's own calls (parked, then transferred): on the app's leg with
//!   `target_legs: opposite`, so only the customer hears it;
//! - calls our TeXML app answered (a `<Dial>` to the phone): on the
//!   caller's leg (its `CallSid` is its Call Control id) with
//!   `target_legs: self`.
//!
//! A user picks one of the built-in tracks (`api/data/hold-music/`,
//! original CC0 music served at `GET /v1/public/hold-music/{track}.mp3`),
//! their own MP3 or WAV (uploaded to the bucket, at most
//! [`MAX_UPLOAD_BYTES`]), or none (silence, as before).

/// A built-in track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    /// Its id, as stored and sent (`hold_music`).
    pub id: &'static str,
    /// What the app calls it.
    pub name: &'static str,
    /// The MP3.
    pub bytes: &'static [u8],
}

/// The built-in tracks, in the order the app lists them.
pub const TRACKS: [Track; 3] = [
    Track {
        id: "calm",
        name: "Calm piano",
        bytes: include_bytes!("../../../../data/hold-music/calm.mp3"),
    },
    Track {
        id: "bright",
        name: "Bright marimba",
        bytes: include_bytes!("../../../../data/hold-music/bright.mp3"),
    },
    Track {
        id: "ambient",
        name: "Soft ambient",
        bytes: include_bytes!("../../../../data/hold-music/ambient.mp3"),
    },
];

/// A user who chose nothing hears this track.
pub const DEFAULT_TRACK: &str = "calm";
/// `hold_music` for the user's own upload.
pub const CUSTOM: &str = "custom";
/// `hold_music` for silence.
pub const NONE: &str = "none";
/// The largest track a user may upload (5 MB: a few minutes of MP3).
pub const MAX_UPLOAD_BYTES: u64 = 5 * 1024 * 1024;
/// The bucket folder of uploaded tracks (`hold-music/{company id}/...`).
pub const UPLOAD_FOLDER: &str = "hold-music";

/// The key prefix of `user_id`'s uploads in `company_id`.
pub fn upload_prefix(company_id: i32, user_id: i32) -> String {
    format!("{UPLOAD_FOLDER}/{company_id}/{user_id}-")
}

/// The built-in track `id`.
pub fn track(id: &str) -> Option<&'static Track> {
    TRACKS.iter().find(|track| track.id == id)
}

/// Where Telnyx (and the app's preview) fetches built-in track `id`.
pub fn track_url(public_api_url: &str, id: &str) -> String {
    format!(
        "{}/v1/public/hold-music/{id}.mp3",
        public_api_url.trim_end_matches('/')
    )
}

/// What a user with this choice plays on hold: the URL of the audio, or
/// None for silence (`none`, a custom choice without a file, or a track
/// that no longer exists).
pub fn audio_url(public_api_url: &str, choice: &str, custom_url: Option<&str>) -> Option<String> {
    match choice {
        NONE => None,
        CUSTOM => custom_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        id => track(id).map(|track| track_url(public_api_url, track.id)),
    }
}

/// The audio type of an uploaded file, by its first bytes: `mp3` (an ID3
/// tag or an MPEG audio frame) or `wav` (RIFF/WAVE); None for anything
/// else.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("wav");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"ID3" {
        return Some("mp3");
    }
    // An MPEG frame sync: 11 set bits, a layer that is not "reserved".
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0 && bytes[1] & 0x06 != 0 {
        return Some("mp3");
    }
    None
}

/// The content type an upload must declare, by its file's extension or
/// type; None when it is neither MP3 nor WAV.
pub fn upload_kind(file_name: &str, content_type: &str) -> Option<&'static str> {
    let content_type = content_type.trim().to_ascii_lowercase();
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match (content_type.as_str(), ext.as_str()) {
        ("audio/mpeg" | "audio/mp3", _) | (_, "mp3") => Some("mp3"),
        ("audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave", _) | (_, "wav") => {
            Some("wav")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_track_is_a_small_mp3() {
        for track in TRACKS {
            assert_eq!(sniff(track.bytes), Some("mp3"), "{}", track.id);
            assert!(track.bytes.len() < 500_000, "{}", track.id);
        }
        assert!(track(DEFAULT_TRACK).is_some());
    }

    #[test]
    fn the_choice_names_the_audio() {
        let api = "https://api.test/";
        assert_eq!(
            audio_url(api, "bright", None).as_deref(),
            Some("https://api.test/v1/public/hold-music/bright.mp3")
        );
        assert_eq!(audio_url(api, NONE, Some("https://x/y.mp3")), None);
        assert_eq!(
            audio_url(api, CUSTOM, Some("https://bucket/hold-music/100/a.mp3")).as_deref(),
            Some("https://bucket/hold-music/100/a.mp3")
        );
        assert_eq!(audio_url(api, CUSTOM, None), None);
        assert_eq!(audio_url(api, "gone", None), None);
    }

    #[test]
    fn uploads_are_mp3_or_wav() {
        assert_eq!(sniff(b"ID3\x04\x00rest"), Some("mp3"));
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x64]), Some("mp3"));
        assert_eq!(sniff(b"RIFF\x24\x08\x00\x00WAVEfmt "), Some("wav"));
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(sniff(&[0xFF, 0xE0]), None);
        assert_eq!(upload_kind("Song.MP3", ""), Some("mp3"));
        assert_eq!(upload_kind("a", "audio/x-wav"), Some("wav"));
        assert_eq!(upload_kind("a.ogg", "audio/ogg"), None);
    }
}

//! Reading the IRS letter that gave a business its EIN (the CP 575, or the
//! 147C that confirms it) for the 10DLC form ([`super::tendlc`]). TCR
//! matches the brand's legal name, EIN and address to the IRS's records
//! exactly, and leaves a brand that differs unverified for good, so the form
//! must have them as the letter does. The AI reads the uploaded file; only
//! what it says is kept ([`TenDlcEinLetter`]), never the file.

use super::tendlc::{TenDlcEinLetter, tax_id};
use crate::ai::{AiError, Caller, LARGE_FALLBACK_MODEL, LARGE_MODEL};
use crate::state::AppState;
use chrono::{SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// The biggest file read, in bytes.
pub const MAX_LETTER_BYTES: usize = 4 * 1024 * 1024;
/// The biggest request carrying one: base64 grows a file by a third.
pub const MAX_LETTER_BODY_BYTES: usize = MAX_LETTER_BYTES / 3 * 4 + 64 * 1024;
/// The longest file name kept.
const FILE_NAME_CHARS: usize = 200;
const AI_TIMEOUT: Duration = Duration::from_secs(120);
const REASONING_EFFORT: &str = "medium";
const MAX_COMPLETION_TOKENS: u32 = 8_000;
/// The file types the AI reads.
const TYPES: &[&str] = &[
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
];

/// Why a letter was not read.
#[derive(Debug, thiserror::Error)]
pub enum LetterError {
    /// Not a file the AI reads, not an IRS letter, or not readable: what to
    /// do, for the person.
    #[error("{0}")]
    Invalid(String),
    /// The AI did not answer.
    #[error(transparent)]
    Ai(#[from] AiError),
}

/// The file's type and its base64, from its `data:` URL.
fn parts(data_url: &str) -> Result<(String, String), LetterError> {
    let invalid =
        || LetterError::Invalid("Upload the letter as a PDF or a photo (PNG, JPG or WEBP).".into());
    let rest = data_url.strip_prefix("data:").ok_or_else(invalid)?;
    let (meta, data) = rest.split_once(',').ok_or_else(invalid)?;
    let kind = meta
        .split(';')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !meta.ends_with(";base64") || !TYPES.contains(&kind.as_str()) {
        return Err(invalid());
    }
    let data = data.trim();
    if data.len() > MAX_LETTER_BYTES / 3 * 4 + 4 {
        return Err(LetterError::Invalid(
            "The file is too big: upload at most 4 MB.".into(),
        ));
    }
    let base64 = !data.is_empty()
        && data.len() % 4 == 0
        && data
            .trim_end_matches('=')
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/');
    if !base64 {
        return Err(invalid());
    }
    Ok((kind, data.to_string()))
}

/// What the AI reads off the letter.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Read {
    is_irs_ein_letter: bool,
    kind: String,
    legal_name: String,
    ein: String,
    street: String,
    city: String,
    state: String,
    postal_code: String,
    unreadable: String,
}

fn schema() -> Value {
    let text = json!({ "type": "string" });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["isIrsEinLetter", "kind", "legalName", "ein", "street", "city", "state",
                     "postalCode", "unreadable"],
        "properties": {
            "isIrsEinLetter": { "type": "boolean" },
            "kind": { "type": "string", "enum": ["CP575", "147C", "other"] },
            "legalName": text,
            "ein": text,
            "street": text,
            "city": text,
            "state": text,
            "postalCode": text,
            "unreadable": text,
        },
    })
}

const SYSTEM: &str = "You read a letter from the IRS that a US business uploaded: the CP 575 \
that assigned its Employer Identification Number (EIN), or the 147C that confirms it. The \
registry matches the business's details to the IRS's records character for character, so copy \
them exactly as printed: the same words, abbreviations, numbers and punctuation (\"ST\" stays \
\"ST\", never \"Street\"); case does not matter. Never correct, complete or guess anything.

- `isIrsEinLetter`: true only for an IRS letter that gives the business's EIN (a CP 575 or a \
147C); false for anything else (a tax return, an invoice, a state registration, a blank page).
- `kind`: `CP575`, `147C` or `other`.
- `legalName`: the business's legal name: the first name line of the letter's address block \
(the line under it, when there is one, is a trade name or DBA: leave it out). On a 147C, the \
name the letter says the EIN is assigned to.
- `ein`: the EIN, as 12-3456789.
- `street`: the street line of the address block, as printed (number, street, suite).
- `city`, `state` (2 letters), `postalCode` (the ZIP, with its 4 more digits when printed).
- `unreadable`: what you could not read with certainty (\"the street line is cut off\"); \"\" \
when everything is clear. A field you cannot read is \"\" and named here.";

/// Reads the uploaded letter (`data_url`: a PDF or a photo) for `company_id`'s
/// registration, asked by `user_id`.
pub async fn read(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    data_url: &str,
    file_name: &str,
) -> Result<TenDlcEinLetter, LetterError> {
    let (kind, data) = parts(data_url)?;
    let file_name: String = file_name.trim().chars().take(FILE_NAME_CHARS).collect();
    let file = if kind == "application/pdf" {
        json!({ "type": "file", "file": {
            "filename": if file_name.is_empty() { "letter.pdf" } else { file_name.as_str() },
            "file_data": format!("data:application/pdf;base64,{data}"),
        } })
    } else {
        json!({ "type": "image_url", "image_url": {
            "url": format!("data:{kind};base64,{data}"), "detail": "high",
        } })
    };
    let messages = json!([
        { "role": "system", "content": SYSTEM },
        { "role": "user", "content": [
            { "type": "text", "text": "The letter:" },
            file,
        ] },
    ]);
    let caller = Caller {
        feature: "tendlc_letter",
        user_id: Some(user_id),
        company_id: Some(company_id),
    };
    let ask = |model: &'static str| {
        let body = json!({
            "model": model,
            "messages": messages,
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "irs_letter", "strict": true, "schema": schema() },
            },
            "reasoning_effort": REASONING_EFFORT,
            "max_completion_tokens": MAX_COMPLETION_TOKENS,
        });
        async move {
            state
                .ai
                .complete(&state.pool, caller, model, &body, AI_TIMEOUT)
                .await
        }
    };
    let message = match ask(LARGE_MODEL).await {
        Err(err) if err.is_model_permission() => ask(LARGE_FALLBACK_MODEL).await?,
        result => result?,
    };
    let read: Read = message["content"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or(AiError::Empty)?;
    letter(read, &file_name)
}

/// What the AI read, as kept: only a whole, clear IRS letter.
fn letter(read: Read, file_name: &str) -> Result<TenDlcEinLetter, LetterError> {
    if !read.is_irs_ein_letter || read.kind == "other" {
        return Err(LetterError::Invalid(
            "This is not an IRS EIN letter. Upload the CP 575 the IRS sent when it gave you the \
             EIN, or the 147C letter it sends on request."
                .into(),
        ));
    }
    let clean = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let ein = tax_id(&read.ein, "US");
    let state = clean(&read.state).to_uppercase();
    let fields = [
        ("legal name", clean(&read.legal_name).is_empty()),
        ("EIN", ein.is_none()),
        ("street address", clean(&read.street).is_empty()),
        ("city", clean(&read.city).is_empty()),
        (
            "state",
            !(state.len() == 2 && state.chars().all(|c| c.is_ascii_uppercase())),
        ),
        (
            "ZIP code",
            clean(&read.postal_code)
                .chars()
                .filter(char::is_ascii_digit)
                .count()
                < 5,
        ),
    ];
    let missing: Vec<&str> = fields
        .iter()
        .filter(|(_, missing)| *missing)
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() || !read.unreadable.trim().is_empty() {
        let what = if missing.is_empty() {
            read.unreadable.trim().to_string()
        } else {
            format!("the {}", missing.join(", "))
        };
        return Err(LetterError::Invalid(format!(
            "We could not read {what} on the letter. Upload a clearer copy of the whole letter."
        )));
    }
    Ok(TenDlcEinLetter {
        kind: read.kind,
        file_name: file_name.to_string(),
        uploaded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        legal_name: clean(&read.legal_name),
        ein: ein.unwrap_or_default(),
        street: clean(&read.street),
        city: clean(&read.city),
        state,
        postal_code: clean(&read.postal_code),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read() -> Read {
        Read {
            is_irs_ein_letter: true,
            kind: "CP575".into(),
            legal_name: " GRANITE DEPOT  OF INDIANAPOLIS LLC ".into(),
            ein: "921340005".into(),
            street: "1175 W 16TH ST".into(),
            city: "INDIANAPOLIS".into(),
            state: "in".into(),
            postal_code: "46202".into(),
            unreadable: String::new(),
        }
    }

    #[test]
    fn only_pdfs_and_photos_up_to_4_mb_are_read() {
        assert_eq!(
            parts("data:application/pdf;base64,JVBERi0xLjQK").unwrap().0,
            "application/pdf"
        );
        assert!(parts("data:image/heic;base64,AAAA").is_err());
        assert!(parts("data:application/pdf,plain").is_err());
        assert!(parts("data:image/png;base64,not base64!").is_err());
        let big = format!(
            "data:image/png;base64,{}",
            "A".repeat(MAX_LETTER_BYTES / 3 * 4 + 8)
        );
        assert!(parts(&big).unwrap_err().to_string().contains("too big"));
    }

    #[test]
    fn a_clear_irs_letter_is_kept_as_written() {
        let kept = letter(read(), "cp575.pdf").expect("read");
        assert_eq!(kept.legal_name, "GRANITE DEPOT OF INDIANAPOLIS LLC");
        assert_eq!(kept.ein, "92-1340005");
        assert_eq!(kept.street, "1175 W 16TH ST");
        assert_eq!(kept.state, "IN");
        assert_eq!(kept.file_name, "cp575.pdf");

        let mut other = read();
        other.is_irs_ein_letter = false;
        assert!(
            letter(other, "x.pdf")
                .unwrap_err()
                .to_string()
                .starts_with("This is not an IRS EIN letter")
        );
        let mut blurry = read();
        blurry.street = String::new();
        blurry.unreadable = "the street line is cut off".into();
        assert_eq!(
            letter(blurry, "x.jpg").unwrap_err().to_string(),
            "We could not read the street address on the letter. Upload a clearer copy of the \
             whole letter."
        );
    }
}

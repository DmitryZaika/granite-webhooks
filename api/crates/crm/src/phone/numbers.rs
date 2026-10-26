//! Phone numbers for Granite Phone: E.164, the last 10 digits customers are
//! matched by, which numbers the app may call, and digits for text to speech.

/// Area codes of NANP countries other than the US and Canada (the
/// Caribbean, Bermuda), and premium-rate 900: `+1` numbers Telnyx bills as
/// international or premium calls.
const OTHER_NANP_AREAS: [&str; 22] = [
    "242", "246", "264", "268", "284", "345", "441", "473", "649", "658", "664", "721", "758",
    "767", "784", "809", "829", "849", "868", "869", "876", "900",
];

/// Non-geographic and special-service area codes no business call goes to:
/// 456 (inbound international), the 5XX personal communications codes,
/// 600 (Canadian services), 700 (carrier services) and 710 (the US
/// government).
const SPECIAL_NANP_AREAS: [&str; 22] = [
    "456", "500", "521", "522", "523", "524", "525", "526", "527", "528", "529", "530", "531",
    "532", "533", "544", "566", "577", "588", "600", "700", "710",
];

/// The pay-per-call exchange (`NPA-976-XXXX`), premium in every area code.
const PREMIUM_EXCHANGE: &str = "976";

/// Service and emergency codes. The app dials them with the phone's own
/// dialer, never through Telnyx.
const N11_CODES: [&str; 11] = [
    "211", "311", "411", "511", "611", "711", "811", "911", "988", "112", "933",
];

/// The emergency codes among them (911, 112, the 933 address test line and
/// the 988 crisis line): a call to one is never refused.
const EMERGENCY_CODES: [&str; 4] = ["911", "112", "933", "988"];

/// The E.164 form of a typed number.
///
/// Ten digits, or eleven starting with 1, are US/Canada (`+1...`); a number
/// typed with `+` keeps its country code (8 to 15 digits, and `+1` needs
/// exactly 10 more). Spaces, dashes, dots and brackets are ignored;
/// anything else (letters, extensions) makes it invalid.
pub fn to_e164(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let (plus, rest) = trimmed
        .strip_prefix('+')
        .map_or((false, trimmed), |rest| (true, rest));
    if !rest
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '-' | '.' | '(' | ')'))
    {
        return None;
    }
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    if plus {
        return match digits.as_bytes().first() {
            Some(b'1') => (digits.len() == 11).then(|| format!("+{digits}")),
            Some(b'2'..=b'9') => (8..=15)
                .contains(&digits.len())
                .then(|| format!("+{digits}")),
            _ => None,
        };
    }
    match digits.len() {
        10 => Some(format!("+1{digits}")),
        11 if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

/// The last 10 digits of a number, however it was typed (how customers'
/// free-text phones are matched); `None` under 10 digits.
pub fn last10(input: &str) -> Option<String> {
    let digits: Vec<char> = input.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then(|| digits[digits.len() - 10..].iter().collect())
}

/// A US or Canadian number Granite Phone may call.
///
/// `+1`, an area code and an exchange that both start with 2-9 and are not
/// N11 codes, not an area code of another NANP country (the Caribbean,
/// Bermuda), premium 900 or a non-geographic or special-service code (456,
/// the 5XX personal codes, 600, 700, 710), and not the pay-per-call 976
/// exchange. Toll-free numbers are fine.
pub fn is_nanp(e164: &str) -> bool {
    let Some(national) = e164.strip_prefix("+1") else {
        return false;
    };
    let digits = national.as_bytes();
    if digits.len() != 10 || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let n11 = |code: &[u8]| code[1] == b'1' && code[2] == b'1';
    let area = &national[..3];
    digits[0] >= b'2'
        && digits[3] >= b'2'
        && !n11(&digits[0..3])
        && !n11(&digits[3..6])
        && !OTHER_NANP_AREAS.contains(&area)
        && !SPECIAL_NANP_AREAS.contains(&area)
        && &national[3..6] != PREMIUM_EXCHANGE
}

/// A service or emergency code: 211, 311, 411, 511, 611, 711, 811, 911,
/// 988, 933 or 112, as dialed (punctuation ignored, also with a leading 1,
/// or as the user of a `sip:` URI).
pub fn is_n11_or_emergency(dialed: &str) -> bool {
    n11_code(dialed).is_some()
}

/// The emergency code dialed (`911`, `112`, `933` or `988`, read as
/// [`n11_code`] reads it); `None` for anything else, the other N11 codes
/// included.
pub fn emergency_code(dialed: &str) -> Option<&'static str> {
    n11_code(dialed).filter(|code| EMERGENCY_CODES.contains(code))
}

/// Whether `code` (as [`n11_code`] gives it) is an emergency code.
pub fn is_emergency(code: &str) -> bool {
    EMERGENCY_CODES.contains(&code)
}

/// The service or emergency code dialed (`911`, `988`, `112` ...) when
/// [`is_n11_or_emergency`]; `None` for anything else.
pub fn n11_code(dialed: &str) -> Option<&'static str> {
    let dialed = dialed.trim();
    let dialed = dialed.strip_prefix("sip:").unwrap_or(dialed);
    let user = dialed.split('@').next().unwrap_or_default();
    if !user
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '+' | '-' | '(' | ')'))
    {
        return None;
    }
    let digits: String = user.chars().filter(char::is_ascii_digit).collect();
    let code = if digits.len() == 4 {
        digits.strip_prefix('1').unwrap_or(&digits)
    } else {
        &digits
    };
    N11_CODES.iter().copied().find(|n11| *n11 == code)
}

/// Digits for text to speech, grouped the way a number is read out: a
/// US/Canada number as "3 1 7, 5 5 5, 0 1 4 2" (no country code), anything
/// else in threes ("1 2 3, 4 5 6").
pub fn spoken_digits(number: &str) -> String {
    let digits: Vec<char> = number.chars().filter(char::is_ascii_digit).collect();
    let national = if digits.len() == 11 && digits[0] == '1' {
        &digits[1..]
    } else {
        &digits[..]
    };
    let groups: Vec<&[char]> = if national.len() == 10 {
        vec![&national[..3], &national[3..6], &national[6..]]
    } else {
        national.chunks(3).collect()
    };
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .map(char::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_numbers_become_e164() {
        for typed in [
            "3175550142",
            "317-555-0142",
            "(317) 555-0142",
            "1 317 555 0142",
            "+1 (317) 555.0142",
        ] {
            assert_eq!(to_e164(typed).as_deref(), Some("+13175550142"), "{typed}");
        }
        assert_eq!(
            to_e164("+44 20 7946 0958").as_deref(),
            Some("+442079460958")
        );
        for bad in [
            "",
            "+",
            "555-0142",
            "23175550142",
            "+1317555014",
            "+131755501423",
            "+0123456789",
            "317-555-0142 ext 5",
            "1-800-FLOWERS",
            "+12345678901234567",
            "+4412345",
        ] {
            assert_eq!(to_e164(bad), None, "{bad}");
        }
    }

    #[test]
    fn last10_keeps_the_last_ten_digits() {
        assert_eq!(last10("+1 (317) 555-0142").as_deref(), Some("3175550142"));
        assert_eq!(last10("317-555-0142").as_deref(), Some("3175550142"));
        assert_eq!(last10("+44 20 7946 0958").as_deref(), Some("2079460958"));
        assert_eq!(last10("555-0142"), None);
        assert_eq!(last10(""), None);
    }

    #[test]
    fn only_us_and_canada_numbers_are_nanp() {
        for good in [
            "+13175550142",
            "+14165550199",
            "+18005550100",
            "+12025550123",
        ] {
            assert!(is_nanp(good), "{good}");
        }
        for bad in [
            "+18765550142", // Jamaica
            "+18095550142", // Dominican Republic
            "+14415550142", // Bermuda
            "+19005550142", // premium
            "+11235550142", // area code starts with 1
            "+13171550142", // exchange starts with 1
            "+14115550142", // N11 area code
            "+13179110142", // N11 exchange
            "+14565550142", // inbound international
            "+15005550142", // personal communications
            "+15215550142",
            "+15335550142",
            "+15445550142",
            "+15665550142",
            "+15775550142",
            "+15885550142",
            "+16005550142", // Canadian non-geographic
            "+17005550142", // carrier services
            "+17105550142", // US government
            "+13179760142", // pay-per-call exchange
            "+442079460958",
            "3175550142",
            "+1317555014",
            "+1317555014x",
        ] {
            assert!(!is_nanp(bad), "{bad}");
        }
        // Geographic 5XX and 6XX/7XX area codes stay callable, and 976 only
        // counts as the exchange.
        for good in [
            "+15025550142",
            "+15205550142",
            "+15345550142",
            "+16015550142",
            "+17025550142",
            "+19765550142",
        ] {
            assert!(is_nanp(good), "{good}");
        }
    }

    #[test]
    fn service_and_emergency_codes() {
        for code in [
            "211", "311", "411", "511", "611", "711", "811", "911", "988", "112", "933",
        ] {
            assert!(is_n11_or_emergency(code), "{code}");
        }
        for dialed in ["9-1-1", " 911 ", "1911", "+1911", "sip:911@sip.telnyx.com"] {
            assert!(is_n11_or_emergency(dialed), "{dialed}");
            assert_eq!(n11_code(dialed), Some("911"), "{dialed}");
            assert_eq!(emergency_code(dialed), Some("911"), "{dialed}");
        }
        assert_eq!(n11_code("1112"), Some("112"));
        assert_eq!(n11_code("+13175550142"), None);
        // Only 911, 112, 933 and 988 are emergencies; 211 ... 811 are not.
        for code in ["911", "112", "933", "988", "1112", "sip:988@sip.telnyx.com"] {
            assert!(emergency_code(code).is_some(), "{code}");
        }
        for code in [
            "211",
            "311",
            "411",
            "511",
            "611",
            "711",
            "811",
            "+13175550142",
            "",
        ] {
            assert_eq!(emergency_code(code), None, "{code}");
        }
        assert!(is_emergency("933") && !is_emergency("411"));
        for other in [
            "999",
            "9111",
            "91",
            "",
            "+13175550142",
            "sip:gencred911@sip.telnyx.com",
            "*911",
        ] {
            assert!(!is_n11_or_emergency(other), "{other}");
        }
    }

    #[test]
    fn digits_are_spoken_in_groups() {
        assert_eq!(spoken_digits("+13175550142"), "3 1 7, 5 5 5, 0 1 4 2");
        assert_eq!(spoken_digits("317-555-0142"), "3 1 7, 5 5 5, 0 1 4 2");
        assert_eq!(spoken_digits("123456"), "1 2 3, 4 5 6");
        assert_eq!(spoken_digits("+442079460958"), "4 4 2, 0 7 9, 4 6 0, 9 5 8");
        assert_eq!(spoken_digits(""), "");
    }
}

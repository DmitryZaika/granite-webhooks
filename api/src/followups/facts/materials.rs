//! Stone and color names in a customer's history (part of
//! `smsFlowAiFollowupFacts.ts`).

use super::super::js::{fancy_test, trim};
use super::types::MaterialReplacementRelationship;
use crate::re;
use regex::Regex;

pub(crate) fn material_name_re() -> &'static Regex {
    re!(
        r"\b(msi quartz|dekton|black mist|taj mahal|calacatta(?:\s+(?:laza|oro|gold|viola|borghini))*|carrara|absolute black|white ice|fantasy brown|leathered|honed|statuario|cashmere|concrete|soapstone|fusion|mazaratta|izaro|unique|cristallo|turquoise|viraldi|idillio|vincent black|black pearl)\b",
        "i"
    )
}

pub fn confirmed_selection_re() -> &'static Regex {
    re!(
        r"\b(picked|selected|going with|chose|ordered|going to order)\b",
        "i"
    )
}

pub(crate) fn material_list_intro_re() -> &'static Regex {
    re!(
        r"(was her )?liked:|likes:|interested in:|favorites?:|options:|materials:|colors:",
        "i"
    )
}

pub(crate) fn section_header_re() -> &'static Regex {
    re!(
        r"^(Activity:|Deal:|Notes:|Deal note|Customer message:|Lead details:|Project type:|Name:|Phone:|Sink:|Backsplash:|You \()",
        "i"
    )
}

/// `COUNTERTOP_MATERIAL_RE` with lookahead (`fancy-regex`) for `matchAll`.
pub(crate) fn countertop_material_fancy() -> &'static fancy_regex::Regex {
    crate::fre!(
        r"\b((?:msi\s+)?dekton(?:\s+(?!quote\b)\w+)?|black mist|taj mahal|white spring|calacatta(?:\s+(?:laza|oro|gold|viola|borghini))+|calacatta|carrara|absolute black|white ice|fantasy brown|lincoln snow|infinity white|ornamental white|butter scotch|samira white|soapstone|fusion|mazaratta(?:\s+dolo(?:mite|ite))?|izaro|unique|bianco absoluto|frost white|devinity|mirag(?:io|g)(?:\s+cove)?|vincent black(?:\s+honed)?|black pearl(?:\s+leathered)?)\b",
        "i"
    )
}

fn cabinet_preference_re() -> &'static Regex {
    re!(
        r"\b((?:karamel|white|black|gray|grey)(?:\s+cabinets?)?|shaker|white shaker|maple cabinets?|oak cabinets?)\b",
        "gi"
    )
}

/// `MATERIAL_TYPE_RE` (`/gi` with lookahead) for `matchAll`.
pub(crate) fn material_type_fancy() -> &'static fancy_regex::Regex {
    crate::fre!(
        r"\b(quartz|soap\s*stone|soapstone|marble|quartzite|dekton|granite)\b(?!\s+depot)(?!-manager)",
        "i"
    )
}

pub(crate) fn generic_material_word_re() -> &'static Regex {
    re!(
        r"^(quartz|soapstone|soap stone|granite|marble|quartzite|dekton|island|kitchen|sink|backsplash|countertop|cabinets?|special|package|white|black)$",
        "i"
    )
}

pub(crate) fn invalid_material_entity_re() -> &'static Regex {
    re!(
        r"^(it|this|that|them|one|something|anything|option|options|design|quote|project|the|a|an|stones?|inches?|feet|foot|ft|sqft|sf|creates?|going|today|saw|we|and|of|by|for|with|from|into|over)$",
        "i"
    )
}

pub(crate) fn measurement_or_grammar_material_re() -> &'static Regex {
    re!(
        r"^(inches?|feet|foot|ft|sq\.?\s*ft|sqft|sf|this creates|we saw|going to|and \d)$",
        "i"
    )
}

/// `uniquePreferenceNames` over the first captures of `matches`.
fn unique_preference_names<'a>(matches: impl Iterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for name in matches.flatten() {
        let name = trim(name);
        if name.is_empty() {
            continue;
        }
        let title = re!(r"\s+", "g").replace_all(name, " ").into_owned();
        if !found
            .iter()
            .any(|item| item.to_lowercase() == title.to_lowercase())
        {
            found.push(title);
        }
    }
    found
}

fn countertop_material_names(text: &str) -> Vec<String> {
    let names: Vec<String> = countertop_material_fancy()
        .captures_iter(text)
        .filter_map(Result::ok)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect();
    unique_preference_names(names.iter().map(|name| Some(name.as_str())))
}

pub fn extract_historical_materials(profile: &str) -> Vec<String> {
    let mut named = countertop_material_names(profile);
    let looked_at = crate::fre!(
        r"looked at (?!it\b|the\b|your\b)([A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,1})\b",
        ""
    );
    for caps in looked_at.captures_iter(profile).filter_map(Result::ok) {
        let Some(raw) = caps.get(1).map(|m| trim(m.as_str())) else {
            continue;
        };
        if raw.is_empty() || invalid_material_entity_re().is_match(raw) {
            continue;
        }
        let title = title_case_material(raw);
        if !is_valid_material_entity(&title) {
            continue;
        }
        if !named
            .iter()
            .any(|item| item.to_lowercase() == title.to_lowercase())
        {
            named.push(title);
        }
    }
    named
}

/// `titleCaseMaterial`: each word capitalized; MSI and CT upper case.
pub fn title_case_material(name: &str) -> String {
    re!(r"\s+")
        .split(name)
        .map(|word| {
            if re!(r"^(msi|ct)$", "i").is_match(word) {
                return word.to_uppercase();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                format!("{}{}", first.to_uppercase(), chars.as_str())
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_material_list_item(line: &str) -> bool {
    let without_favorite = re!(r"\(\s*favorite\s*\)", "i").replace(line, "");
    let cleaned_owned = re!(r"^[-*•]\s*", "u")
        .replace(&without_favorite, "")
        .into_owned();
    let cleaned = trim(&cleaned_owned);
    if cleaned.is_empty() || super::super::js::js_len(cleaned) > 50 {
        return false;
    }
    if re!(r"[.?!]$").is_match(cleaned) {
        return false;
    }
    if section_header_re().is_match(cleaned) {
        return false;
    }
    if re!(r"^(will|remove|need|asked|she will|he will|customer )", "i").is_match(cleaned) {
        return false;
    }
    let words = re!(r"\s+").split(cleaned).count();
    (1..=5).contains(&words)
}

/// `activityName`: the activity's name from an `Activity:` line.
pub fn activity_name(line: &str) -> String {
    let stripped = re!(r"^Activity:\s*", "u").replace(line, "");
    trim(stripped.split(" · ").next().unwrap_or_default()).to_string()
}

pub fn extract_activity_search_materials(profile: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for raw in profile.split('\n') {
        if !re!(r"^Activity:", "i").is_match(raw) {
            continue;
        }
        let name = activity_name(raw);
        for caps in re!(
            r"find\s+([A-Za-z][\w']*(?:\s+[A-Za-z][\w']*){0,3})(?:\s+for\s+|\s+and\s+|$)",
            "gi"
        )
        .captures_iter(&name)
        {
            let Some(raw_name) = caps.get(1).map(|m| trim(m.as_str())) else {
                continue;
            };
            if raw_name.is_empty() || re!(r"^(the|a|quote)$", "i").is_match(raw_name) {
                continue;
            }
            let title = title_case_material(&re!(r"doloite", "i").replace(raw_name, "dolomite"));
            if !is_valid_material_entity(&title) {
                continue;
            }
            if !found
                .iter()
                .any(|item| item.to_lowercase() == title.to_lowercase())
            {
                found.push(title);
            }
        }
    }
    found
}

/// Names a profile lists as liked, and the one marked favorite.
pub fn extract_listed_material_preferences(profile: &str) -> (Vec<String>, Option<String>) {
    let mut names: Vec<String> = Vec::new();
    let mut favorite: Option<String> = None;
    let mut add = |raw: &str, is_favorite: bool| {
        let name = title_case_material(trim(&re!(r"\(\s*favorite\s*\)", "i").replace(raw, "")));
        if name.is_empty() {
            return;
        }
        if invalid_material_entity_re().is_match(&name)
            || measurement_or_grammar_material_re().is_match(&name)
        {
            return;
        }
        if !names
            .iter()
            .any(|item| item.to_lowercase() == name.to_lowercase())
        {
            names.push(name.clone());
        }
        if is_favorite {
            favorite = Some(name);
        }
    };
    let favorite_re = re!(r"\(\s*favorite\s*\)", "i");
    let mut in_list = false;
    for raw_line in profile.split('\n') {
        let line = trim(raw_line);
        if material_list_intro_re().is_match(line) || re!(r"was her liked:?", "i").is_match(line) {
            in_list = true;
            let after_owned = re!(
                r"^.*((?:was her )?liked:|likes:|interested in:|favorites?:|options:|materials:|colors:)\s*",
                "iu"
            )
            .replace(line, "")
            .into_owned();
            let after = trim(&after_owned);
            if !after.is_empty() && looks_like_material_list_item(after) {
                add(after, favorite_re.is_match(after));
            }
            continue;
        }
        if let Some(caps) =
            re!(r"\bliked?\s+([a-z][\w\s]+?)\s+and\s+([a-z][\w\s]+)", "i").captures(line)
            && let (Some(first), Some(second)) = (caps.get(1), caps.get(2))
        {
            add(first.as_str(), false);
            add(second.as_str(), false);
            continue;
        }
        if let Some(inline) = re!(r"Liked\s+([A-Za-z][A-Za-z0-9'',\s-]+)$", "i")
            .captures(line)
            .and_then(|caps| caps.get(1))
        {
            for part in inline.as_str().split(',') {
                let name = trim(part);
                if !name.is_empty() && looks_like_material_list_item(name) {
                    add(name, false);
                }
            }
            continue;
        }
        if !in_list {
            if favorite_re.is_match(line) && looks_like_material_list_item(line) {
                add(line, true);
            }
            continue;
        }
        if line.is_empty()
            || section_header_re().is_match(line)
            || !looks_like_material_list_item(line)
        {
            in_list = false;
            continue;
        }
        add(line, favorite_re.is_match(line));
    }
    for name in extract_inline_named_materials(profile) {
        add(&name, false);
    }
    (names, favorite)
}

const NAMED_STONE_IMPLIES_TYPE: [(&str, &str); 15] = [
    ("white spring", "Granite"),
    ("black pearl", "Granite"),
    ("taj mahal", "Granite"),
    ("fantasy brown", "Granite"),
    ("absolute black", "Granite"),
    ("lincoln snow", "Granite"),
    ("ornamental white", "Granite"),
    ("butter scotch", "Granite"),
    ("samira white", "Granite"),
    ("black mist", "Quartz"),
    ("dekton", "Dekton"),
    ("soapstone", "Soapstone"),
    ("calacatta", "Quartz"),
    ("carrara", "Marble"),
    ("statuario", "Marble"),
];

/// `normalizeMaterialEntity`: single spaces, trimmed.
pub fn normalize_material_entity(text: &str) -> String {
    trim(&re!(r"\s+", "g").replace_all(text, " ")).to_string()
}

/// The stone type a well-known stone name implies (Black Pearl is granite).
pub fn infer_material_type_for_named_stone(name: &str) -> Option<&'static str> {
    // `/\b<name>\b/i` for each entry.
    let lower = name.to_lowercase();
    NAMED_STONE_IMPLIES_TYPE.iter().find_map(|(stone, kind)| {
        let mut start = 0;
        while let Some(found) = lower[start..].find(stone) {
            let at = start + found;
            let end = at + stone.len();
            let before_ok = at == 0 || !is_js_word(lower[..at].chars().next_back());
            let after_ok = end == lower.len() || !is_js_word(lower[end..].chars().next());
            if before_ok && after_ok {
                return Some(*kind);
            }
            start = at + 1;
        }
        None
    })
}

fn is_js_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a name can be a stone or color (not a pronoun, unit or filler).
pub fn is_valid_material_entity(name: &str) -> bool {
    let candidate = normalize_material_entity(name);
    if super::super::js::js_len(&candidate) < 2 {
        return false;
    }
    if invalid_material_entity_re().is_match(&candidate)
        || generic_material_word_re().is_match(&candidate)
        || measurement_or_grammar_material_re().is_match(&candidate)
    {
        return false;
    }
    if re!(r"^\d").is_match(&candidate) {
        return false;
    }
    if infer_material_type_for_named_stone(&candidate).is_some() {
        return true;
    }
    if fancy_test(countertop_material_fancy(), &candidate) {
        return true;
    }
    if material_name_re().is_match(&candidate) {
        return true;
    }
    let maker = re!(
        r"^(msi|cambria|cosentino|caesarstone|silestone|dekton)\s+",
        "i"
    );
    if re!(
        r"^(msi|cambria|cosentino|caesarstone|silestone|dekton)\s+\S+",
        "i"
    )
    .is_match(&candidate)
    {
        let without_maker = maker.replace(&candidate, "");
        if !without_maker.is_empty() && !invalid_material_entity_re().is_match(&without_maker) {
            return true;
        }
    }
    let words: Vec<&str> = re!(r"\s+").split(&candidate).collect();
    if (1..=4).contains(&words.len()) {
        let proper_noun = words.iter().all(|word| {
            re!(r"^[A-Z][a-zA-Z0-9'’-]*$").is_match(word) || re!(r"^(msi|ct)$", "i").is_match(word)
        });
        if proper_noun {
            return true;
        }
    }
    false
}

/// Drops a name when a longer one starts or ends with it.
pub fn prefer_longest_material_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| {
            let lower = name.to_lowercase();
            !names.iter().any(|other| {
                if other == *name {
                    return false;
                }
                let other_lower = other.to_lowercase();
                other_lower.starts_with(&format!("{lower} "))
                    || other_lower.ends_with(&format!(" {lower}"))
            })
        })
        .cloned()
        .collect()
}

pub fn filter_valid_material_entities(names: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for name in names {
        let stripped = re!(r"^msi\s+", "i")
            .replace(&normalize_material_entity(name), "")
            .into_owned();
        let normalized = title_case_material(&stripped);
        let normalized_valid = is_valid_material_entity(&normalized);
        if !normalized_valid && !is_valid_material_entity(name) {
            continue;
        }
        // The CRM tests the normalized name a second time here; the call
        // matters for the stateful material test, so it is repeated.
        let stored = if is_valid_material_entity(&normalized) {
            normalized
        } else {
            title_case_material(&normalize_material_entity(name))
        };
        if !found
            .iter()
            .any(|item| item.to_lowercase() == stored.to_lowercase())
        {
            found.push(stored);
        }
    }
    prefer_longest_material_names(&found)
}

/// Quote states imply the earlier ones (received ⇒ sent ⇒ created).
pub const fn apply_quote_state_implications(
    received_or_discussed: bool,
    sent: bool,
    created: bool,
) -> (bool, bool, bool) {
    if received_or_discussed && !sent {
        (received_or_discussed, true, true)
    } else if sent && !created {
        (received_or_discussed, sent, true)
    } else {
        (received_or_discussed, sent, created)
    }
}

fn extract_inline_named_materials(profile: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for raw_line in profile.split('\n') {
        let line =
            re!(r"^(Customer message|Lead details|Deal note[^:]*):\s*", "iu").replace(raw_line, "");
        let after_size = re!(
            r#"\d+\s*(?:'|ft|feet|")\s*(?:x|by|×)\s*\d+\s*(?:'|ft|feet|"|in|inch)?\s+(.+)$"#,
            "i"
        )
        .captures(&line)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default();
        let source = re!(
            r"\b([A-Za-z][A-Za-z0-9'’-]*(?:\s+[A-Za-z][A-Za-z0-9'’-]*)?)(?:\s*,\s*[A-Za-z][A-Za-z0-9'’-]*(?:\s+[A-Za-z][A-Za-z0-9'’-]*)?){1,6}\b",
            ""
        )
        .find(&after_size)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
        if source.is_empty() {
            continue;
        }
        for part in source.split(',') {
            let name = trim(part);
            if name.is_empty() || generic_material_word_re().is_match(name) {
                continue;
            }
            if !is_valid_material_entity(name) {
                continue;
            }
            if re!(r"\s+").split(name).count() > 4 {
                continue;
            }
            if !found
                .iter()
                .any(|item| item.to_lowercase() == name.to_lowercase())
            {
                found.push(name.to_string());
            }
        }
    }
    found
}

/// Stone types (quartz, granite...) a text names, except granite being removed.
pub fn extract_material_type_preferences(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in material_type_fancy()
        .captures_iter(text)
        .filter_map(Result::ok)
    {
        let Some(group) = caps.get(1) else { continue };
        let raw = trim(&re!(r"\s+", "g").replace_all(group.as_str(), " ")).to_string();
        if raw.is_empty() {
            continue;
        }
        let start = caps.get(0).map_or(0, |m| m.start());
        let around_owned =
            super::super::js::js_window(text, start, 48, super::super::js::js_len(&raw) + 8);
        let around = around_owned.as_str();
        if re!(
            r"remove .{0,40}granite|existing granite|granite removal|remove her granite|remove his granite",
            "i"
        )
        .is_match(around)
        {
            continue;
        }
        if re!(r"chang(?:ing|e)\s+(?:my\s+)?(?:current\s+)?granite", "i").is_match(around)
            && re!(r"granite", "i").is_match(&raw)
        {
            continue;
        }
        let name = if re!(r"soap", "i").is_match(&raw) {
            "Soapstone".to_string()
        } else {
            title_case_material(&raw)
        };
        if !found
            .iter()
            .any(|item| item.to_lowercase() == name.to_lowercase())
        {
            found.push(name);
        }
    }
    found
}

fn normalize_replacement_material_type(raw: &str) -> Option<String> {
    let cleaned = trim(&re!(r"\s+", "g").replace_all(raw, " ")).to_string();
    if cleaned.is_empty() {
        return None;
    }
    let named = [
        (r"soap\s*stone|soapstone", "Soapstone"),
        (r"quartzite", "Quartzite"),
        (r"quartz", "Quartz"),
        (r"granite", "Granite"),
        (r"marble", "Marble"),
        (r"laminate", "Laminate"),
        (r"dekton", "Dekton"),
    ];
    for (pattern, name) in named {
        if super::super::js::js_regex(pattern, "i").is_match(&cleaned) {
            return Some(name.to_string());
        }
    }
    Some(title_case_material(
        re!(r"\s+").split(&cleaned).next().unwrap_or(&cleaned),
    ))
}

/// "Changing my granite to white quartz" and the like.
pub fn parse_material_replacement_relationship(
    text: &str,
) -> Option<MaterialReplacementRelationship> {
    let patterns: [&Regex; 6] = [
        re!(
            r"chang(?:ing|e)\s+(?:my\s+)?(?:current\s+)?(\w+(?:\s+\w+)?)\s+(?:countertops?\s+)?to\s+([^.\n]+)",
            "i"
        ),
        re!(r"replace\s+(\w+(?:\s+\w+)?)\s+with\s+([^.\n]+)", "i"),
        re!(
            r"remove\s+(\w+(?:\s+\w+)?)\s+and\s+install\s+([^.\n]+)",
            "i"
        ),
        re!(r"currently\s+(\w+),?\s+want\s+([^.\n]+)", "i"),
        re!(
            r"want'?s?\s+us\s+to\s+remove\s+(?:her|his|their|the)\s+(granite|quartz|marble|quartzite|countertops?)",
            "i"
        ),
        re!(
            r"remove\s+(?:her|his|their|the)\s+(granite|quartz|marble|quartzite)\b",
            "i"
        ),
    ];
    for pattern in patterns {
        let Some(caps) = pattern.captures(text) else {
            continue;
        };
        let Some(existing_raw) = caps.get(1) else {
            continue;
        };
        let existing_material_type = normalize_replacement_material_type(existing_raw.as_str());
        let target_raw = trim(caps.get(2).map_or("", |m| m.as_str())).to_string();
        let target_material_type = material_type_fancy()
            .captures_iter(&target_raw)
            .filter_map(Result::ok)
            .find_map(|type_caps| {
                let raw = trim(&re!(r"\s+", "g").replace_all(type_caps.get(1)?.as_str(), " "))
                    .to_string();
                if raw.is_empty() {
                    return None;
                }
                Some(if re!(r"soap", "i").is_match(&raw) {
                    "Soapstone".to_string()
                } else {
                    title_case_material(&raw)
                })
            });
        let target_color = re!(
            r"\b(white|gray|grey|black|cream|charcoal|dark green)\b",
            "i"
        )
        .captures(&target_raw)
        .and_then(|caps| caps.get(1))
        .map(|color| title_case_material(color.as_str()))
        .or_else(|| {
            re!(r"white quartz|type of white", "i")
                .is_match(&target_raw)
                .then(|| "White".to_string())
        });
        if existing_material_type.is_some() || target_material_type.is_some() {
            return Some(MaterialReplacementRelationship {
                existing_material_type,
                target_material_type,
                target_color,
                evidence: caps
                    .get(0)
                    .map_or_else(String::new, |m| m.as_str().to_string()),
            });
        }
    }
    None
}

/// Countertop colors the customer asked for (white, gray, neutral...).
pub fn extract_countertop_color_preferences(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut add = |value: &str| {
        let name = title_case_material(&re!(r"s$").replace(value, ""));
        if name.to_lowercase() == "neutral" {
            if !found.iter().any(|item| re!(r"neutral", "i").is_match(item)) {
                found.push("Neutral combinations".to_string());
            }
            return;
        }
        if !found
            .iter()
            .any(|item| item.to_lowercase() == name.to_lowercase())
        {
            found.push(name);
        }
    };
    for raw in text.split('\n') {
        let line = trim(raw);
        if !re!(
            r"color|grey|gray|charcoal|dark green|neutral|medium to dark|white granite|white stone|white\/cream|looking for white|\btan\b|light brown",
            "i"
        )
        .is_match(line)
        {
            continue;
        }
        if re!(r"cabinets?", "i").is_match(line)
            && !re!(r"countertop|color|surface|granite|stone|tan|brown", "i").is_match(line)
        {
            continue;
        }
        if re!(r"combination of neutrals|neutrals", "i").is_match(line) {
            add("Neutral combinations");
        }
        if re!(r"white\/cream|pretty neutral white", "i").is_match(line) {
            add("Neutral white/cream");
        }
        if re!(
            r"white granite|white stone|looking for white|white quartz|type of white",
            "i"
        )
        .is_match(line)
        {
            add("White");
        }
        if re!(r"\btan\b", "i").is_match(line) {
            add("Tan");
        }
        if re!(r"light brown", "i").is_match(line) {
            add("Light brown");
        }
        for caps in re!(r"\b(gray|grey|charcoal|dark green)\b", "gi").captures_iter(line) {
            if let Some(color) = caps.get(1) {
                add(color.as_str());
            }
        }
    }
    if let Some(black) = re!(r"\b(black)\s+countertops?\b", "i")
        .captures(text)
        .and_then(|caps| caps.get(1))
    {
        add(black.as_str());
    }
    found
}

/// Cabinet styles and colors a profile mentions.
pub fn extract_cabinet_preferences(profile: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    if re!(r"\bwhite shaker\b", "i").is_match(profile) {
        found.push("white shaker".to_string());
    }
    if re!(r"\bcabinets and drawers\b", "i").is_match(profile) {
        found.push("cabinets and drawers".to_string());
    }
    if re!(r"\bhave (my )?own hardware\b", "i").is_match(profile) {
        found.push("customer has own hardware".to_string());
    }
    if re!(
        r"\bdelivery and installation\b|\bneed delivery\b|\bneed installation\b",
        "i"
    )
    .is_match(profile)
    {
        found.push("delivery and installation requested".to_string());
    }
    for raw in profile.split('\n') {
        let line = trim(raw);
        if !re!(r"cabinet", "i").is_match(line) {
            continue;
        }
        let names: Vec<String> = cabinet_preference_re()
            .captures_iter(line)
            .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
            .collect();
        for name in unique_preference_names(names.iter().map(|name| Some(name.as_str()))) {
            let normalized = re!(r"^(white|black|gray|grey)(?:\s+cabinets?)?$", "i")
                .captures(&name)
                .and_then(|caps| caps.get(1))
                .map_or_else(|| name.clone(), |color| color.as_str().to_lowercase());
            if !found
                .iter()
                .any(|item| item.to_lowercase() == normalized.to_lowercase())
            {
                found.push(normalized);
            }
        }
    }
    found
}

/// The material of a quote the customer said they got ("got the quartz
/// quote"), unless they also said they prefer it.
pub fn extract_quoted_material_context(text: &str) -> Option<String> {
    let caps = re!(
        r"\bgot (?:the |your |a )?(msi quartz|dekton|black mist|taj mahal|calacatta(?:\s+\w+)?|carrara|absolute black|white ice|fantasy brown|white spring|vincent black|black pearl|quartz|granite|marble|quartzite|soapstone)\s+quote\b",
        "i"
    )
    .captures(text)?;
    let raw = caps.get(1).map(|m| m.as_str()).filter(|m| !m.is_empty())?;
    let material = title_case_material(trim(raw));
    if re!(
        r"\b(lean(?:ing)? toward|prefer|favorite|picked|selected|going with|like|love)\b",
        "i"
    )
    .is_match(text)
    {
        let named = Regex::new(&format!(
            r"(?i)(?-u:\b){}(?-u:\b)",
            regex::escape(&material)
        ));
        if named.is_ok_and(|named| named.is_match(text)) {
            return None;
        }
    }
    Some(material)
}

/// Every stone the customer liked, looked at or searched for.
pub fn extract_material_preferences(profile: &str) -> Vec<String> {
    let (listed, _) = extract_listed_material_preferences(profile);
    let named: Vec<String> = extract_historical_materials(profile)
        .iter()
        .map(|name| title_case_material(name))
        .collect();
    let mut extras = extract_activity_search_materials(profile);
    if re!(
        r"neutral white\/cream|pretty neutral white\/cream|white\/cream",
        "i"
    )
    .is_match(profile)
    {
        extras.push("neutral white/cream".into());
    }
    if re!(r"\bmsi quartz\b", "i").is_match(profile)
        && !named
            .iter()
            .any(|name| re!(r"msi quartz", "i").is_match(name))
    {
        extras.push("MSI Quartz".into());
    }
    let mut found: Vec<String> = Vec::new();
    for name in listed.into_iter().chain(named).chain(extras) {
        if !found
            .iter()
            .any(|item| item.to_lowercase() == name.to_lowercase())
        {
            found.push(name);
        }
    }
    found
}

pub fn extract_favorite_material(profile: &str) -> Option<String> {
    extract_listed_material_preferences(profile).1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_stones_imply_their_type() {
        assert_eq!(
            infer_material_type_for_named_stone("Black Pearl Leathered"),
            Some("Granite")
        );
        assert_eq!(
            infer_material_type_for_named_stone("Calacatta Laza"),
            Some("Quartz")
        );
        assert_eq!(infer_material_type_for_named_stone("blackpearl"), None);
    }

    #[test]
    fn titles_keep_maker_abbreviations() {
        assert_eq!(
            title_case_material("msi calacatta  laza"),
            "MSI Calacatta Laza"
        );
    }

    #[test]
    fn replacement_relationships_name_both_sides() {
        let found = parse_material_replacement_relationship(
            "We are changing my current granite countertops to white quartz.",
        )
        .unwrap();
        assert_eq!(found.existing_material_type.as_deref(), Some("Granite"));
        assert_eq!(found.target_material_type.as_deref(), Some("Quartz"));
        assert_eq!(found.target_color.as_deref(), Some("White"));
    }
}

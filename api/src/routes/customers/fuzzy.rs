//! Typo-tolerant name matching for the customer search. Port of
//! `matchesNameFuzzy` in the Remix app's `app/utils/customerNameSearch.server.ts`;
//! the SQL fetches loose candidates and this decides which ones match.

/// Damerau-Levenshtein distance (an adjacent swap counts as one edit).
pub fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                dp[i][j] = dp[i][j].min(dp[i - 2][j - 2] + 1);
            }
        }
    }
    dp[a.len()][b.len()]
}

fn is_adjacent_transposition(a: &[char], b: &[char]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for i in 0..a.len().saturating_sub(1) {
        if a[i] != b[i] {
            return a[i] == b[i + 1] && a[i + 1] == b[i] && a[i + 2..] == b[i + 2..];
        }
    }
    false
}

/// Drops name suffixes (`Jr.`, `Sr`, `II`, `III`, `IV`) and extra spaces.
pub fn normalize_term(term: &str) -> String {
    term.split_whitespace()
        .filter(|word| {
            let bare = word.trim_end_matches('.').to_lowercase();
            !matches!(bare.as_str(), "jr" | "sr" | "ii" | "iii" | "iv")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn word_typo_match(name_word: &[char], term_word: &[char]) -> bool {
    match levenshtein(name_word, term_word) {
        0 => true,
        1 => is_adjacent_transposition(name_word, term_word) || name_word.len() != term_word.len(),
        _ => false,
    }
}

fn word_matches(name_word: &str, term_word: &str) -> bool {
    let name_chars: Vec<char> = name_word.chars().collect();
    let term_chars: Vec<char> = term_word.chars().collect();
    if word_typo_match(&name_chars, &term_chars) {
        return true;
    }
    if name_word.starts_with(term_word) || term_word.starts_with(name_word) {
        return true;
    }
    let term_len = term_chars.len();
    let name_len = name_chars.len();
    if term_len >= 3 && name_word.contains(term_word) {
        return true;
    }
    if name_len >= 3 && term_word.contains(name_word) {
        return true;
    }
    let initial = name_word.replace('.', "");
    let initial = initial.trim();
    initial.chars().count() == 1 && term_word.starts_with(initial)
}

/// Every word of `term` appears in `name`, allowing one typo per word,
/// prefixes, and initials ("J. Smith" matches "john smith").
pub fn matches_name_fuzzy(name: &str, term: &str) -> bool {
    let name = name.to_lowercase();
    let term = normalize_term(term).to_lowercase();
    let term_words: Vec<&str> = term.split_whitespace().collect();
    if term_words.is_empty() {
        return true;
    }
    let name_words: Vec<&str> = name.split_whitespace().collect();
    term_words.iter().all(|term_word| {
        name.contains(term_word)
            || name_words
                .iter()
                .any(|name_word| word_matches(name_word, term_word))
    })
}

/// A customer found by name.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NamedCustomer {
    pub id: i32,
    pub name: Option<String>,
    pub company_name: Option<String>,
    pub phone: Option<String>,
    pub phone_2: Option<String>,
}

/// 0 exact, 1 the name starts with the term as a word, 2 contains it, 3 else.
fn name_rank(name: &str, term: &str) -> u8 {
    let name = name.trim().to_lowercase();
    let term = normalize_term(term).to_lowercase();
    if name == term {
        0
    } else if name.starts_with(&format!("{term} ")) {
        1
    } else if name.contains(&term) {
        2
    } else {
        3
    }
}

/// The company's live customers whose name or company matches `term`, typos
/// allowed, best match first. The SQL fetches loose candidates (with several
/// words, N-1 of N hits) and [`matches_name_fuzzy`] decides. Port of
/// `findCustomersByFuzzyName`.
pub async fn find_by_name(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    term: &str,
    limit: usize,
) -> Result<Vec<NamedCustomer>, sqlx::Error> {
    let term = term.trim();
    if term.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let normalized = normalize_term(term);
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT id, name, company_name, phone, phone_2 FROM customers
          WHERE company_id = ",
    );
    sql.push_bind(company_id)
        .push(" AND deleted_at IS NULL AND (name LIKE ");
    let short = |word: &str| format!("%{}%", word.chars().take(3).collect::<String>());
    if words.len() > 1 {
        sql.push_bind(format!("%{}%", words.join(" ")))
            .push(" OR (");
        for (index, word) in words.iter().enumerate() {
            if index > 0 {
                sql.push(" + ");
            }
            sql.push("(name LIKE ")
                .push_bind(short(word))
                .push(" OR name LIKE ")
                .push_bind(format!("%{word}%"))
                .push(")");
        }
        sql.push(format!(") >= {}", words.len() - 1));
    } else {
        sql.push_bind(short(&normalized));
        let chars: Vec<char> = normalized.chars().collect();
        if chars.len() > 3 {
            let tail: String = chars[chars.len() - 3..].iter().collect();
            sql.push(" OR name LIKE ").push_bind(format!("%{tail}%"));
        }
    }
    sql.push(" OR (company_name IS NOT NULL AND company_name != '' AND (company_name LIKE ")
        .push_bind(format!("{normalized}%"))
        .push(" OR company_name LIKE ")
        .push_bind(format!("%{normalized}%"))
        .push(")))")
        .push(" ORDER BY name LIMIT ")
        .push_bind(i64::try_from(limit.max(100)).unwrap_or(100));
    let mut rows: Vec<NamedCustomer> = sql.build_query_as().fetch_all(pool).await?;
    rows.retain(|c| {
        matches_name_fuzzy(c.name.as_deref().unwrap_or_default(), term)
            || matches_name_fuzzy(c.company_name.as_deref().unwrap_or_default(), term)
    });
    rows.sort_by(|a, b| {
        let (a_name, b_name) = (
            a.name.as_deref().unwrap_or_default(),
            b.name.as_deref().unwrap_or_default(),
        );
        name_rank(a_name, term)
            .cmp(&name_rank(b_name, term))
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
    });
    rows.truncate(limit);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn levenshtein_counts_swaps_as_one_edit() {
        assert_eq!(levenshtein(&chars("smith"), &chars("smith")), 0);
        assert_eq!(levenshtein(&chars("smith"), &chars("smiht")), 1);
        assert_eq!(levenshtein(&chars("smith"), &chars("smyth")), 1);
        assert_eq!(levenshtein(&chars("smith"), &chars("smit")), 1);
        assert_eq!(levenshtein(&chars(""), &chars("abc")), 3);
        assert_eq!(levenshtein(&chars("kitten"), &chars("sitting")), 3);
    }

    #[test]
    fn suffixes_are_ignored() {
        assert_eq!(normalize_term("  John   Smith Jr. "), "John Smith");
        assert_eq!(normalize_term("Henry IV"), "Henry");
        assert_eq!(normalize_term("Ivy"), "Ivy");
    }

    #[test]
    fn matches_words_prefixes_and_typos() {
        assert!(matches_name_fuzzy("Alice Lead", "alice"));
        assert!(matches_name_fuzzy("Alice Lead", "Ali"));
        assert!(matches_name_fuzzy("Alice Lead", "lead alice"));
        // One swapped pair or one extra/missing letter per word.
        assert!(matches_name_fuzzy("Alice Lead", "Alcie"));
        assert!(matches_name_fuzzy("Alice Lead", "Alic Led"));
        // Initials.
        assert!(matches_name_fuzzy("J. Smith", "john smith"));
        assert!(matches_name_fuzzy("Zoë Ünicode-Łukasz", "zoë"));
        assert!(matches_name_fuzzy("anything", ""));
    }

    #[test]
    fn rejects_other_names() {
        assert!(!matches_name_fuzzy("Alice Lead", "bob"));
        assert!(!matches_name_fuzzy("Alice Lead", "alice smith"));
        // Same length, one letter replaced: not a typo match.
        assert!(!matches_name_fuzzy("Carol", "Carl x"));
        assert!(!matches_name_fuzzy("Bob", "Bab"));
    }
}

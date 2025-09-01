use rand::Rng;
use teloxide::types::MaybeInaccessibleMessage;

pub fn parse_assign(data: &str) -> Option<(i32, i64)> {
    let parts: Vec<&str> = data.split(':').collect();
    if parts.len() == 3 && parts[0] == "assign" {
        let lead_id = parts[1].parse().ok()?;
        let user_id = parts[2].parse().ok()?;
        Some((lead_id, user_id))
    } else {
        None
    }
}

pub fn lead_url(deal_id: u64) -> String {
    format!("https://granite-manager.com/employee/deals/edit/{deal_id}/project")
}

pub fn is_email(text: &str) -> bool {
    let email = text.trim();
    if email.len() < 3 || email.len() > 254 {
        return false;
    }
    email.contains('@')
}

/// Из /start <email> вытаскиваем email (поддерживает /start@YourBot)
pub fn parse_slash_email(text: &str) -> Option<String> {
    let mut it = text.split_whitespace();
    let cmd = it.next()?;
    if !(cmd == "/email" || cmd.starts_with("/email@")) {
        return None;
    }
    let email = it.next()?; // ожидание: /start user@example.com
    if !is_email(email) {
        return None;
    }
    Some(email.to_string())
}

pub fn parse_code(text: &str) -> Option<i32> {
    let code = text.trim();
    if code.len() != 6 {
        return None;
    }
    code.parse().ok()
}

pub fn gen_code() -> i32 {
    rand::rng().random_range(100_000..=999_999)
}

pub fn extract_message(message: &MaybeInaccessibleMessage) -> Option<String> {
    if let MaybeInaccessibleMessage::Regular(msg) = message {
        return msg.text().map(std::string::ToString::to_string);
    }
    None
}

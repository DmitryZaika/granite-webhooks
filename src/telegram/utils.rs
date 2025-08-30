use rand::Rng;

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

pub fn lead_url(lead_id: i32) -> String {
    format!("https://granite-manager.com/employee/deals/edit/{lead_id}/project")
}

/// Из /start <email> вытаскиваем email (поддерживает /start@YourBot)
pub fn parse_start_email(text: &str) -> Option<String> {
    let mut it = text.split_whitespace();
    let cmd = it.next()?;
    if !(cmd == "/start" || cmd.starts_with("/start@")) {
        return None;
    }
    let email = it.next()?; // ожидание: /start user@example.com
    Some(email.to_string())
}

pub fn gen_code() -> String {
    let n: u32 = rand::rng().random_range(0..=999_999);
    format!("{n:06}")
}

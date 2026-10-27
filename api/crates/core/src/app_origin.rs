//! Links to the CRM: the app origin of a company (its subdomain of `APP_URL`).

/// The app origin for the user's company: `https://{subdomain}.{apex}` when
/// the company has a subdomain and the app is not on localhost, else `APP_URL`.
pub fn app_origin(app_url: &str, subdomain: Option<&str>) -> String {
    let Some(subdomain) = subdomain else {
        return app_url.to_string();
    };
    let Some((scheme, rest)) = app_url.split_once("://") else {
        return app_url.to_string();
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let bare = host.split(':').next().unwrap_or(host);
    if bare == "localhost"
        || bare.ends_with(".localhost")
        || bare.parse::<std::net::IpAddr>().is_ok()
    {
        return app_url.to_string();
    }
    let apex = host.strip_prefix("www.").unwrap_or(host);
    format!("{scheme}://{subdomain}.{apex}")
}

/// The company's subdomain (lowercase), when it has one.
pub async fn company_subdomain(
    pool: &sqlx::MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<String>> {
    let subdomain: Option<Option<String>> =
        sqlx::query_scalar("SELECT subdomain FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(subdomain
        .flatten()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::app_origin;

    #[test]
    fn reset_links_use_the_company_host() {
        assert_eq!(
            app_origin("https://granite-manager.com", Some("acme")),
            "https://acme.granite-manager.com"
        );
        assert_eq!(
            app_origin("https://www.granite-manager.com", Some("acme")),
            "https://acme.granite-manager.com"
        );
        assert_eq!(
            app_origin("http://localhost:5173", Some("acme")),
            "http://localhost:5173"
        );
        assert_eq!(
            app_origin("https://granite-manager.com", None),
            "https://granite-manager.com"
        );
    }
}

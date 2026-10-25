//! A company domain's email deliverability, from DNS.
//!
//! Scores 0-100 (higher = more spam risk) from MX, SPF, DMARC, common DKIM
//! selectors and the Spamhaus / Nordspam domain blocklists, with the DNS
//! changes that would lower it; results are kept a week in
//! `company_domain_trust`. Port of the CRM's
//! `deliverability.provider.server.ts` and `domainReputation.server.ts`.

use crate::openapi::ToSchema;
use regex::Regex;
use serde::Serialize;
use sqlx::MySqlPool;
use std::sync::LazyLock;

/// A DNS answer that is not records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The name or record type does not exist.
    NotFound,
    /// The resolver failed (timeout, refused).
    Failed,
}

/// What the check asks DNS; a stub in tests.
#[allow(async_fn_in_trait)]
pub trait Dns: Sync {
    /// `(exchange, preference)` of each MX record.
    async fn mx(&self, host: &str) -> Result<Vec<(String, u16)>, Lookup>;
    /// Each TXT record, its strings joined.
    async fn txt(&self, host: &str) -> Result<Vec<String>, Lookup>;
    /// A records.
    async fn a(&self, host: &str) -> Result<Vec<std::net::Ipv4Addr>, Lookup>;
    /// CNAME targets (failures read as none).
    async fn cname(&self, host: &str) -> Vec<String>;
}

/// What the API resolves with: the system resolver, or fixed answers
/// (tests).
#[derive(Clone)]
pub enum AppDns {
    System(std::sync::Arc<SystemDns>),
    Fixed(std::sync::Arc<FixedDns>),
}

impl AppDns {
    /// The system resolver; none (every name missing) with `DNS_LOOKUPS=off`.
    pub fn system() -> Self {
        if std::env::var("DNS_LOOKUPS").is_ok_and(|value| value.trim() == "off") {
            return Self::Fixed(std::sync::Arc::default());
        }
        SystemDns::new().map_or_else(
            || Self::Fixed(std::sync::Arc::default()),
            |dns| Self::System(std::sync::Arc::new(dns)),
        )
    }
}

impl Dns for AppDns {
    async fn mx(&self, host: &str) -> Result<Vec<(String, u16)>, Lookup> {
        match self {
            Self::System(dns) => dns.mx(host).await,
            Self::Fixed(dns) => dns.mx(host).await,
        }
    }
    async fn txt(&self, host: &str) -> Result<Vec<String>, Lookup> {
        match self {
            Self::System(dns) => dns.txt(host).await,
            Self::Fixed(dns) => dns.txt(host).await,
        }
    }
    async fn a(&self, host: &str) -> Result<Vec<std::net::Ipv4Addr>, Lookup> {
        match self {
            Self::System(dns) => dns.a(host).await,
            Self::Fixed(dns) => dns.a(host).await,
        }
    }
    async fn cname(&self, host: &str) -> Vec<String> {
        match self {
            Self::System(dns) => dns.cname(host).await,
            Self::Fixed(dns) => dns.cname(host).await,
        }
    }
}

/// Fixed answers by host; anything else does not exist.
#[derive(Debug, Default)]
pub struct FixedDns {
    pub mx: std::collections::HashMap<String, Vec<(String, u16)>>,
    pub txt: std::collections::HashMap<String, Vec<String>>,
    pub a: std::collections::HashMap<String, Vec<std::net::Ipv4Addr>>,
}

#[allow(clippy::unused_async_trait_impl)] // the trait is async
impl Dns for FixedDns {
    async fn mx(&self, host: &str) -> Result<Vec<(String, u16)>, Lookup> {
        self.mx.get(host).cloned().ok_or(Lookup::NotFound)
    }
    async fn txt(&self, host: &str) -> Result<Vec<String>, Lookup> {
        self.txt.get(host).cloned().ok_or(Lookup::NotFound)
    }
    async fn a(&self, host: &str) -> Result<Vec<std::net::Ipv4Addr>, Lookup> {
        self.a.get(host).cloned().ok_or(Lookup::NotFound)
    }
    async fn cname(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
}

/// The system resolver.
pub struct SystemDns(hickory_resolver::TokioResolver);

impl SystemDns {
    /// The system's resolvers, giving up on a name after two seconds.
    pub fn new() -> Option<Self> {
        let mut builder = hickory_resolver::Resolver::builder_tokio().ok()?;
        let options = builder.options_mut();
        options.timeout = std::time::Duration::from_secs(2);
        options.attempts = 1;
        Some(Self(builder.build()))
    }
}

fn lookup_error(error: &hickory_resolver::ResolveError) -> Lookup {
    if error.is_nx_domain() || error.is_no_records_found() {
        Lookup::NotFound
    } else {
        Lookup::Failed
    }
}

impl Dns for SystemDns {
    async fn mx(&self, host: &str) -> Result<Vec<(String, u16)>, Lookup> {
        let answer = self.0.mx_lookup(host).await.map_err(|e| lookup_error(&e))?;
        Ok(answer
            .iter()
            .map(|mx| {
                let exchange = mx.exchange().to_utf8();
                (exchange.trim_end_matches('.').to_string(), mx.preference())
            })
            .collect())
    }

    async fn txt(&self, host: &str) -> Result<Vec<String>, Lookup> {
        let answer = self
            .0
            .txt_lookup(host)
            .await
            .map_err(|e| lookup_error(&e))?;
        Ok(answer
            .iter()
            .map(|txt| {
                txt.txt_data()
                    .iter()
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<String>()
            })
            .collect())
    }

    async fn a(&self, host: &str) -> Result<Vec<std::net::Ipv4Addr>, Lookup> {
        let answer = self
            .0
            .ipv4_lookup(host)
            .await
            .map_err(|e| lookup_error(&e))?;
        Ok(answer.iter().map(|a| a.0).collect())
    }

    async fn cname(&self, host: &str) -> Vec<String> {
        self.0
            .lookup(host, hickory_resolver::proto::rr::RecordType::CNAME)
            .await
            .map(|answer| {
                answer
                    .iter()
                    .filter_map(|data| data.as_cname().map(|c| c.0.to_utf8()))
                    .map(|name| name.trim_end_matches('.').to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Whether a domain is on the checked blocklists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blacklist {
    Clean,
    Listed,
    Unavailable,
}

/// A DKIM selector that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DkimFound {
    pub selector: String,
}

/// How complete the DNS picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// The parts of a score.
#[derive(Debug, Clone)]
pub struct Findings {
    pub spf_has_ptr: bool,
    pub spf_has_malformed_include: bool,
}

/// A scored domain.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub domain: String,
    pub score: i64,
    /// The summary line, then numbered DNS changes (or "no changes needed").
    pub reasons: Vec<String>,
    pub confidence: Confidence,
    pub findings: Findings,
}

/// What the scoring reads.
pub struct DnsFacts<'a> {
    pub domain: &'a str,
    pub mx: &'a [(String, u16)],
    pub spf_records: &'a [String],
    pub dmarc_records: &'a [String],
    pub blacklist: Blacklist,
    pub dkim_found: &'a [DkimFound],
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static SPF: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^v=spf1\b"));
static DMARC: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^v=dmarc1\b"));
static DMARC_POLICY: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(?:^|;)\s*p\s*=\s*([a-z]+)"));
static SPF_ALL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s[+~?-]all\b"));
static SPF_LOOKUPS: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(?:include|a|mx|ptr|exists|redirect)(?::|\b)"));
static SPF_PTR: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bptr(?::|\b)"));
static SPF_INCLUDE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\binclude:(\S+)"));
static INCLUDE_HOST: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^[a-z0-9._-]+$"));
static PLUS_ALL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s\+all\b"));
static PLUS_ALL_END: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s\+all$"));
static QUESTION_ALL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s\?all\b"));
static SOFT_ALL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s~all\b"));
static DKIM_V: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)v=dkim1"));
static DKIM_P: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bp="));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

fn dmarc_policy(record: &str) -> Option<String> {
    DMARC_POLICY
        .captures(record)
        .map(|caps| caps[1].to_lowercase())
}

fn policy_rank(policy: Option<&str>) -> u8 {
    match policy {
        Some("reject") => 3,
        Some("quarantine") => 2,
        Some("none") => 1,
        _ => 0,
    }
}

struct Stacks {
    google: bool,
    microsoft: bool,
    ses: bool,
    godaddy: bool,
    label: String,
    spf_includes: Vec<&'static str>,
}

fn detect_stacks(mx: &[(String, u16)]) -> Stacks {
    let hosts = mx
        .iter()
        .map(|(exchange, _)| exchange.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let google = hosts.contains("google.com") || hosts.contains("googlemail.com");
    let microsoft = hosts.contains("protection.outlook.com");
    let ses = hosts.contains("amazonaws.com") || hosts.contains("amazonses.com");
    let godaddy = hosts.contains("secureserver.net");
    let mut labels = Vec::new();
    let mut spf_includes = Vec::new();
    for (on, label, include) in [
        (google, "Google Workspace", "include:_spf.google.com"),
        (
            microsoft,
            "Microsoft 365",
            "include:spf.protection.outlook.com",
        ),
        (ses, "Amazon SES inbound", "include:amazonses.com"),
        (godaddy, "GoDaddy email", "include:secureserver.net"),
    ] {
        if on {
            labels.push(label);
            spf_includes.push(include);
        }
    }
    let label = if !labels.is_empty() {
        labels.join(" + ")
    } else if mx.is_empty() {
        "no MX".to_string()
    } else {
        format!(
            "MX: {}",
            mx.iter()
                .map(|(exchange, _)| exchange.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if spf_includes.is_empty() {
        spf_includes.push("include:_spf.google.com");
    }
    Stacks {
        google,
        microsoft,
        ses,
        godaddy,
        label,
        spf_includes,
    }
}

fn is_non_sending_spf(spf: Option<&str>) -> bool {
    spf.is_some_and(|spf| SPACES.replace_all(spf, " ").trim().to_lowercase() == "v=spf1 -all")
}

fn replace_spf_all(spf: &str, next: &str) -> String {
    if SPF_ALL.is_match(spf) {
        SPF_ALL.replace(spf, format!(" {next}")).into_owned()
    } else {
        format!("{} {next}", spf.trim())
    }
}

fn malformed_includes(spf: &str) -> Vec<String> {
    SPF_INCLUDE
        .captures_iter(spf)
        .map(|caps| caps[1].to_string())
        .filter(|value| {
            value.is_empty()
                || value.contains("://")
                || !INCLUDE_HOST.is_match(value)
                || !value.contains('.')
        })
        .collect()
}

const GENERIC_SELECTORS: [&str; 23] = [
    "s1",
    "s2",
    "k1",
    "k2",
    "k3",
    "default",
    "dkim",
    "mail",
    "smtp",
    "hs1",
    "hs2",
    "cm",
    "sm",
    "pm",
    "brevo1",
    "brevo2",
    "pdk1",
    "pdk2",
    "zoho",
    "mandrill",
    "sig1",
    "everlytickey1",
    "everlytickey2",
];

fn selectors_to_probe(stacks: &Stacks) -> Vec<&'static str> {
    let mut ordered: Vec<&'static str> = Vec::new();
    let mut push = |items: &[&'static str]| {
        for item in items {
            if !ordered.contains(item) {
                ordered.push(item);
            }
        }
    };
    if stacks.google {
        push(&["google", "googlemail"]);
    }
    if stacks.microsoft {
        push(&["selector1", "selector2"]);
    }
    if stacks.ses {
        push(&["amazonses"]);
    }
    if stacks.godaddy {
        push(&["default", "mail"]);
    }
    push(&GENERIC_SELECTORS);
    ordered
}

/// Scores what DNS said. Port of `scoreEmailDeliverability`.
#[allow(clippy::too_many_lines)]
pub fn score(facts: &DnsFacts<'_>) -> Assessment {
    let DnsFacts {
        domain,
        mx,
        spf_records,
        dmarc_records,
        blacklist,
        dkim_found,
    } = *facts;
    let mut score: i64 = 0;
    let stacks = detect_stacks(mx);
    let has_mx = !mx.is_empty();
    let has_spf = !spf_records.is_empty();
    let spf = spf_records.first().map(String::as_str);
    let major_esp = stacks.google || stacks.microsoft || stacks.ses;

    let mut weakest: Option<String> = None;
    let mut weakest_set = false;
    let mut strongest: Option<String> = None;
    let mut strongest_record: Option<&str> = None;
    for record in dmarc_records {
        let policy = dmarc_policy(record);
        if policy_rank(policy.as_deref()) >= policy_rank(strongest.as_deref()) {
            strongest.clone_from(&policy);
            strongest_record = Some(record);
        }
        if !weakest_set || policy_rank(policy.as_deref()) < policy_rank(weakest.as_deref()) {
            weakest = policy;
            weakest_set = true;
        }
    }
    let has_dmarc = !dmarc_records.is_empty();
    let dmarc_policy = strongest.clone();

    let lookups = spf.map_or(0, |spf| SPF_LOOKUPS.find_iter(spf).count());
    let has_ptr = spf.is_some_and(|spf| SPF_PTR.is_match(spf));
    let malformed = spf.map(malformed_includes).unwrap_or_default();
    let selectors: Vec<&str> = dkim_found.iter().map(|d| d.selector.as_str()).collect();
    let has = |selector: &str| selectors.contains(&selector);
    let has_primary_dkim = (stacks.google && has("google"))
        || (stacks.microsoft && (has("selector1") || has("selector2")))
        || (!stacks.google && !stacks.microsoft && !selectors.is_empty());
    let has_any_dkim = !selectors.is_empty();

    let unknown = |p: &Option<String>| p.clone().unwrap_or_else(|| "unknown".into());
    let summary = [
        format!("Inbox risk check v5 for {domain}:"),
        if has_mx {
            format!("MX OK ({})", stacks.label)
        } else {
            "MX missing".into()
        },
        if has_spf {
            if spf_records.len() > 1 {
                format!("SPF = {} records", spf_records.len())
            } else {
                format!("SPF = \"{}\"", spf.unwrap_or_default())
            }
        } else {
            "SPF missing".into()
        },
        if has_dmarc {
            if dmarc_records.len() > 1 {
                format!(
                    "DMARC has {} conflicting records (strongest p={}, weakest p={})",
                    dmarc_records.len(),
                    unknown(&strongest),
                    unknown(&weakest)
                )
            } else {
                format!(
                    "DMARC p={} (\"{}\")",
                    unknown(&dmarc_policy),
                    strongest_record.unwrap_or_default()
                )
            }
        } else {
            "DMARC missing".into()
        },
        if has_any_dkim {
            format!("DKIM found ({})", selectors.join(", "))
        } else {
            "DKIM not found on common selectors".into()
        },
        match blacklist {
            Blacklist::Listed => "on a domain blocklist",
            Blacklist::Unavailable => "blocklist check unavailable",
            Blacklist::Clean => "not on checked blocklists",
        }
        .into(),
    ]
    .join(" · ");

    let mut improvements: Vec<String> = Vec::new();
    let mut add = |points: i64, text: String| {
        score += points;
        improvements.push(format!("{}. {text}", improvements.len() + 1));
    };

    if !has_mx {
        add(
            18,
            format!("{domain}: add MX records for your mailbox provider."),
        );
    }
    if !has_spf {
        let suggested = format!("v=spf1 {} -all", stacks.spf_includes.join(" "));
        add(
            if major_esp { 14 } else { 20 },
            format!(
                "{domain}: publish ONE SPF TXT on the root domain. Suggested for detected stack ({}): {suggested}",
                stacks.label
            ),
        );
    } else if spf_records.len() > 1 {
        add(
            12,
            format!(
                "{domain}: remove duplicate SPF TXT records. Found: {}.",
                spf_records
                    .iter()
                    .map(|r| format!("\"{r}\""))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
        );
    } else {
        let spf = spf.unwrap_or_default();
        if PLUS_ALL.is_match(spf) || PLUS_ALL_END.is_match(spf) {
            add(
                35,
                format!(
                    "{domain}: replace dangerous SPF +all. Current: \"{spf}\". Target: \"{}\".",
                    replace_spf_all(spf, "-all")
                ),
            );
        } else if QUESTION_ALL.is_match(spf) {
            add(
                8,
                format!("{domain}: tighten SPF ?all → -all. Current: \"{spf}\"."),
            );
        } else if SOFT_ALL.is_match(spf) && matches!(dmarc_policy.as_deref(), None | Some("none")) {
            add(
                2,
                format!(
                    "{domain}: with weak/no DMARC, prefer SPF -all. Current: \"{spf}\". Target: \"{}\".",
                    replace_spf_all(spf, "-all")
                ),
            );
        }
        if has_ptr {
            add(
                10,
                format!("{domain}: remove SPF \"ptr\" (deprecated and slow). Current: \"{spf}\"."),
            );
        }
        if !malformed.is_empty() {
            add(
                16,
                format!(
                    "{domain}: fix malformed SPF include(s): {}. Current: \"{spf}\".",
                    malformed
                        .iter()
                        .map(|v| format!("\"{v}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if lookups > 10 {
            add(
                18,
                format!(
                    "{domain}: SPF likely exceeds the 10 DNS-lookup limit (estimate {lookups}). Receivers treat that as SPF PermError/fail — flatten or reduce includes."
                ),
            );
        }
    }

    if dmarc_records.len() > 1 {
        add(
            8,
            format!(
                "{domain}: keep a single DMARC on _dmarc.{domain}. Currently: {}. Keep strongest (p={}).",
                dmarc_records
                    .iter()
                    .map(|r| format!("\"{r}\""))
                    .collect::<Vec<_>>()
                    .join(" AND "),
                strongest.clone().unwrap_or_else(|| "null".into())
            ),
        );
    }
    if !has_dmarc {
        add(
            14,
            format!(
                "{domain}: add DMARC at _dmarc.{domain}: v=DMARC1; p=none; rua=mailto:dmarc@{domain}; fo=1 — then raise to p=quarantine / p=reject."
            ),
        );
    } else if dmarc_policy.as_deref() == Some("none") {
        add(
            6,
            format!(
                "{domain}: DMARC p=none is monitoring only (meets bare Gmail/Yahoo minimum, weak enforcement). Current: \"{}\". Move to p=quarantine then p=reject.",
                strongest_record.unwrap_or_default()
            ),
        );
    }

    let non_sending = is_non_sending_spf(spf);
    if !non_sending {
        if stacks.google && !has("google") {
            add(
                14,
                format!(
                    "{domain}: enable Google Workspace DKIM and publish TXT at google._domainkey.{domain} (Admin → Apps → Gmail → Authenticate email). No google DKIM selector was found in DNS."
                ),
            );
        } else if stacks.microsoft && !has("selector1") && !has("selector2") {
            add(
                14,
                format!(
                    "{domain}: enable Microsoft 365 DKIM (selector1/selector2 CNAMEs). No Microsoft DKIM selectors were found in DNS."
                ),
            );
        } else if !has_any_dkim && has_mx {
            add(
                10,
                format!(
                    "{domain}: no common DKIM selectors found in DNS. Enable DKIM in your email provider and publish the DNS records they provide."
                ),
            );
        }
    }

    if blacklist == Blacklist::Listed {
        add(
            45,
            format!(
                "{domain}: listed on Spamhaus DBL / similar. Delist at https://www.spamhaus.org/dbl/"
            ),
        );
    }

    let confidence = {
        let mut confidence = if blacklist == Blacklist::Listed
            || (non_sending && has_spf && has_dmarc && dmarc_policy.as_deref() == Some("reject"))
            || (has_spf
                && has_dmarc
                && matches!(dmarc_policy.as_deref(), Some("quarantine" | "reject"))
                && has_primary_dkim
                && malformed.is_empty()
                && !has_ptr
                && lookups <= 10)
        {
            Confidence::High
        } else {
            // The CRM's remaining branches (and its `low`) all end up medium.
            Confidence::Medium
        };
        if !has_any_dkim && major_esp && !non_sending && confidence == Confidence::High {
            confidence = Confidence::Medium;
        }
        confidence
    };

    let mut reasons = vec![summary];
    if improvements.is_empty() {
        reasons.push(format!(
            "No DNS changes needed to improve this score for {domain}. MX/SPF/DMARC/DKIM look correctly set for auth readiness."
        ));
    } else {
        reasons.extend(improvements);
    }
    Assessment {
        domain: domain.to_string(),
        score: score.clamp(0, 100),
        reasons,
        confidence,
        findings: Findings {
            spf_has_ptr: has_ptr,
            spf_has_malformed_include: !malformed.is_empty(),
        },
    }
}

async fn safe_txt(dns: &impl Dns, host: &str) -> Result<Vec<String>, Lookup> {
    match dns.txt(host).await {
        Ok(records) => Ok(records
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect()),
        Err(Lookup::NotFound) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

async fn dnsbl(dns: &impl Dns, domain: &str, zone: &str) -> Blacklist {
    match dns.a(&format!("{domain}.{zone}")).await {
        Ok(ips) => {
            let ips: Vec<String> = ips.iter().map(ToString::to_string).collect();
            if ips.iter().any(|ip| ip.starts_with("127.255.255.")) {
                Blacklist::Unavailable
            } else if ips.iter().any(|ip| ip.starts_with("127.0.0.")) {
                Blacklist::Listed
            } else {
                Blacklist::Clean
            }
        }
        Err(Lookup::NotFound) => Blacklist::Clean,
        Err(Lookup::Failed) => Blacklist::Unavailable,
    }
}

/// Probes DNS and scores the domain. Errors when MX or TXT lookups fail or
/// the whole check takes over ten seconds.
pub async fn assess(dns: &impl Dns, domain: &str) -> Result<Assessment, Lookup> {
    tokio::time::timeout(std::time::Duration::from_secs(10), assess_now(dns, domain))
        .await
        .unwrap_or(Err(Lookup::Failed))
}

async fn assess_now(dns: &impl Dns, domain: &str) -> Result<Assessment, Lookup> {
    let domain = domain.trim().to_lowercase();
    let dmarc_host = format!("_dmarc.{domain}");
    let (mx, root, dmarc, spamhaus, nordspam) = tokio::join!(
        dns.mx(&domain),
        safe_txt(dns, &domain),
        safe_txt(dns, &dmarc_host),
        dnsbl(dns, &domain, "dbl.spamhaus.org"),
        dnsbl(dns, &domain, "dbl.nordspam.com"),
    );
    let mx = match mx {
        Ok(mx) => mx,
        Err(Lookup::NotFound) => Vec::new(),
        Err(error) => return Err(error),
    };
    let (root, dmarc) = (root?, dmarc?);
    let blacklist = if spamhaus == Blacklist::Listed || nordspam == Blacklist::Listed {
        Blacklist::Listed
    } else if spamhaus == Blacklist::Unavailable && nordspam == Blacklist::Unavailable {
        Blacklist::Unavailable
    } else {
        Blacklist::Clean
    };
    let stacks = detect_stacks(&mx);
    let probes = selectors_to_probe(&stacks).into_iter().map(|selector| {
        let domain = domain.clone();
        async move {
            let host = format!("{selector}._domainkey.{domain}");
            let txt = safe_txt(dns, &host).await.unwrap_or_default();
            let present = txt.iter().any(|r| DKIM_V.is_match(r) || DKIM_P.is_match(r))
                || !dns.cname(&host).await.is_empty();
            present.then(|| DkimFound {
                selector: selector.to_string(),
            })
        }
    });
    let dkim_found: Vec<DkimFound> = futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect();
    let spf_records: Vec<String> = root.into_iter().filter(|r| SPF.is_match(r)).collect();
    let dmarc_records: Vec<String> = dmarc.into_iter().filter(|r| DMARC.is_match(r)).collect();
    Ok(score(&DnsFacts {
        domain: &domain,
        mx: &mx,
        spf_records: &spf_records,
        dmarc_records: &dmarc_records,
        blacklist,
        dkim_found: &dkim_found,
    }))
}

/// The provider's answer: the summary, a confidence line, then the changes.
pub async fn lookup(dns: &impl Dns, domain: &str) -> Result<(String, i64, Vec<String>), Lookup> {
    let assessment = assess(dns, domain).await?;
    let mut reasons = Vec::with_capacity(assessment.reasons.len() + 1);
    let mut rest = assessment.reasons.into_iter();
    if let Some(summary) = rest.next() {
        reasons.push(summary);
    }
    reasons.push(format!(
        "Confidence: {} (DNS auth readiness — not a guarantee of Gmail/Outlook inbox placement).",
        assessment.confidence.as_str()
    ));
    reasons.extend(rest);
    Ok((assessment.domain, assessment.score, reasons))
}

// ---- The company's trust record

const FREE_EMAIL_DOMAINS: [&str; 21] = [
    "gmail.com",
    "googlemail.com",
    "yahoo.com",
    "yahoo.co.uk",
    "ymail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "mail.com",
    "proton.me",
    "protonmail.com",
    "zoho.com",
    "gmx.com",
    "gmx.net",
    "fastmail.com",
    "hey.com",
];

static DOMAIN: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$"));

/// The domain of a company domain field or email address.
pub fn company_domain(input: Option<&str>) -> Option<String> {
    let mut value = input?.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    if let Some((_, host)) = value.rsplit_once('@') {
        value = host.to_string();
    }
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(&value);
    let value = value.split(['/', '?', '#']).next().unwrap_or_default();
    let value = match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => value,
    };
    let value = value.strip_prefix("www.").unwrap_or(value);
    DOMAIN.is_match(value).then(|| value.to_string())
}

/// 0-20 good, 21-50 review, above high risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskLevel {
    Good,
    Review,
    HighRisk,
    Unknown,
}

pub const fn risk_level(score: Option<i64>) -> RiskLevel {
    match score {
        None => RiskLevel::Unknown,
        Some(score) if score <= 20 => RiskLevel::Good,
        Some(score) if score <= 50 => RiskLevel::Review,
        Some(_) => RiskLevel::HighRisk,
    }
}

/// A company domain's deliverability.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DomainTrust {
    /// The domain checked.
    pub domain: String,
    /// 0-100, higher = more spam risk; null when the check failed.
    pub reputation_score: Option<i64>,
    /// `GOOD`, `REVIEW`, `HIGH_RISK` or `UNKNOWN`.
    pub risk_level: RiskLevel,
    /// A summary line, a confidence line, then the DNS changes to make.
    pub reputation_reasons: Vec<String>,
    /// When it was checked (ISO 8601).
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct TrustRow {
    domain: String,
    reputation_score: Option<i64>,
    risk_level: String,
    reputation_reasons: Option<String>,
    checked_at: chrono::NaiveDateTime,
}

fn from_row(row: TrustRow) -> DomainTrust {
    DomainTrust {
        domain: row.domain,
        reputation_score: row.reputation_score,
        risk_level: match row.risk_level.as_str() {
            "GOOD" => RiskLevel::Good,
            "REVIEW" => RiskLevel::Review,
            "HIGH_RISK" => RiskLevel::HighRisk,
            _ => RiskLevel::Unknown,
        },
        reputation_reasons: row
            .reputation_reasons
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        checked_at: row.checked_at.and_utc(),
    }
}

async fn save(pool: &MySqlPool, company_id: i32, trust: &DomainTrust) {
    let saved = sqlx::query(
        "INSERT INTO company_domain_trust
           (company_id, domain, reputation_score, risk_level, reputation_reasons, checked_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE domain = VALUES(domain), reputation_score = VALUES(reputation_score),
           risk_level = VALUES(risk_level), reputation_reasons = VALUES(reputation_reasons),
           checked_at = VALUES(checked_at)",
    )
    .bind(company_id)
    .bind(&trust.domain)
    .bind(trust.reputation_score)
    .bind(match trust.risk_level {
        RiskLevel::Good => "GOOD",
        RiskLevel::Review => "REVIEW",
        RiskLevel::HighRisk => "HIGH_RISK",
        RiskLevel::Unknown => "UNKNOWN",
    })
    .bind(serde_json::to_string(&trust.reputation_reasons).unwrap_or_else(|_| "[]".into()))
    .bind(trust.checked_at.naive_utc())
    .execute(pool)
    .await;
    if let Err(error) = saved {
        lambda_http::tracing::warn!(?error, company_id, "could not save the domain trust");
    }
}

/// The company domain's deliverability, checked or reused.
///
/// A week-old check of the same domain is reused, free mail providers score high risk without asking DNS, and a
/// failed check is saved as `UNKNOWN`. `None` without a usable domain.
pub async fn ensure_company_trust(
    pool: &MySqlPool,
    dns: &impl Dns,
    company_id: i32,
    domain_or_email: Option<&str>,
) -> Result<Option<DomainTrust>, sqlx::Error> {
    let Some(domain) = company_domain(domain_or_email) else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    let trust = if FREE_EMAIL_DOMAINS.contains(&domain.as_str()) {
        DomainTrust {
            domain,
            reputation_score: Some(85),
            risk_level: RiskLevel::HighRisk,
            reputation_reasons: vec![
                "Replace free-mail domains (Gmail, Yahoo, Outlook, etc.) with your company domain. Shared free providers cannot build dedicated sender reputation for your brand and raise spam risk.".into(),
                "Create company email at Google Workspace, Microsoft 365, or similar, then set SPF, DKIM, and DMARC on that company domain.".into(),
            ],
            checked_at: now,
        }
    } else {
        let cached: Option<TrustRow> = sqlx::query_as(
            "SELECT domain, CAST(reputation_score AS SIGNED) AS reputation_score, risk_level,
                    CAST(reputation_reasons AS CHAR) AS reputation_reasons, checked_at
               FROM company_domain_trust
              WHERE domain = ? AND checked_at >= (UTC_TIMESTAMP() - INTERVAL 7 DAY)
              ORDER BY checked_at DESC LIMIT 1",
        )
        .bind(&domain)
        .fetch_optional(pool)
        .await?;
        match cached.map(from_row).filter(|trust| {
            trust
                .reputation_reasons
                .first()
                .is_some_and(|line| line.starts_with("Inbox risk check v5 for "))
        }) {
            Some(cached) => cached,
            None => match lookup(dns, &domain).await {
                Ok((checked, score, reasons)) => DomainTrust {
                    domain: if checked.is_empty() { domain } else { checked },
                    reputation_score: Some(score),
                    risk_level: risk_level(Some(score)),
                    reputation_reasons: reasons,
                    checked_at: now,
                },
                Err(_) => DomainTrust {
                    domain,
                    reputation_score: None,
                    risk_level: RiskLevel::Unknown,
                    reputation_reasons: vec![
                        "Deliverability check temporarily unavailable. Try again later, and manually verify SPF, DKIM, DMARC, MX, and Spamhaus DBL for this domain.".into(),
                    ],
                    checked_at: now,
                },
            },
        }
    };
    save(pool, company_id, &trust).await;
    Ok(Some(trust))
}

/// The company's saved trust record.
pub async fn company_trust(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<DomainTrust>, sqlx::Error> {
    let row: Option<TrustRow> = sqlx::query_as(
        "SELECT domain, CAST(reputation_score AS SIGNED) AS reputation_score, risk_level,
                CAST(reputation_reasons AS CHAR) AS reputation_reasons, checked_at
           FROM company_domain_trust WHERE company_id = ? LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(from_row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn domains_come_from_emails_and_urls() {
        assert_eq!(
            company_domain(Some("john@abcgranite.com")).as_deref(),
            Some("abcgranite.com")
        );
        assert_eq!(
            company_domain(Some("https://www.abcgranite.com/path")).as_deref(),
            Some("abcgranite.com")
        );
        assert_eq!(
            company_domain(Some("ABCGRANITE.COM")).as_deref(),
            Some("abcgranite.com")
        );
        assert_eq!(company_domain(Some("not-a-domain")), None);
        assert_eq!(
            company_domain(Some("acme.com:8080")).as_deref(),
            Some("acme.com")
        );
        assert_eq!(company_domain(None), None);
    }

    #[test]
    fn scores_map_to_risk_levels() {
        assert_eq!(risk_level(Some(0)), RiskLevel::Good);
        assert_eq!(risk_level(Some(20)), RiskLevel::Good);
        assert_eq!(risk_level(Some(21)), RiskLevel::Review);
        assert_eq!(risk_level(Some(50)), RiskLevel::Review);
        assert_eq!(risk_level(Some(51)), RiskLevel::HighRisk);
        assert_eq!(risk_level(None), RiskLevel::Unknown);
    }

    #[test]
    fn a_healthy_google_domain_scores_zero_with_high_confidence() {
        let result = score(&DnsFacts {
            domain: "healthy.example",
            mx: &[("aspmx.l.google.com".into(), 1)],
            spf_records: &strings(&["v=spf1 include:_spf.google.com -all"]),
            dmarc_records: &strings(&["v=DMARC1; p=reject; rua=mailto:dmarc@healthy.example"]),
            blacklist: Blacklist::Clean,
            dkim_found: &[DkimFound {
                selector: "google".into(),
            }],
        });
        assert_eq!(result.score, 0);
        assert_eq!(result.confidence, Confidence::High);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.starts_with("No DNS changes needed"))
        );
    }

    #[test]
    fn missing_spf_dmarc_and_mx_is_high_risk() {
        let result = score(&DnsFacts {
            domain: "broken.example",
            mx: &[],
            spf_records: &[],
            dmarc_records: &[],
            blacklist: Blacklist::Clean,
            dkim_found: &[],
        });
        assert!(result.score >= 51);
    }

    #[test]
    fn google_without_spf_with_two_dmarcs_and_no_dkim_is_review() {
        let result = score(&DnsFacts {
            domain: "granitedepotindy.com",
            mx: &[("aspmx.l.google.com".into(), 1)],
            spf_records: &[],
            dmarc_records: &strings(&["v=DMARC1; p=none", "v=DMARC1; p=quarantine; pct=100;"]),
            blacklist: Blacklist::Clean,
            dkim_found: &[],
        });
        // 14 missing SPF + 8 duplicate DMARC + 14 missing google DKIM.
        assert_eq!(result.score, 36);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.contains("google._domainkey"))
        );
        assert!(result.reasons[0].contains("strongest p=quarantine, weakest p=none"));
    }

    #[test]
    fn malformed_includes_and_ptr_are_penalized() {
        let result = score(&DnsFacts {
            domain: "gmq.example",
            mx: &[("aspmx.l.google.com".into(), 1)],
            spf_records: &strings(&[
                "v=spf1 a mx ptr include:secureserver.net include:dc-aa8e722993._://bad.example ~all",
            ]),
            dmarc_records: &strings(&["v=DMARC1; p=none"]),
            blacklist: Blacklist::Clean,
            dkim_found: &[DkimFound {
                selector: "k2".into(),
            }],
        });
        assert!(result.findings.spf_has_ptr);
        assert!(result.findings.spf_has_malformed_include);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.contains("malformed SPF include"))
        );
        assert!(result.reasons.iter().any(|r| r.contains("ptr")));
    }

    struct Stub(HashMap<String, Vec<String>>);

    #[allow(clippy::unused_async_trait_impl)]
    impl Dns for Stub {
        async fn mx(&self, _: &str) -> Result<Vec<(String, u16)>, Lookup> {
            Ok(vec![("aspmx.l.google.com".into(), 10)])
        }
        async fn txt(&self, host: &str) -> Result<Vec<String>, Lookup> {
            Ok(self.0.get(host).cloned().unwrap_or_default())
        }
        async fn a(&self, _: &str) -> Result<Vec<std::net::Ipv4Addr>, Lookup> {
            Err(Lookup::NotFound)
        }
        async fn cname(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn the_provider_probes_dns_and_adds_the_confidence_line() {
        let dns = Stub(HashMap::from([
            (
                "example.com".to_string(),
                strings(&["v=spf1 include:_spf.google.com -all"]),
            ),
            (
                "_dmarc.example.com".to_string(),
                strings(&["v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com"]),
            ),
            (
                "google._domainkey.example.com".to_string(),
                strings(&["v=DKIM1; k=rsa; p=abc"]),
            ),
        ]));
        let (domain, score, reasons) = lookup(&dns, "example.com").await.unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(score, 0);
        assert!(reasons[1].starts_with("Confidence: high"));
        assert!(
            reasons
                .iter()
                .any(|r| r.starts_with("No DNS changes needed"))
        );
    }
}

use crate::billing::catalog::{PlanChoice, Quote};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult};
use crate::routes::users::manage::checked_email;
use crate::signup::consent::{ConsentAcceptance, RenderedConsent};
use crate::signup::intent::{CompanyDetails, SignupStatus};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query of `GET /v1/public/plans`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlansQuery {
    /// Estimated active users, 1 to 500 (default 5; out-of-range values are
    /// clamped, unreadable ones fall back to the default).
    pub users: Option<String>,
    /// Chosen add-ons (repeat the parameter): `signatures`, `quotes` or
    /// `quickbooks`. Unknown values are ignored.
    #[serde(default)]
    pub addon: Vec<String>,
}

/// The base plan.
#[derive(Debug, Serialize, ToSchema)]
pub struct BasePlan {
    /// Always `Base plan`.
    pub name: String,
    /// Monthly price in cents, before sales tax.
    pub monthly_cents: i64,
    /// Active users the base price covers.
    pub included_users: i64,
}

/// A paid add-on of the catalog.
#[derive(Debug, Serialize, ToSchema)]
pub struct AddonInfo {
    pub key: AddonKey,
    /// Name people see, e.g. `QuickBooks`.
    pub name: String,
    /// One line: what it unlocks.
    pub description: String,
    /// Monthly price in cents, before sales tax.
    pub monthly_cents: i64,
}

/// The catalog and a quote for one choice.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlansResponse {
    pub base: BasePlan,
    /// Monthly price in cents of each active user above the included ones.
    pub extra_user_cents: i64,
    /// Months without a charge after signup.
    pub free_months: u32,
    /// Smallest user estimate.
    pub min_users: i64,
    /// Largest user estimate.
    pub max_users: i64,
    /// Every add-on.
    pub addons: Vec<AddonInfo>,
    /// The price of the requested choice.
    pub quote: Quote,
}

/// The owner of the new company, who signs in with these.
#[derive(Deserialize, ToSchema)]
pub struct OwnerDetails {
    /// Full name, up to 100 characters.
    pub name: String,
    /// Email, up to 254 characters. Receives the code; used to sign in.
    pub email: String,
    /// Password, 10 to 200 characters (surrounding spaces are dropped).
    pub password: String,
}

/// Body of `POST /v1/public/signups`.
#[derive(Deserialize, ToSchema)]
pub struct CreateSignupBody {
    /// What the buyer picked in step 1.
    pub plan: PlanChoice,
    pub company: CompanyDetails,
    pub owner: OwnerDetails,
}

/// Answer of `POST /v1/public/signups`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedSignup {
    /// The signup id. Keep it private: it is the only secret of the flow.
    pub id: String,
    /// Seconds until another code can be requested.
    pub resend_in_seconds: i64,
}

/// Answer of `POST /v1/public/signups/{id}/code`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResentCode {
    /// Seconds until another code can be requested.
    pub resend_in_seconds: i64,
}

/// Body of `POST /v1/public/signups/{id}/verification`.
#[derive(Deserialize, ToSchema)]
pub struct VerifyBody {
    /// The 6-digit code from the email.
    pub code: String,
}

/// Where the card or bank account is collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMode {
    /// The api attaches a test payment method itself (sandbox).
    Test,
    /// The Payment Element collects one.
    Stripe,
}

/// A signup in progress.
#[derive(Debug, Serialize, ToSchema)]
pub struct Signup {
    pub id: String,
    pub status: SignupStatus,
    /// The email with its local part hidden, e.g. `a***@acme.com`.
    pub email_masked: String,
    pub company_name: String,
    pub plan: PlanChoice,
    /// The price of `plan` today.
    pub quote: Quote,
    pub payment_mode: PaymentMode,
    /// Seconds until another code can be requested (0 = now).
    pub resend_in_seconds: i64,
}

/// Answer of `GET /v1/public/signups/{id}/consents`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConsentsResponse {
    /// The checkboxes, in the order to show them.
    pub consents: Vec<RenderedConsent>,
}

/// Answer of `POST /v1/public/signups/{id}/payment-setup`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PaymentSetup {
    /// A test payment method was attached; nothing for the page to do.
    Test,
    /// Confirm the SetupIntent in the browser with the Payment Element.
    Stripe {
        /// The SetupIntent's client secret.
        client_secret: String,
        /// The Stripe publishable key.
        publishable_key: String,
    },
}

/// Body of `POST /v1/public/signups/{id}/completion`.
#[derive(Deserialize, ToSchema)]
pub struct CompletionBody {
    /// Every checkbox the buyer ticked, as `GET .../consents` showed it.
    pub consents: Vec<ConsentAcceptance>,
}

/// Answer of `POST /v1/public/signups/{id}/completion`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Completion {
    /// Where the browser goes next.
    pub redirect_url: String,
}

/// A request that passed validation.
pub struct ValidCreate {
    pub plan: PlanChoice,
    pub company: CompanyDetails,
    pub owner_name: String,
    pub email: String,
    pub password: String,
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn required(value: &str, label: &str, max: usize) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(bad(&format!("Enter the {label}.")));
    }
    if value.chars().count() > max {
        return Err(bad(&format!("The {label} can be up to {max} characters.")));
    }
    if value.chars().any(char::is_control) {
        return Err(bad(&format!(
            "The {label} has a character we cannot accept."
        )));
    }
    Ok(value.to_string())
}

fn valid_zip(zip: &str) -> bool {
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    match zip.split_once('-') {
        Some((five, four)) => digits(five, 5) && digits(four, 4),
        None => digits(zip, 5),
    }
}

fn valid_phone(phone: &str) -> bool {
    phone.strip_prefix('+').is_some_and(|digits| {
        (8..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

impl CreateSignupBody {
    /// Checks and normalizes every field before anything is written.
    pub fn validate(self) -> ApiResult<ValidCreate> {
        let company = self.company;
        let name = required(&company.name, "company name", 100)?;
        let street = required(&company.street, "street address", 150)?;
        let city = required(&company.city, "city", 80)?;
        let state = company.state.trim().to_ascii_uppercase();
        if state.len() != 2 || !state.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(bad("Enter a two-letter state, like IN."));
        }
        let zip = company.zip.trim().to_string();
        if !valid_zip(&zip) {
            return Err(bad("Enter a ZIP code like 46204 or 46204-1234."));
        }
        let phone = company.phone.trim().to_string();
        if !valid_phone(&phone) {
            return Err(bad(
                "Enter a phone number with the country code, like +13175550100.",
            ));
        }
        let website = match company.website.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(site) if site.chars().count() > 200 || site.contains(char::is_whitespace) => {
                return Err(bad("Enter a valid website or leave it empty."));
            }
            Some(site) => Some(site.to_string()),
        };
        let owner_name = required(&self.owner.name, "owner name", 100)?;
        let email = checked_email(&self.owner.email)?;
        if email.chars().count() > 254 {
            return Err(bad("The email can be up to 254 characters."));
        }
        let password = self.owner.password.trim().to_string();
        let length = password.chars().count();
        if length < 10 {
            return Err(bad("Password must be at least 10 characters long."));
        }
        if length > 200 {
            return Err(bad("Password can be up to 200 characters."));
        }
        if password.contains('\0') {
            return Err(bad("Password has a character we cannot accept."));
        }
        Ok(ValidCreate {
            plan: PlanChoice::normalized(self.plan.users, &self.plan.addons),
            company: CompanyDetails {
                name,
                phone,
                website,
                street,
                city,
                state,
                zip,
            },
            owner_name,
            email,
            password,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(edit: impl FnOnce(&mut CreateSignupBody)) -> CreateSignupBody {
        let mut body = CreateSignupBody {
            plan: PlanChoice {
                users: 5,
                addons: Vec::new(),
            },
            company: CompanyDetails {
                name: "Acme Stone".into(),
                phone: "+13175550100".into(),
                website: None,
                street: "1 Main St".into(),
                city: "Indianapolis".into(),
                state: "in".into(),
                zip: "46204".into(),
            },
            owner: OwnerDetails {
                name: "Ann Owner".into(),
                email: " Ann@Acme.test ".into(),
                password: "correct horse".into(),
            },
        };
        edit(&mut body);
        body
    }

    fn message(result: ApiResult<ValidCreate>) -> String {
        match result {
            Err(ApiError::BadRequest(message)) => message,
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("expected a 400"),
        }
    }

    #[test]
    fn a_good_request_is_normalized() {
        let valid = body(|_| {}).validate().ok().unwrap();
        assert_eq!(valid.email, "ann@acme.test");
        assert_eq!(valid.company.state, "IN");
    }

    #[test]
    fn bad_fields_are_named() {
        assert_eq!(
            message(body(|b| b.owner.password = "123456789".into()).validate()),
            "Password must be at least 10 characters long."
        );
        assert!(message(body(|b| b.company.zip = "4620".into()).validate()).contains("ZIP"));
        assert!(message(body(|b| b.company.phone = "317".into()).validate()).contains("phone"));
        assert!(message(body(|b| b.company.state = "IND".into()).validate()).contains("state"));
        assert!(
            message(body(|b| b.company.name = "  ".into()).validate()).contains("company name")
        );
        assert!(message(body(|b| b.owner.email = "nope".into()).validate()).contains("email"));
        assert!(
            body(|b| b.company.zip = "46204-1234".into())
                .validate()
                .is_ok()
        );
    }
}

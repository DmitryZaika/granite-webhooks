use crate::crud::ringcentral::CustomerWithMapping;
use crate::ringcentral::api::PublicRcContactPayload;
pub use common::telephony::ringcentral_contacts::{
    contact_email, contact_notes, normalize_to_e164, split_name,
};

pub fn build_rc_contact_payload(
    customer: &CustomerWithMapping,
    first_name: String,
    last_name: String,
) -> PublicRcContactPayload {
    let phones: Vec<String> = [&customer.phone, &customer.phone_2]
        .iter()
        .filter_map(|p| normalize_to_e164(p.as_deref()))
        .collect();
    PublicRcContactPayload {
        first_name,
        last_name,
        email: contact_email(customer.email.as_deref()),
        mobile_phone: phones.first().cloned(),
        home_phone: phones.get(1).cloned(),
        business_phone: None,
        notes: contact_notes(customer.id),
    }
}

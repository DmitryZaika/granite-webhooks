//! Whose automated emails and texts go out for a sales rep's deals when a
//! deal lands on a list (Employee CRM › Automations).
//!
//! Each user picks in `users.lead_automation`:
//! - `company` (the default): the company's automated templates
//!   (Admin › Automations), those naming no sales reps or naming this one;
//! - `own`: only the user's own automated templates.

/// SQL condition on a drip template aliased `t` (`email_templates` or
/// `cloudtalk_sms_templates`): it goes out for deals of the user bound to
/// each of its three `?`, in order.
pub const DRIP_TEMPLATE_FOR_REP: &str = "IF((SELECT u.lead_automation FROM users u WHERE u.id = ?) <=> 'own', \
       t.user_id <=> ?, \
       t.user_id IS NULL AND (t.sales_rep_ids IS NULL OR JSON_LENGTH(t.sales_rep_ids) = 0 \
            OR JSON_CONTAINS(t.sales_rep_ids, CAST(? AS JSON))))";

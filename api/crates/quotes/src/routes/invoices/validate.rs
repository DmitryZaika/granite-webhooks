//! An invoice from the editor, checked and with its totals worked out.

use super::schemas::{InvoiceInput, InvoiceLine};
use chrono::NaiveDate;

/// Most lines one invoice takes.
pub const MAX_LINES: usize = 200;

/// An invoice ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanInvoice {
    pub invoice_number: Option<i32>,
    pub customer_id: Option<i32>,
    pub quote_id: Option<i32>,
    pub invoice_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub bill_to_name: String,
    pub bill_to_email: Option<String>,
    pub project_address: Option<String>,
    pub lines: Vec<InvoiceLine>,
    pub subtotal: f64,
    pub tax: f64,
    pub total: f64,
    pub customer_memo: Option<String>,
    pub private_note: Option<String>,
}

/// Dollars rounded to cents.
pub fn cents(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn date(value: &str, what: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| format!("{what} must be a date (YYYY-MM-DD)"))
}

/// A finite number, or an error naming the field.
fn finite(value: Option<f64>, what: &str) -> Result<Option<f64>, String> {
    match value {
        Some(value) if !value.is_finite() => Err(format!("{what} must be a number")),
        other => Ok(other),
    }
}

fn line(line: &InvoiceLine, at: usize) -> Result<Option<InvoiceLine>, String> {
    let description = line.description.trim().to_string();
    let unit_price = finite(line.unit_price, &format!("Line {at}: price"))?;
    let Some(unit_price) = unit_price else {
        // A text line; a blank one is dropped.
        return Ok((!description.is_empty()).then_some(InvoiceLine {
            description,
            qty: None,
            unit_price: None,
            amount: None,
        }));
    };
    let qty = finite(line.qty, &format!("Line {at}: quantity"))?.unwrap_or(1.0);
    if description.is_empty() {
        return Err(format!("Line {at} needs a description"));
    }
    Ok(Some(InvoiceLine {
        description,
        qty: Some(qty),
        unit_price: Some(unit_price),
        amount: Some(cents(qty * unit_price)),
    }))
}

/// Checks `input`; `valid_email` says whether an address can be sent to.
pub fn clean(
    input: &InvoiceInput,
    valid_email: impl Fn(&str) -> bool,
) -> Result<CleanInvoice, String> {
    let bill_to_name = input.bill_to_name.trim().to_string();
    if bill_to_name.is_empty() {
        return Err("Bill to needs a name".into());
    }
    if bill_to_name.chars().count() > 255 {
        return Err("The bill-to name is too long".into());
    }
    let bill_to_email = trimmed(input.bill_to_email.as_deref());
    if bill_to_email
        .as_deref()
        .is_some_and(|email| email.len() > 255 || !valid_email(email))
    {
        return Err("The bill-to email is not a valid address".into());
    }
    let project_address = trimmed(input.project_address.as_deref());
    if project_address
        .as_deref()
        .is_some_and(|address| address.chars().count() > 500)
    {
        return Err("The project address is too long".into());
    }
    if input.invoice_number.is_some_and(|number| number <= 0) {
        return Err("The invoice number must be a positive whole number".into());
    }
    let invoice_date = date(&input.invoice_date, "The invoice date")?;
    let due_date = trimmed(input.due_date.as_deref())
        .map(|due| date(&due, "The due date"))
        .transpose()?;
    if due_date.is_some_and(|due| due < invoice_date) {
        return Err("The due date is before the invoice date".into());
    }
    if input.lines.len() > MAX_LINES {
        return Err(format!("An invoice takes at most {MAX_LINES} lines"));
    }
    let mut lines = Vec::new();
    for (i, edited) in input.lines.iter().enumerate() {
        if let Some(line) = line(edited, i + 1)? {
            lines.push(line);
        }
    }
    if !lines.iter().any(|line| line.amount.is_some()) {
        return Err("The invoice needs a line with a price".into());
    }
    let tax = finite(input.tax, "Tax")?.map_or(0.0, cents);
    if tax < 0.0 {
        return Err("Tax cannot be negative".into());
    }
    let subtotal = cents(lines.iter().filter_map(|line| line.amount).sum());
    Ok(CleanInvoice {
        invoice_number: input.invoice_number,
        customer_id: input.customer_id.filter(|id| *id > 0),
        quote_id: input.quote_id.filter(|id| *id > 0),
        invoice_date,
        due_date,
        bill_to_name,
        bill_to_email,
        project_address,
        lines,
        subtotal,
        tax,
        total: cents(subtotal + tax),
        customer_memo: trimmed(input.customer_memo.as_deref()),
        private_note: trimmed(input.private_note.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(description: &str, qty: Option<f64>, unit_price: f64) -> InvoiceLine {
        InvoiceLine {
            description: description.into(),
            qty,
            unit_price: Some(unit_price),
            amount: Some(999.0),
        }
    }

    fn text(description: &str) -> InvoiceLine {
        InvoiceLine {
            description: description.into(),
            qty: None,
            unit_price: None,
            amount: None,
        }
    }

    fn input() -> InvoiceInput {
        InvoiceInput {
            invoice_number: None,
            customer_id: Some(100),
            quote_id: None,
            invoice_date: "2026-10-27".into(),
            due_date: Some(String::new()),
            bill_to_name: "  Jane Doe ".into(),
            bill_to_email: Some(" jane@x.test ".into()),
            project_address: Some("1 Main St, Carmel, IN".into()),
            lines: vec![
                priced("Kitchen @ 59.2 sqft", Some(59.2), 99.0),
                text("  Eased edge "),
                text("   "),
                priced("Discount", None, -150.0),
            ],
            tax: Some(213.456),
            customer_memo: Some("Thank you!".into()),
            private_note: Some("  ".into()),
        }
    }

    #[test]
    fn totals_come_from_the_lines_and_tax() {
        let invoice = clean(&input(), |_| true).unwrap();
        assert_eq!(invoice.bill_to_name, "Jane Doe");
        assert_eq!(invoice.bill_to_email.as_deref(), Some("jane@x.test"));
        assert_eq!(invoice.due_date, None);
        assert_eq!(invoice.private_note, None);
        // The blank line is dropped; the text line keeps no numbers.
        assert_eq!(invoice.lines.len(), 3);
        assert_eq!(invoice.lines[0].amount, Some(5860.8));
        assert_eq!(invoice.lines[1].description, "Eased edge");
        assert_eq!(invoice.lines[1].amount, None);
        assert_eq!(invoice.lines[2].qty, Some(1.0));
        assert_eq!(invoice.lines[2].amount, Some(-150.0));
        assert!((invoice.subtotal - 5710.8).abs() < 1e-9);
        assert!((invoice.tax - 213.46).abs() < 1e-9);
        assert!((invoice.total - 5924.26).abs() < 1e-9);
    }

    #[test]
    fn refuses_what_cannot_be_invoiced() {
        let refused = |change: fn(&mut InvoiceInput)| {
            let mut input = input();
            change(&mut input);
            clean(&input, |email| email.contains('@')).unwrap_err()
        };
        assert!(refused(|i| i.bill_to_name = " ".into()).contains("name"));
        assert!(refused(|i| i.bill_to_email = Some("nope".into())).contains("email"));
        assert!(refused(|i| i.invoice_date = "10/27/2026".into()).contains("date"));
        assert!(refused(|i| i.due_date = Some("2026-10-01".into())).contains("before"));
        assert!(refused(|i| i.invoice_number = Some(0)).contains("number"));
        assert!(refused(|i| i.tax = Some(-1.0)).contains("Tax"));
        assert!(refused(|i| i.lines = vec![text("Only a note")]).contains("price"));
        assert!(refused(|i| i.lines[0].description = String::new()).contains("Line 1"));
        assert!(refused(|i| i.lines[0].unit_price = Some(f64::NAN)).contains("Line 1"));
    }
}

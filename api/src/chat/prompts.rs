//! The chat's fixed prompts and question routing (from the CRM's
//! `api.chat.ts` and `lib/chatTools.ts`).

use crate::re;

/// How the model uses the company instructions.
pub const INSTRUCTION_RULES: &str = r#"How to use this context:
1. Each entry above is one instruction with a clear TITLE and the BODY that belongs to that exact title.
2. When the user asks about a topic, find the instruction whose TITLE or BODY best matches what they need.
3. If the user asks about a price, cost, fee, rate, or charge, search instruction BODIES for the specific dollar amount or rate they asked about. The answer often lives under a general title like "Standard Formula" even when the title does not mention the topic.
4. Match your answer scope to the question scope. Price questions get narrow answers — for example, if the user asks "price for fabrication", give only the fabrication rates by material; do not include special-order formulas, minimum charges, contractor discounts, cutting board prices, or other fees unless the user asked about them. Procedure and workflow questions get complete checklists for the matching scenario (see rule 19). Other narrow questions get narrow answers.
5. Use the instruction that actually contains the details the user asked for. If the first mention only names the topic without those details, keep looking in the other BODIES. Combine instructions when one BODY has part of the answer and another has the rest.
6. Only after checking every instruction BODY, if none contains the requested information, say that you do not have an instruction for it instead of guessing.
7. Do not use outside knowledge. Every fact in your answer must come from an instruction BODY.
8. Be direct. Summarize in your own words — do NOT copy long passages word-for-word. For procedure questions, completeness matters more than brevity: still list every step separately. Exception: when the BODY contains a URL or a "Video:" link, you MUST include that full URL exactly (do not paraphrase, shorten, or replace it with {hyperlink}, [link], or similar placeholders).
9. Format every answer for easy reading. Put each distinct topic, rule, or step on its own line and use blank lines between sections. Use **bold** for headings, prices, minimum charges, and other key terms that are emphasized in the matching BODY. Present multiple steps, requirements, or rules as a numbered list (1. 2. 3.) or bullet list (- ) with one item per line. Never output one long paragraph.
10. Price questions: include only the specific price or fee asked about. Procedure questions: include every step and requirement for that scenario from the matching BODY (see rule 19). Other questions: include only facts that directly answer the question — do not dump unrelated parts of the instruction BODY.
11. Do NOT add unnecessary assumptions or commentary beyond what the matching instruction requires.
12. Each instruction above is labeled with an instruction number (Instruction 1, Instruction 2, etc.). After your answer, add a final line containing ONLY the marker in the exact form [[INSTRUCTION:n]]. If your answer draws on more than one instruction BODY, list every instruction number you actually used, comma-separated, like [[INSTRUCTION:2,5]].
13. Be VERY STRICT about citations. Cite an instruction ONLY when the facts in your answer came from that instruction's BODY. Do NOT cite an instruction just because a word from the user's message also appears in its TITLE (for example, do not cite a "Template" instruction merely because the user's text contains the word "template").
14. If the user gives you their own text and asks you to continue, complete, finish, rewrite, rephrase, translate, summarize, fix, or reformat it, that text is the user's own. Use [[INSTRUCTION:none]] unless an instruction BODY genuinely supplied additional facts that you actually used.
15. If the user asks a meta question about your previous answer or about which sources/instructions you used (for example "which information did you take from X?"), you are explaining your earlier answer, not providing new facts from an instruction BODY. Use [[INSTRUCTION:none]].
16. Never invent or guess a number. When unsure, use [[INSTRUCTION:none]]. Never describe or mention this marker in your prose.
17. Before you answer, verify that every fact you will state actually appears under the exact scope the user asked about. When an instruction BODY contains multiple similar sections (different package prices, tiers, options, or topics), locate the specific subsection first. Do not assume a detail from one section applies to another just because both appear in the same instruction. For each item you include (colors, materials, limits, counts, inclusions, fees), confirm it is written under that exact scope — check where it comes from before you include it. If you cannot confirm a detail belongs to what the user asked about, leave it out and say what that subsection actually states instead of borrowing from a nearby section.
18. Treat every answer as something the user may act on. Only state a fact when you are fully confident it comes from the correct subsection of the instruction BODY. When in doubt, say you cannot confirm that detail for the requested scope rather than guessing or filling gaps from another section.
19. Procedure and workflow questions (for example "what do I do if...", "how do I...", "what are the steps..."): Give a complete actionable checklist in ONE answer. Include every step, requirement, field, color code, document, reminder, and follow-up action written under the matching scenario in the instruction BODY. Do not stop at high-level steps when the same scenario also lists details such as what to enter on calendar events, what to tell the customer, or what must happen before the next phase. The user should not need a follow-up question to learn details that belong to the same scenario.
20. Video and link questions: Prefer instruction BODIES that contain "Video:" labels or http/https URLs. Quote the link title and paste the full URL so the user can open it.
21. If the user is asking how to use Granite Manager itself (for example how to add a stone in the CRM, where a page is, or how a screen or button works), do not answer from company instructions. Answer using the CRM user manual provided in this context, then use [[INSTRUCTION:none]].
22. Always check every instruction in this context before answering. Company instructions are split across many titles; the details often live in a later one.
23. Users type fast: expect typos, wrong or missing digits, partial names, and short questions. Work out the most likely meaning from the instructions and answer it, starting with one short line that says what you assumed (for example "Showing **$15,499** (closest match to $154,999)."). Follow any LIKELY TYPOS note. Ask a question back only when two readings are equally likely and give different answers, and then name both options in one line (for example "Did you mean the **$11,999** or the **$15,499** package?") rather than asking an open question."#;

/// How the model uses the supplier price lists.
pub const PRICE_LIST_RULES: &str = r#"How to use this context:
1. You are in Price lists mode. Use ONLY the supplier documents below.
2. These documents are structured Markdown tables extracted from the supplier's price files. Each row has columns for color/product, thickness, group/level, slab size, and price. A "-" in a column means the source document showed no value for that cell, not a price of zero. Exception for slab size: if a section or block shows one size for all products (merged cell, heading, or the first row of the block), that size applies to every product in the block even when later rows show "-". Do not say the slab size is not specified in that case.
3. Thickness is optional. If the document has no thickness for a product, answer with the listed price and do not mention missing thickness.
4. When the same product has multiple different thicknesses with prices:
   - If the user already named a thickness (for example 2cm / 2 CM / 3cm / 20 mm), use that matching row. If that thickness is not listed, say so and list the available thicknesses with their prices, then ask which one they want.
   - If the user did not name a thickness, do NOT pick one. List each available thickness with its price and ask which thickness they would like to use. Do not add a special-order marker. Instead add a thickness-options marker on its own line at the end in the exact form [[THICKNESS_OPTIONS:0.8 CM=19.69|1.2 CM=29.82|2 CM=36.78]] (label=price per option, pipe-separated, no $ signs). The app shows buttons from this marker. Never describe or mention this marker in your prose.
5. When the chosen product/thickness has multiple slab sizes with prices or availability (for example 126" x 57" and 130" x 64"):
   - If the user already named one size, use that size.
   - If the user did not name a size, do NOT pick one and do NOT add a special-order marker. List the available slab sizes and add a slab-size-options marker on its own line at the end in the exact form [[SLAB_SIZE_OPTIONS:126x57|130x64]]. The app shows buttons from this marker. Never describe or mention this marker in your prose.
5b. When the search returns multiple distinct products or color variants each with its own price (for example Soapstone Metropolis and Soapstone Mist - Concrete), and the user has not picked one yet:
   - Do NOT pick one and do NOT add a special-order marker. List each product with its price and slab size, then add a price-list-options marker on its own line at the end in the exact form [[PRICE_LIST_OPTIONS:Soapstone Metropolis*=21.47,130,65|Soapstone Mist - Concrete=24.53,130,65]] (label=price per sqft, optional length and width in inches after commas, pipe-separated, no $ signs). The app shows buttons from this marker. Never describe or mention this marker in your prose.
6. Only state a dollar price if an exact price appears in the documents. Never calculate, estimate, or invent a price.
7. Some supplier tables list many products in a Group N block with no price on each row, and one price at the bottom of that block. That bottom price is the Group N price for every product listed above it in the same block unless a row shows its own price. When a GROUP N SLAB PRICE line appears in the context, use it for products in that group.
8. Always check every SOURCE from the matched supplier before answering. Suppliers often have multiple price-list files; never stop after the first SOURCE.
9. Many supplier documents show color groups or levels instead of prices. When one SOURCE from a supplier lists the product with a group or level and size but no dollar amount on its row, check the GROUP price line and every other SOURCE from that same supplier before answering.
10. Only if no SOURCE from that supplier contains a dollar price should you say the price is not specified and state the level or group and size. Example: "The price is not specified. The level is Group 7. The size is 126x63."
11. When citing a dollar price, use the SOURCE that contains the price, not the color group document.
12. If the requested color name is close but not exact (for example the user asks for "Adonia" and the document lists "Calacatta Adonia"), give the price for the closest matching product from that supplier. Say clearly that the name was not an exact match, give the exact product name from the document, then state the price and slab size. Example: "There is no exact match for "Adonia", but Calacatta Adonia is listed at $21.47 per sqft for a 130x79 slab."
13. Only say you could not find the product when there is no reasonable close match in the documents from that supplier.
14. Be brief and direct. Do not guess or use outside knowledge.
15. Each document is labeled with a SOURCE number. After your answer, add a final line containing ONLY the marker in the exact form [[SOURCE:n]] (use a colon, no spaces), where n is the SOURCE number of the document in which you found the color/product you used. Cite ONLY that document. Do not cite other documents you merely checked. Use [[SOURCE:n,m]] only when the same color appears in more than one document and you used facts from more than one. If you could not find the product, use [[SOURCE:none]]. Never describe or mention this marker in your prose.
16. Whenever you state one exact dollar price the user can act on (whether per sqft, per slab, or other wording), do NOT ask about special orders in your prose. Always add a special-order marker on its own line at the end. If you also know the single chosen slab size in inches, use [[SPECIAL_ORDER:price=X,length=Y,width=Z]] where X is the price per sqft (convert from slab price if the document lists per slab), Y is length in inches, and Z is width in inches. A section-wide size (one size for all slabs in the block) counts as a known size — include those inches. Only use [[SPECIAL_ORDER:price=X]] with no dimensions when no size appears anywhere in that product's section. The app adds the special order question automatically. Never describe or mention this marker in your prose. Skip this marker when multiple thicknesses are still unresolved (use [[THICKNESS_OPTIONS:...]] instead), when multiple slab sizes are still unresolved (use [[SLAB_SIZE_OPTIONS:...]] instead), or when multiple products/variants are still unresolved (use [[PRICE_LIST_OPTIONS:...]] instead)."#;

/// How the model uses the CRM user manual.
pub const CRM_MANUAL_RULES: &str = r#"How to use the CRM user manual:
1. Use the CRM user manual above to answer questions about how to use Granite Manager: navigation, features, workflows, forms, and settings described in the manual.
2. For CRM how-to questions, use ONLY the CRM user manual — not company instructions, supplier price lists, or outside knowledge. Every fact about the CRM UI must come from the manual.
3. Do not use the manual for non-CRM questions. If the question is about company procedures, pricing, or another topic covered by the active context, answer from that context instead.
4. If the exact field, button, slider, or workflow the user asked about is not described in the manual, say clearly that you cannot confirm that feature exists. Do NOT invent controls, sliders, filters, or pages. Do NOT redirect the user to a different page unless that page is explicitly documented for the same task. Never guess.
5. Be brief and direct. Never describe what the manual does or does not cover, what is documented or undocumented, or where information lives. Never use the word "manual" in your answer or in follow-up offers like "if you want I can summarize...". Just give the steps and move on. Summarize in your own words instead of copying long passages.
6. Format answers for easy reading. Use **bold** for section names, buttons, and menu items. Use numbered or bullet lists for steps. Use only button, tab, and menu labels that appear in the manual text. Never invent control names (for example do not invent a **Call history** button when the manual describes **Made a call** rows in an activity dialog).
7. Match your answer scope to the question. A narrow question gets a narrow answer. Only include details that directly relate to what the user asked. Do not volunteer extra features, checkboxes, filters, or alternative pages if they do not help answer the question. If a user asks how to tell leads from customers on the Customers page, give the source filter tabs — not the Invalid leads checkbox, and not how Deals shows pipeline progress unless the user asked about the pipeline.
8. Deal notes are a real feature. When asked how to add, edit, pin, or delete a note, use the deal activity panel (Notes, Write a note..., Add Note) and/or Customer Information (deal card, Add Note).
9. Deal workspace structure: left tabs are Project, General, Files, Quotes (deal images and documents are sections inside Files); right side is the activity panel (Activity / Notes + History). History is not a left-column tab.
10. Deals have no dedicated address field. To show an address on the deal board card, use General tab, Title — it displays under the customer name. The customer actual address is on the customer record and appears read-only on the Project tab; change it with General > Edit Customer or in CRM > Customers.
11. Deals have no dedicated invoice amount field. For a single pipeline value, use General, Amount or inline amount on the board card. Priced work belongs in a quote (Quotes). Invoices are QuickBooks invoices in Customer Information > Company info > QuickBooks (when QuickBooks is connected); there is no Invoices page in the sidebar.
12. Deals have no "selected stone" field. Stone choice and square footage belong in the deal's quote (Quotes: the drawing tool calculates the square footage and holds the color options). For a quick board label use General, Title (e.g. "Misterio 45 sqft"). To actually take material, sell the slab from Inventory > Stones, which creates a transaction tied to the customer.
13. Navigation paths: describe where to go using only sidebar labels and arrows, like CRM > Customers or Inventory > Stones. Never include raw URLs (e.g. /employee/customers) or parenthesized paths in your answer. The audience does not need routes — just the sidebar path.
14. Company logo upload: Admin, Company settings, Company, Logo section, file chooser, optional Logo height slider, Save, toast Company settings saved. Logo displays in the app header. Stone Main photo is Admin, Inventory, Stones.
15. Sharing the customer catalog: go to any page, click header Customer button, copy the full URL from the browser address bar, send to the customer. This is the public showroom link — not the staff-only Customers page.
16. Telegram setup (employees): go to **Dashboard**, find the **Telegram** card and click **Connect Telegram**; it links two bots one after the other (leads bot, then notifications bot) — click it again if the card is still there. Shop accounts use **My Account** > **Connect notifications bot**. Leads assigned to you then arrive in Telegram; Sales Managers who connect get new-lead alerts and assign incoming leads. Which alerts go to Telegram is set under the header bell > gear.
17. Position-based access. The CRM has 11 user positions; some lock the user into one portal. Always know which position your answer applies to:
  a. Sales Rep (position 1): Employee CRM — Dashboard, Inventory (Stones, Sinks, Faucets), CRM (Customers, Deals, Emails, SMS), Quotes, Schedule, Transactions, Resources, Operations, My Account, Samples. NO admin pages.
  b. Sales Manager (position 2): Same as Sales Rep plus Invalid and Delete on the Customers list, Telegram lead alerts, and assigning incoming leads.
  c. Shop Manager (position 3): Adds Maintenance to the Employee/Admin sidebar (only Shop Managers can edit equipment there) and opens the shop pages. Otherwise access depends on Employee/Admin access.
  d. Shop Worker (position 4): Shop app only — Transactions, Samples, Maintenance, My Account. Can NOT access employee CRM or admin pages.
  e. Manager (position 5): Unlocks Company settings > Billing on its own. When paired with Admin access it also UNLOCKS Admin > Company settings > Users and Admin > Company settings > Company (company sales tax, logo, address). Without this position, even an admin user cannot see those pages.
  f. Installer (position 6): Installer portal only — Schedule, Map, Checklist (post-install checklists), and Invoices when QuickBooks is connected.
  g. Marketing (position 7): External Marketing portal only — leads, add lead, edit lead, delete lead, walk-in stats, deal-by-stage summary. Can NOT access employee CRM or admin pages. Only mention marketing features (leads, marketing portal, marketing-sourced customers) when the user specifically asks about marketing or leads. Do not volunteer marketing features when a user asks about the employee or admin CRM.
  h. Walk-in (position 8): Only the walk-in check-in form.
  i. Super Admin (position 9): Full admin and employee access across all assigned companies. Can switch companies from the header.
  j. Office Manager (position 10): Employee CRM; automatic customer texts (estimate confirmations and reminders, survey links) go out from this person's line.
  k. Template Guy (position 11): Template schedule portal only (the technician's own appointments), unless the user is also a Super Admin, who then gets My Schedule in the Employee sidebar.
18. When your answer mentions Admin pages (Admin > Company settings > Company, Admin > Company settings > Users, Admin > Survey, Admin > CRM > Automations, Admin > Quotes, etc.), make it clear that only users with Admin access can reach those pages. Do not suggest an admin-only page to someone who only described employee-level actions.
19. Tax: quote tax is chosen per quote in the drawing tool's Price Details (Pricing Settings), with presets from Admin > Quotes > Tax Rates (any admin). The company sales tax in Admin > Company settings > Company (Manager position and Admin access) is used by Special Order, supplier-priced colors and chat price-list calculations. Say which one applies.
20. SMS attachments: When attachments are enabled (the default), **CRM > SMS** paperclip **Attach files** can add images and documents — **Upload from computer**, **From Stones**, **From Images**, **From Documents** (max 10). Image attachments can be edited before send. If attachments are turned off, the paperclip is hidden and the composer is text, emoji, templates, and AI only. Never say SMS can never send pictures.
21. Instruction position visibility (**Visible to**): On Admin > Resources > Instructions, each instruction has a **Visible to** control on Add/Edit. Hover to open position toggle switches. No toggles on = **Everyone**. One or more toggles on = only users with at least one matching position see it on Employee > Instructions and in the AI Chat. Available toggles: Sales Representative, Sales Manager, Shop Manager, Shop Worker, Manager, Installer, Marketing, Office Manager, Template Guy. Super Admin and Walk-in are not in the panel. Admin Instructions page shows all items regardless. Teach Mode is NOT position-filtered. Each instruction has its own setting — children do not copy the parent's setting, but when a parent is hidden from a user, everything under it is hidden too.
22. Stone photo brightness/contrast/RGB sliders: ONLY in the **Edit image** dialog of a stone's main photo (**File**) on **Admin > Inventory > Stones** (Add or Edit). Hover the preview, click **Edit image**, use Brightness, Contrast, Red, Green, Blue; click **Crop image** and drag corners to crop; click **Use edited image**, then save the stone. **Resources > Images** (Admin or Employee) has NO brightness, contrast, RGB, or crop controls — only name, folder and file upload/replace. Never tell users to adjust contrast on Resources > Images.
23. Follow CURRENT USER ACCESS and the access rules below. Never give Superuser Dashboard / Create Company steps unless platform superuser = yes.
24. CloudTalk phone number vs agent ID: **CloudTalk agent ID** and **CloudTalk phone number** are separate fields on **My Account** (shown when the company uses CloudTalk) and on **Admin > Company settings > Users > Edit**. The phone number is the line texts are sent from and is also needed (with the agent ID) for calling from the computer; the agent ID links call history. RingCentral users have a separate **RingCentral phone number** field. To find the actual phone number value, use CloudTalk Dashboard > Numbers (CloudTalk Admin access). Never say only the agent ID field exists in the CRM.
25. Changing a person's name (installer, sales rep, technician, shop worker, any staff member) is always the same task: rename their user account. There is no separate installer-name, sales-rep-name, or technician-name field anywhere in the product. Steps: Admin > Company settings > Users, find the row, **⋯** > **Edit**, change **Name**, **Save** (toast **User updated**). A **Manager** with Admin access can do this (a Super Admin also needs the Manager position), and a Manager may rename anyone in their own company including installers. The new name then appears wherever that person is shown (Survey cards, the survey installer and sales rep dropdowns, checklists, Users Activity, assignment lists), including surveys and checklists submitted before the rename. Never answer that you cannot confirm how to change a name, and never treat "installer name" as an undocumented field. Users can also change their own name on My Account.
26. Mark as unread / Mark as read are real features. Emails: **CRM > Emails**, **Inbox** row envelope (closed = **Mark as unread**, open = **Mark as read**), or the toolbar envelope after selecting threads in Inbox or Sent. Opening a thread marks it read. SMS: **CRM > SMS**, hover a read row and click the envelope **Mark as unread**. Opening an SMS thread marks it read. Never answer that you cannot confirm Mark as unread.
27. Users type fast: expect typos and loose names for pages, buttons, and features (for example "custmers", "deal bord", "the stones page"). Answer for the closest matching page or feature, starting with one short line naming it if the wording was off. Ask a question back only when two features fit equally well, and then name both in one line.
28. Keep how-to answers short: the numbered steps of the one way that does what was asked (about 8 at most, one line each), then one line on who can do it when that matters. No background, alternatives, troubleshooting or extra settings the user did not ask about.
29. A special order in a quote is a color whose name is typed in **Color** instead of picked from the list, priced in the **$ Price /sq ft** box. Never write the words "Other color" or "other color", even when the user does: say special order."#;

/// `buildInstructionUserMessage`: the question with the hints it needs.
pub fn instruction_user_message(query: &str) -> String {
    let price = if super::instructions::is_price_query(query) {
        "Answer only the specific price or fee the user asked about. Do not include formulas, minimum charges, discounts, or unrelated fees unless the user asked for them.\n\n"
    } else {
        ""
    };
    let procedure = if super::instructions::is_procedure_query(query) {
        "Give a complete checklist for this scenario. Include every step and requirement from the matching instruction subsection so the user does not need follow-up questions.\n\n"
    } else {
        ""
    };
    let video = if super::instructions::is_video_or_link_query(query) {
        "If the matching instruction BODY contains a video or URL, include the full URL exactly in your answer. Never write placeholders like {hyperlink} or [link].\n\n"
    } else {
        ""
    };
    format!(
        "{price}{procedure}{video}Before answering, verify each fact belongs to the exact scope of the question. Do not borrow details from other sections in the same instruction.\n\nAnswer the question as best as you can.\n\nQuestion: {query}\n\nAnswer:"
    )
}

/// A how-to answered from the CRM manual: short steps, not the company
/// instructions' complete checklist.
pub fn manual_user_message(question: &str) -> String {
    // The users' word for a special order, which the app doesn't use: the
    // model kept writing it back to them.
    let other_color = if re!(r"\bother colou?rs?\b", "i").is_match(question) {
        "\n\nThe user writes \"other color\": in Granite Manager that is a special order, a color name typed in Color instead of picked from the list. Don't write the words \"other color\"."
    } else {
        ""
    };
    format!("{MANUAL_QUESTION}{other_color}\n\nQuestion: {question}\n\nAnswer:")
}

/// How [`manual_user_message`] starts.
const MANUAL_QUESTION: &str = "Answer how to do this in Granite Manager from the CRM user manual: short numbered steps with the exact labels, only what the question needs.";

/// Whether `message` is a question [`manual_user_message`] wrote: its answer
/// came from the CRM manual.
pub fn is_manual_question(message: &str) -> bool {
    message.starts_with(MANUAL_QUESTION)
}

/// The question in a message [`instruction_user_message`] or
/// [`manual_user_message`] wrote (any other message as it is).
pub fn asked_question(message: &str) -> &str {
    message
        .split_once("\n\nQuestion: ")
        .and_then(|(_, rest)| rest.strip_suffix("\n\nAnswer:"))
        .unwrap_or(message)
}

fn lookup_kind_is_contact(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.contains('@') {
        return true;
    }
    let digits = trimmed.chars().filter(char::is_ascii_digit).count();
    digits >= 7 && re!(r"^[\d\s()+.-]*$").is_match(trimmed)
}

/// `looksLikeCustomerLookupQuery`.
fn looks_like_customer_lookup(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    if lookup_kind_is_contact(trimmed) {
        return true;
    }
    let lower = trimmed.to_lowercase();
    if re!(r"^how\b").is_match(&lower) {
        return false;
    }
    if re!(r"\b(instruction|procedure|price|pricing|manual|policy|rule|step|video|link)\b")
        .is_match(&lower)
    {
        return false;
    }
    let subject =
        re!(r"\b(customers?|clients?|contacts?|leads?|people|person|accounts?)\b").is_match(&lower);
    let verb = re!(r"\b(find|look\s*up|search|pull\s*up|show|open|get)\b").is_match(&lower);
    let named = re!(r"\bnamed\b").is_match(&lower);
    let contact_of_name =
        re!(r"\b\w+'s\s+(?:phone|telephone|email|e-?mail|address|number)\b").is_match(&lower);
    contact_of_name || (subject && (verb || named))
}

/// `looksLikeCrmDataQuery`.
fn looks_like_crm_data(query: &str) -> bool {
    let lower = query.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    let manual_shaped = [
        re!(r"^how\s+(do|does|did|can|could|would|should|to)\b"),
        re!(r"\bhow\s+(do|does)\s+(i|we|you)\b"),
        re!(r"\b(what|where|when)\s+(do|should|can)\s+(i|we)\b"),
        re!(
            r"\b(instructions?|procedures?|manual|handbook|policy|policies|tutorials?|videos?|training|guidelines?|best practices?|tips?|scripts?|templates?)\b"
        ),
        re!(r"\b(steps?|checklist|workflow|process)\b"),
        re!(r"\b(commissions?|salar(?:y|ies)|compensation)\b"),
    ];
    if manual_shaped.iter().any(|p| p.is_match(&lower)) {
        return false;
    }
    if re!(r"\b(deals?|leads?|customers?|clients?|contacts?|accounts?|invoices?|payments?|appointments?|emails?|e-mails?|messages?|texts?|sms|calls?|notes?|threads?|attachments?|tasks?|reminders?|reps?|users?|employees?)\b").is_match(&lower) {
        return true;
    }
    re!(r"\b(sales?|jobs?|projects?|installs?|installations?|measures?|estimates?|quotes?|slabs?|stones?|inventory)\b").is_match(&lower)
        && re!(r"\b(how many|how much|which|who|whose|anyone|anybody|show me|give me|find|search|pull(?:\s*up)?|count|total|breakdown|top\s+\d+|most recent|latest|did we|have we|has anyone)\b|\bby (company|rep|source|stage|month|year|week)\b").is_match(&lower)
}

/// `looksLikeAssistantQuery`: a question about the company's own records.
pub fn looks_like_assistant_query(query: &str) -> bool {
    looks_like_customer_lookup(query) || looks_like_crm_data(query)
}

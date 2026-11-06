# Project rules

- After every request, whatever it is, push the work, open a PR into `main` and merge it right away; always, never ask first, never wait for CI. Migrations and deploys stay manual. Full rule: @.cursor/rules/always-pr-and-merge-main.mdc

- Answer the owner short: reply to exactly what was asked in a few lines (a yes/no question gets yes/no plus one line). Reports: one line per item. No long explanations unless asked.
- No web pages in the phone app, ever: no links to the CRM or website, no in-app browser, no web view, no "open on the web" handoff. Every feature the website has is built as native app screens. Only outside apps stay outside (maps, phone dialer, mail, app store, the privacy policy).
- Build what the owner asks for in full, natively, where they ask for it: "add the website's features to the app" means real app screens with every feature, never a link or an embedded web page. Never ship a shortcut in place of what was asked; if something truly cannot be done, say so before starting.

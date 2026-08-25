#!/usr/bin/env python3
import json
import os
import time
import urllib.request

TOKEN = os.environ.get("TELEGRAM_NOTIFICATIONS_BOT_TOKEN", "")
CHAT_ID = os.environ.get("TELEGRAM_TEST_CHAT_ID", "462986068")

MESSAGES = [
    {
        "text": "✉️ New email\n\n👤 Jane Doe\n✉️ Template approval needed",
        "button": ("📬 Open Email", "https://granite-manager.com/employee/deals/edit/101/project/chat/test-thread-1"),
    },
    {
        "text": "✉️ New email\n\n👤 Gregg Yancy\n✉️ Re: Granite Depot quote",
        "button": ("📬 Open Email", "https://granite-manager.com/employee/emails/chat/test-thread-2"),
    },
    {
        "text": "📋 Activity Reminder\n\n👤 Gregg Yancy\nFollow up on install date",
        "button": ("📂 Open Deal", "https://granite-manager.com/employee/deals/edit/202/project"),
    },
    {
        "text": "💬 New CloudTalk SMS from +13175551234\n\nHi, can you send me the quote?",
        "button": ("💬 Open SMS", "https://granite-manager.com/employee/cloudtalk/thread/13175551234"),
    },
]


def send_message(text: str, button: tuple[str, str] | None = None) -> dict:
    payload: dict = {"chat_id": CHAT_ID, "text": text}
    if button:
        label, url = button
        payload["reply_markup"] = {
            "inline_keyboard": [[{"text": label, "url": url}]],
        }
    req = urllib.request.Request(
        f"https://api.telegram.org/bot{TOKEN}/sendMessage",
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read().decode())


def main() -> None:
    if not TOKEN:
        raise SystemExit("TELEGRAM_NOTIFICATIONS_BOT_TOKEN is not set")

    for index, message in enumerate(MESSAGES, start=1):
        result = send_message(message["text"], message.get("button"))
        ok = result.get("ok")
        print(f"{index}/{len(MESSAGES)} ok={ok} title={message['text'].split(chr(10), 1)[0]}")
        if not ok:
            print(json.dumps(result, indent=2))
            raise SystemExit(1)
        time.sleep(0.4)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
import json
import os
import time
import urllib.request

TOKEN = os.environ.get("TELEGRAM_NOTIFICATIONS_BOT_TOKEN", "")
CHAT_ID = os.environ.get("TELEGRAM_TEST_CHAT_ID", "462986068")

MESSAGES = [
    "✉️ New email\n\nCustomer: Jane Doe\nSubject: Template approval needed\n\nhttps://granite-manager.com/employee/deals/edit/101/project/chat/test-thread-1",
    "✉️ New email\n\nCustomer: Gregg Yancy\nSubject: Re: Granite Depot quote\n\nhttps://granite-manager.com/employee/emails/chat/test-thread-2",
    "✉️ New email\n\nCustomer: Julie\nSubject: Slab layout question\n\nhttps://granite-manager.com/employee/deals/edit/202/project/chat/test-thread-3",
    "📋 Added an Activity\n\nCustomer: Jane Doe\nDema: Call customer about template\n\nhttps://granite-manager.com/employee/deals/edit/101/project",
    "📋 Activity Reminder\n\nCustomer: Gregg Yancy\nFollow up on install date\n\nhttps://granite-manager.com/employee/deals/edit/202/project",
    "📋 Edited an Activity\n\nCustomer: Julie\nTania: Send slab layout\n\nhttps://granite-manager.com/employee/deals/edit/303/project",
    "💬 New CloudTalk SMS from +13175551234\n\nHi, can you send me the quote?\n\nOpen thread: /employee/cloudtalk/thread/13175551234",
    "💬 New CloudTalk SMS from +16145559876\n\nWe are ready to schedule template.\n\nOpen thread: /employee/cloudtalk/thread/16145559876",
    "💬 New CloudTalk SMS from +13179419741\n\nThanks, I will review the layout tonight.\n\nOpen thread: /employee/cloudtalk/thread/13179419741",
]


def send_message(text: str) -> dict:
    payload = json.dumps({"chat_id": CHAT_ID, "text": text}).encode()
    req = urllib.request.Request(
        f"https://api.telegram.org/bot{TOKEN}/sendMessage",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read().decode())


def main() -> None:
    if not TOKEN:
        raise SystemExit("TELEGRAM_NOTIFICATIONS_BOT_TOKEN is not set")

    for index, text in enumerate(MESSAGES, start=1):
        result = send_message(text)
        ok = result.get("ok")
        print(f"{index}/9 ok={ok} title={text.split(chr(10), 1)[0]}")
        if not ok:
            print(json.dumps(result, indent=2))
            raise SystemExit(1)
        time.sleep(0.4)


if __name__ == "__main__":
    main()

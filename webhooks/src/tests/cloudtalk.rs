//! Shared `CloudTalk` webhook payloads. Phones are fixture numbers, never real ones.

/// Inbound SMS carrying text, with the `[sender]`/`[text]` suffixes an older mapping produced.
pub const INBOUND_SMS: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";

/// Shape captured in production 2026-08-04: a photo-only MMS sends text as JSON null.
pub const INBOUND_MMS_NULL_TEXT: &[u8] = b"{\"id\":51753924,\"sender\":\"+16468956758\",\"recipient\":\"+13173161456\",\"text\":null,\"agent\":null,\"media\":null,\"attachments\":null,\"media_urls\":null}";

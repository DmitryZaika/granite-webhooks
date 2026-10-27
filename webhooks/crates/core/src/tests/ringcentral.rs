//! Shared `RingCentral` webhook payloads. Phones are fixture numbers, never real ones.

/// Inbound SMS carrying text, with the `[sender]`/`[text]` suffixes an older mapping produced.
pub const INBOUND_SMS: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";

/// Shape captured in production: RingCentral may send text as JSON null for an empty body.
pub const INBOUND_NULL_TEXT: &[u8] = b"{\"id\":51753924,\"sender\":\"+16468956758\",\"recipient\":\"+13173161456\",\"text\":null,\"agent\":null,\"media\":null,\"attachments\":null,\"media_urls\":null}";

/// `RingCentral`'s own `message-store/instant?type=SMS` webhook body (fixture numbers).
pub const NOTIFY_INBOUND_SMS: &[u8] = br#"{"uuid":"5c5a4c1e-0000-0000-0000-000000000000","event":"/restapi/v1.0/account/~/extension/540273/message-store/instant?type=SMS","timestamp":"2026-10-08T19:34:23.000Z","subscriptionId":"sub-1","ownerId":"540273","body":{"id":"3894319559027","to":[{"phoneNumber":"+13173161456","name":"Rep","location":"Indianapolis, IN"}],"from":{"phoneNumber":"+16468956758","location":"New York, NY"},"type":"SMS","creationTime":"2026-10-08T19:34:23.000Z","lastModifiedTime":"2026-10-08T19:34:23.000Z","readStatus":"Unread","priority":"Normal","attachments":[{"id":"3894319559027","type":"Text","contentType":"text/plain"}],"direction":"Inbound","availability":"Alive","subject":"Is the slab still available?","messageStatus":"Received","conversationId":"123"}}"#;

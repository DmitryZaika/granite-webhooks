use serde_json::Value;

pub fn ses_open_event_json() -> Value {
    serde_json::from_str(r#"{
        "version": "0",
        "id": "df1515d9-b441-32ac-346a-8b4d5dd153c6",
        "detail-type": "Email Opened",
        "source": "aws.ses",
        "account": "741448943665",
        "time": "2025-11-19T00:12:34Z",
        "region": "us-east-2",
        "resources": [
            "arn:aws:ses:us-east-2:741448943665:configuration-set/email-tracking-set"
        ],
        "detail": {
            "eventType": "Open",
            "mail": {
                "timestamp": "2025-11-19T00:12:33.545Z",
                "source": "colin99delahunty@gmail.com",
                "sendingAccountId": "741448943665",
                "messageId": "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000",
                "destination": ["colin99delahunty@gmail.com"],
                "headersTruncated": false,
                "headers": [
                    {"name":"From","value":"colin99delahunty@gmail.com"},
                    {"name":"To","value":"colin99delahunty@gmail.com"},
                    {"name":"Subject","value":"Product Overview Followup"},
                    {"name":"MIME-Version","value":"1.0"},
                    {"name":"Content-Type","value":"text/html; charset=UTF-8"},
                    {"name":"Content-Transfer-Encoding","value":"7bit"}
                ],
                "commonHeaders": {
                    "from": ["colin99delahunty@gmail.com"],
                    "to": ["colin99delahunty@gmail.com"],
                    "messageId": "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000",
                    "subject": "Product Overview Followup"
                },
                "tags": {
                    "ses:source-tls-version": ["TLSv1.3"],
                    "ses:operation": ["SendEmail"],
                    "ses:configuration-set": ["email-tracking-set"],
                    "ses:source-ip": ["68.44.153.241"],
                    "ses:from-domain": ["gmail.com"],
                    "ses:caller-identity": ["dima-ses"]
                }
            },
            "open": {
                "timestamp": "2025-11-19T00:12:34.926Z",
                "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/42.0.2311.135 Safari/537.36 Edge/12.246 Mozilla/5.0",
                "ipAddress": "108.177.2.32"
            }
        }
    }"#).unwrap()
}

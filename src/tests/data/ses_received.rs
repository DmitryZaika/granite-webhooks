use serde::Deserialize;

pub fn ses_received_json<T: for<'de> Deserialize<'de>>() -> T {
    serde_json::from_str(
        r#"{
        "version":"0",
        "id":"fdad161d-2f64-0050-176a-3cd66024e243",
        "detail-type":"Object Created",
        "source":"aws.s3",
        "account":"741448943665",
        "time":"2025-11-23T18:45:59Z",
        "region":"us-east-2",
        "resources":["arn:aws:s3:::granite-ses-inbound-emails"],
        "detail": {
            "version":"0",
            "bucket":{"name":"granite-ses-inbound-emails"},
            "object":{
                "key":"p51f95lgdaa8rpcjp0q7loemss3a17avpnc48ug1",
                "size":4621,
                "etag":"39001b6af9f8d595ff89514dbcd2dc11",
                "sequencer":"00692356678DBC72C0"
            },
            "request-id":"3RXF9EH63QCHH4QF",
            "requester":"ses.amazonaws.com",
            "source-ip-address":"10.0.60.77",
            "reason":"PutObject"
        }
    }"#,
    )
    .unwrap()
}

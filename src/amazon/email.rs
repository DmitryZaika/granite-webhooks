use aws_config::meta::region::RegionProviderChain;
use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use aws_sdk_sesv2::{Client, Error, config::Region};

pub async fn send_message(to: &[&str], subject: &str, message: &str) -> Result<(), Error> {
    let region_provider = RegionProviderChain::first_try(Region::new("us-east-2"));
    let shared_config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&shared_config);

    let mut dest: Destination = Destination::builder().build();
    dest.to_addresses = Some(to.iter().map(|s| s.to_string()).collect());
    let subject_content = Content::builder()
        .data(subject)
        .charset("UTF-8")
        .build()
        .expect("building Content");
    let body_content = Content::builder()
        .data(message)
        .charset("UTF-8")
        .build()
        .expect("building Content");
    let body = Body::builder().text(body_content).build();

    let msg = Message::builder()
        .subject(subject_content)
        .body(body)
        .build();

    let email_content = EmailContent::builder().simple(msg).build();

    client
        .send_email()
        .from_email_address("noreply@granite-manager.com")
        .destination(dest)
        .content(email_content)
        .send()
        .await?;

    Ok(())
}

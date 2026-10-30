use tokio::task::JoinSet;

use crate::amazon::bucket::S3Bucket;
use crate::amazonses::parse_email::{Attachment, UploadedAttachment, attachment_object_key};

pub async fn upload_attachments<C>(
    client: C,
    message_id: &str,
    attachments: Vec<Attachment>,
) -> Result<Vec<UploadedAttachment>, Box<dyn std::error::Error>>
where
    C: S3Bucket + Send + Sync + 'static,
{
    let mut set = JoinSet::new();

    for (index, attachment) in attachments.into_iter().enumerate() {
        let final_client = client.clone(); // важно, если client не Copy
        let key = attachment_object_key(message_id, index, attachment.filename());
        set.spawn(async move { attachment.to_uploaded_attachment(&final_client, key).await });
    }

    let mut uploaded_attachments = Vec::new();

    while let Some(res) = set.join_next().await {
        uploaded_attachments.push(res?);
    }
    Ok(uploaded_attachments)
}

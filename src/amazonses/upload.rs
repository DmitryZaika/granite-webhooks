use tokio::task::JoinSet;

use crate::amazon::bucket::S3Bucket;
use crate::amazonses::parse_email::{Attachment, UploadedAttachment};

pub async fn upload_attachments<C>(
    client: C,
    attachments: Vec<Attachment>,
) -> Result<Vec<UploadedAttachment>, Box<dyn std::error::Error>>
where
    C: S3Bucket + Send + Sync + 'static,
{
    let mut set = JoinSet::new();

    for attachment in attachments {
        let final_client = client.clone(); // важно, если client не Copy
        set.spawn(async move { attachment.to_uploaded_attachment(&final_client).await });
    }

    let mut uploaded_attachments = Vec::new();

    while let Some(res) = set.join_next().await {
        uploaded_attachments.push(res?);
    }
    Ok(uploaded_attachments)
}

use std::path::Path;

use crate::models::SlackFile;
use crate::slack::{DownloadedMedia, DownloadedPreviewAsset, SlackApi, SlackError};

#[allow(dead_code)]
pub(crate) trait FileSlack {
    async fn download_preview_asset(&self, url: &str)
        -> Result<DownloadedPreviewAsset, SlackError>;
    async fn download_media(
        &self,
        url: &str,
        destination: &Path,
    ) -> Result<DownloadedMedia, SlackError>;
    async fn files(&self) -> Result<Vec<SlackFile>, SlackError>;
    async fn file(&self, file_id: &str) -> Result<SlackFile, SlackError>;
}

impl FileSlack for SlackApi {
    async fn download_preview_asset(
        &self,
        url: &str,
    ) -> Result<DownloadedPreviewAsset, SlackError> {
        self.download_preview_asset(url).await
    }

    async fn download_media(
        &self,
        url: &str,
        destination: &Path,
    ) -> Result<DownloadedMedia, SlackError> {
        self.download_media(url, destination).await
    }

    async fn files(&self) -> Result<Vec<SlackFile>, SlackError> {
        self.files().await
    }

    async fn file(&self, file_id: &str) -> Result<SlackFile, SlackError> {
        self.file(file_id).await
    }
}

#[allow(dead_code)]
pub(crate) struct FileService<'a, Slack> {
    slack: &'a Slack,
}

#[allow(dead_code)]
impl<'a, Slack> FileService<'a, Slack>
where
    Slack: FileSlack,
{
    pub(crate) fn new(slack: &'a Slack) -> Self {
        Self { slack }
    }

    pub(crate) async fn download_preview_asset(
        &self,
        url: &str,
    ) -> Result<DownloadedPreviewAsset, SlackError> {
        self.slack.download_preview_asset(url).await
    }

    pub(crate) async fn download_media(
        &self,
        url: &str,
        destination: &Path,
    ) -> Result<DownloadedMedia, SlackError> {
        self.slack.download_media(url, destination).await
    }

    pub(crate) async fn load_files(&self) -> Result<Vec<SlackFile>, SlackError> {
        self.slack.files().await
    }

    pub(crate) async fn load_file(&self, file_id: &str) -> Result<SlackFile, SlackError> {
        self.slack.file(file_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::slack::PreviewAssetMime;

    struct MockFileSlack;

    impl FileSlack for MockFileSlack {
        async fn download_preview_asset(
            &self,
            _url: &str,
        ) -> Result<DownloadedPreviewAsset, SlackError> {
            Ok(DownloadedPreviewAsset {
                bytes: vec![1, 2, 3],
                mime_type: PreviewAssetMime::Png,
            })
        }

        async fn download_media(
            &self,
            _url: &str,
            destination: &Path,
        ) -> Result<DownloadedMedia, SlackError> {
            Ok(DownloadedMedia {
                path: destination.to_path_buf(),
                mime_type: "image/jpeg".to_string(),
                size: 1024,
            })
        }

        async fn files(&self) -> Result<Vec<SlackFile>, SlackError> {
            Ok(vec![SlackFile {
                id: Some("F123".to_string()),
                name: Some("test.png".to_string()),
                title: Some("Test Image".to_string()),
                mimetype: Some("image/png".to_string()),
                size: Some(1024),
                ..SlackFile::default()
            }])
        }

        async fn file(&self, file_id: &str) -> Result<SlackFile, SlackError> {
            Ok(SlackFile {
                id: Some(file_id.to_string()),
                name: Some("test.png".to_string()),
                title: Some("Test Image".to_string()),
                mimetype: Some("image/png".to_string()),
                size: Some(1024),
                ..SlackFile::default()
            })
        }
    }

    #[test]
    fn file_service_loads_files_and_previews() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = MockFileSlack;
            let service = FileService::new(&slack);

            let files = service.load_files().await.unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].id.as_deref(), Some("F123"));

            let file = service.load_file("F999").await.unwrap();
            assert_eq!(file.id.as_deref(), Some("F999"));

            let asset = service
                .download_preview_asset("https://example.com/a.png")
                .await
                .unwrap();
            assert_eq!(asset.bytes, vec![1, 2, 3]);
            assert_eq!(asset.mime_type, PreviewAssetMime::Png);

            let media = service
                .download_media("https://example.com/a.jpg", Path::new("/tmp/test.jpg"))
                .await
                .unwrap();
            assert_eq!(media.path, PathBuf::from("/tmp/test.jpg"));
            assert_eq!(media.size, 1024);
        });
    }
}

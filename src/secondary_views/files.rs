//! File list rows.

use super::model::{is_http_url, FileOpenTarget, FileRow};
use crate::message_html::MessageHtmlContext;
use crate::models::SlackFile;

/// File rows, newest first.
pub(crate) fn file_rows(
    files: &[SlackFile],
    channel_title: &dyn Fn(&str) -> String,
    context: &MessageHtmlContext,
) -> Vec<FileRow> {
    let mut files = files.iter().collect::<Vec<_>>();
    files.sort_by_key(|file| std::cmp::Reverse(file_created(file)));
    files
        .into_iter()
        .map(|file| FileRow {
            title: file.display_title().to_string(),
            icon_name: file_icon_name(file),
            detail: file.detail_label(),
            owner: file
                .user
                .as_deref()
                .and_then(|user_id| context.user_names.get(user_id).cloned()),
            channel_title: file_channel_id(file).map(channel_title),
            created_ts: file_created(file).map(|seconds| seconds.to_string()),
            thumbnail_url: file
                .is_image()
                .then(|| file.preview_url())
                .flatten()
                .or_else(|| {
                    (file.supported_media_kind() == Some("video"))
                        .then(|| file.video_preview_url())
                        .flatten()
                })
                .map(ToString::to_string),
            open: file_open_target(file),
        })
        .collect()
}

fn file_created(file: &SlackFile) -> Option<u64> {
    file.created.or(file.timestamp)
}

fn file_channel_id(file: &SlackFile) -> Option<&str> {
    [&file.channels, &file.groups, &file.ims]
        .into_iter()
        .flatten()
        .flatten()
        .map(String::as_str)
        .find(|id| !id.trim().is_empty())
}

pub(crate) fn file_open_target(file: &SlackFile) -> FileOpenTarget {
    if let (Some(kind), Some(url)) = (file.supported_media_kind(), file.media_url()) {
        return FileOpenTarget::Media {
            url: url.to_string(),
            name: file.display_title().to_string(),
            video: kind == "video",
        };
    }
    match file.link_url().filter(|url| is_http_url(url)) {
        Some(url) => FileOpenTarget::External(url.to_string()),
        None => FileOpenTarget::Unavailable,
    }
}

pub(crate) fn file_icon_name(file: &SlackFile) -> &'static str {
    let mime = file.mimetype.as_deref().unwrap_or_default();
    let kind = file.filetype.as_deref().unwrap_or_default();
    if mime.starts_with("image/") {
        "image-x-generic-symbolic"
    } else if mime.starts_with("video/") {
        "video-x-generic-symbolic"
    } else if mime.starts_with("audio/") {
        "audio-x-generic-symbolic"
    } else if mime.contains("zip")
        || mime.contains("tar")
        || mime.contains("compressed")
        || mime.contains("archive")
    {
        "package-x-generic-symbolic"
    } else if mime.contains("spreadsheet") || matches!(kind, "csv" | "xlsx" | "xls" | "gsheet") {
        "x-office-spreadsheet-symbolic"
    } else if mime.contains("presentation") || matches!(kind, "pptx" | "ppt" | "gpres") {
        "x-office-presentation-symbolic"
    } else if mime == "application/pdf" || mime.contains("document") || kind == "pdf" {
        "x-office-document-symbolic"
    } else {
        "text-x-generic-symbolic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secondary_views::model::tests::{context, title};
    use crate::secondary_views::model::SecondaryRow;

    fn file(id: &str, created: u64, mimetype: &str) -> SlackFile {
        SlackFile {
            id: Some(id.to_string()),
            name: Some(format!("{id}.bin")),
            created: Some(created),
            mimetype: Some(mimetype.to_string()),
            user: Some("U1".to_string()),
            channels: Some(vec!["C1".to_string()]),
            url_private: Some(format!("https://files.slack.com/{id}")),
            permalink: Some(format!("https://example.slack.com/files/{id}")),
            size: Some(2048),
            ..SlackFile::default()
        }
    }

    #[test]
    fn file_rows_are_newest_first_with_owner_channel_and_targets() {
        let files = vec![
            file("F1", 10, "application/pdf"),
            file("F2", 30, "image/png"),
            file("F3", 20, "video/mp4"),
        ];
        let rows = file_rows(&files, &title, &context());

        let titles = rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["F2.bin", "F3.bin", "F1.bin"]);
        assert_eq!(rows[0].icon_name, "image-x-generic-symbolic");
        assert_eq!(rows[0].owner.as_deref(), Some("Alice"));
        assert_eq!(rows[0].channel_title.as_deref(), Some("#C1"));
        assert_eq!(rows[0].created_ts.as_deref(), Some("30"));
        assert!(matches!(
            rows[0].open,
            FileOpenTarget::Media { video: false, .. }
        ));
        assert!(matches!(
            rows[1].open,
            FileOpenTarget::Media { video: true, .. }
        ));
        assert_eq!(
            rows[2].open,
            FileOpenTarget::External("https://example.slack.com/files/F1".to_string())
        );
        assert_eq!(rows[2].icon_name, "x-office-document-symbolic");
        assert_eq!(rows[2].detail, "2.0 KB");
    }

    #[test]
    fn file_open_target_rejects_non_http_links() {
        let mut unsafe_file = file("F1", 1, "application/zip");
        unsafe_file.permalink = Some("file:///etc/passwd".to_string());
        unsafe_file.url_private = None;
        assert_eq!(file_open_target(&unsafe_file), FileOpenTarget::Unavailable);
        assert_eq!(file_icon_name(&unsafe_file), "package-x-generic-symbolic");
    }

    #[test]
    fn file_rows_have_accessible_labels() {
        let context = context();
        let file = SecondaryRow::File(
            file_rows(&[file("F1", 1, "text/plain")], &title, &context)[0].clone(),
        );
        assert_eq!(
            file.accessible_label(&context),
            "F1.bin, 2.0 KB, Alice, #C1"
        );
    }
}

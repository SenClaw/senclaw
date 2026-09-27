//! Media helpers shared by the channel adapters: MIME sniffing by magic
//! bytes, MIME from a file name, and the `data:` URL an attachment travels as.

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::types::MessageAttachment;

/// MIME type from the first bytes of a blob, for the formats a chat message
/// realistically carries. `None` means "unknown", never "not a file".
pub(crate) fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"PK\x03\x04") {
        Some("application/zip")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("video/mp4")
    } else {
        None
    }
}

/// MIME type from a file name's extension; `application/octet-stream` when
/// the extension says nothing.
pub(crate) fn mime_for_name(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// Build the attachment a downloaded blob travels as.
pub(crate) fn attachment_from_bytes(
    bytes: &[u8],
    mime: &str,
    name: Option<String>,
) -> MessageAttachment {
    MessageAttachment {
        data_url: format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
        mime_type: mime.to_string(),
        name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_signatures() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"hello"), None);
    }

    #[test]
    fn mime_from_extension_is_case_insensitive() {
        assert_eq!(mime_for_name("Report.PDF"), "application/pdf");
        assert_eq!(mime_for_name("photo.JPG"), "image/jpeg");
        assert_eq!(mime_for_name("blob"), "application/octet-stream");
    }

    #[test]
    fn attachment_is_a_data_url() {
        let a = attachment_from_bytes(b"abc", "text/plain", Some("a.txt".into()));
        assert_eq!(a.data_url, "data:text/plain;base64,YWJj");
        assert_eq!(a.mime_type, "text/plain");
        assert_eq!(a.name.as_deref(), Some("a.txt"));
    }
}

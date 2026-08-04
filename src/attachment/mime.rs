//! Mime-type/extension mapping and payload sniffing for attachments.
//!
//! Per K7, inbound spool filenames derive from the rumor's trusted
//! `file-type` mime tag, never from the sender-supplied `filename`, so
//! [`extension_for_mime`] never returns a value containing `.`, `/`, or
//! `..`. [`sniff_mime`] backs the outbound path, where the daemon
//! determines the mime type from the payload bytes rather than trusting a
//! caller-supplied content type.

/// Filename extension for a mime type, `"bin"` when unrecognized. Covers the
/// map `pacto-app`'s `extension_from_mime` covers.
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "text/plain" => "txt",
        "application/json" => "json",
        _ => "bin",
    }
}

/// Sniff a mime type from payload bytes, `"application/octet-stream"` when
/// unknown.
pub fn sniff_mime(bytes: &[u8]) -> &'static str {
    infer::get(bytes)
        .map(|kind| kind.mime_type())
        .unwrap_or("application/octet-stream")
}

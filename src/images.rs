use anyhow::{Context, Result};
use std::path::PathBuf;
use uuid::Uuid;

const LOCAL_PREFIX: &str = "memonotes-image://";
const REMOTE_PREFIX: &str = "/file/attachments/";

pub fn image_directory() -> Result<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("could not locate the application data directory")?;
    Ok(data_home.join("memonotes/images"))
}

// Local files are named `<uuid>.<ext>`; the UUID doubles as the Memos attachment ID.
pub fn attachment_id(name: &str) -> Option<&str> {
    let (stem, extension) = name.split_once('.')?;
    let valid_extension = !extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric());
    (valid_extension && stem.len() == 36 && Uuid::parse_str(stem).is_ok()).then_some(stem)
}

pub fn mime_type(name: &str) -> &'static str {
    match name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("tif" | "tiff") => "image/tiff",
        Some("avif") => "image/avif",
        Some("heic") => "image/heic",
        _ => "application/octet-stream",
    }
}

// Byte range of each valid reference and the local file name it points at.
fn references(content: &str, prefix: &str, remote: bool) -> Vec<(std::ops::Range<usize>, String)> {
    let is_name_byte =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_');
    let mut found = Vec::new();
    let mut search_from = 0;
    while let Some(offset) = content[search_from..].find(prefix) {
        let start = search_from + offset;
        let mut cursor = start + prefix.len();
        search_from = cursor;
        let bytes = content.as_bytes();
        let id = if remote {
            let id_end = cursor
                + bytes[cursor..]
                    .iter()
                    .take_while(|byte| is_name_byte(**byte))
                    .count();
            if bytes.get(id_end) != Some(&b'/') {
                continue;
            }
            let id = &content[cursor..id_end];
            cursor = id_end + 1;
            Some(id)
        } else {
            None
        };
        let end = cursor
            + bytes[cursor..]
                .iter()
                .take_while(|byte| is_name_byte(**byte))
                .count();
        let name = &content[cursor..end];
        if attachment_id(name).is_some_and(|stem| id.is_none_or(|id| id == stem)) {
            found.push((start..end, name.to_owned()));
            search_from = end;
        }
    }
    found
}

fn rewrite(content: &str, prefix: &str, remote: bool) -> String {
    let mut output = String::with_capacity(content.len());
    let mut copied = 0;
    for (range, name) in references(content, prefix, remote) {
        output.push_str(&content[copied..range.start]);
        if remote {
            output.push_str(LOCAL_PREFIX);
            output.push_str(&name);
        } else {
            output.push_str(REMOTE_PREFIX);
            output.push_str(attachment_id(&name).unwrap_or_default());
            output.push('/');
            output.push_str(&name);
        }
        copied = range.end;
    }
    output.push_str(&content[copied..]);
    output
}

fn unique_names(references: Vec<(std::ops::Range<usize>, String)>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, name) in references {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

pub fn local_images(content: &str) -> Vec<String> {
    unique_names(references(content, LOCAL_PREFIX, false))
}

pub fn remote_images(content: &str) -> Vec<String> {
    unique_names(references(content, REMOTE_PREFIX, true))
}

pub fn to_remote_content(content: &str) -> String {
    rewrite(content, LOCAL_PREFIX, false)
}

pub fn to_local_content(content: &str) -> String {
    rewrite(content, REMOTE_PREFIX, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "550e8400-e29b-41d4-a716-446655440000.png";

    #[test]
    fn local_references_map_to_attachment_urls_and_back() {
        let local = format!(
            "# Titel\n\n![foto](memonotes-image://{NAME}#w=320) en ![](memonotes-image://{NAME})"
        );
        let remote = to_remote_content(&local);
        assert_eq!(
            remote,
            format!(
                "# Titel\n\n![foto](/file/attachments/{0}/{1}#w=320) en ![](/file/attachments/{0}/{1})",
                &NAME[..36],
                NAME
            )
        );
        assert_eq!(local_images(&local), [NAME]);
        assert_eq!(remote_images(&remote), [NAME]);
        assert_eq!(to_local_content(&remote), local);
    }

    #[test]
    fn foreign_or_malformed_references_stay_untouched() {
        let content = format!(
            "![a](memonotes-image://../x.png) ![b](/file/attachments/abc/photo.png) \
             ![c](/file/attachments/other/{NAME}) ![d](memonotes-image://notauuid.png)"
        );
        assert_eq!(to_remote_content(&content), content);
        assert_eq!(to_local_content(&content), content);
        assert!(local_images(&content).is_empty());
        assert!(remote_images(&content).is_empty());
    }

    #[test]
    fn mime_types_follow_the_extension() {
        assert_eq!(mime_type("a.PNG"), "image/png");
        assert_eq!(mime_type("a.jpeg"), "image/jpeg");
        assert_eq!(mime_type("a.xyz"), "application/octet-stream");
    }
}

use anyhow::{Context, Result};
use std::ops::Range;
use std::path::PathBuf;
use uuid::Uuid;

const LOCAL_PREFIX: &str = "memonotes-image://";
const REMOTE_PREFIX: &str = "/file/attachments/";
const WIDTH_FRAGMENT: &str = "#w=";
const FOREIGN_PREFIX: &str = "memos_";
const MAX_FOREIGN_FILE_BYTES: usize = 80;
const MAX_ATTACHMENT_ID_BYTES: usize = 64;

pub fn image_directory() -> Result<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("could not locate the application data directory")?;
    Ok(data_home.join("memonotes/images"))
}

// An image reference in note content: the Memos attachment it stands for and its local file.
// `range` covers the URL (and the local `#w=` fragment), `alt` the Markdown alt text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub range: Range<usize>,
    pub alt: Option<Range<usize>>,
    pub width: Option<u32>,
    pub id: String,
    pub file: String,
    pub local: String,
}

// Files made by Memonotes are named `<uuid>.<ext>`; the UUID doubles as the attachment ID.
pub fn attachment_id(name: &str) -> Option<&str> {
    let (stem, extension) = name.split_once('.')?;
    let valid_extension = !extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric());
    (valid_extension && stem.len() == 36 && Uuid::parse_str(stem).is_ok()).then_some(stem)
}

pub fn is_uploadable(image: &ImageRef) -> bool {
    attachment_id(&image.local).is_some()
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

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
}

fn is_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

// The file segment of a remote URL is kept verbatim, so it may stay percent-encoded.
fn is_file_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'~' | b'-')
}

fn run(content: &str, start: usize, accept: fn(u8) -> bool) -> usize {
    content.as_bytes()[start..]
        .iter()
        .take_while(|byte| accept(**byte))
        .count()
}

// Local name for an attachment: its own name for Memonotes uploads, an encoded one otherwise.
// An empty `file` stands for a `/file/attachments/<id>` URL without a filename.
fn local_name(id: &str, file: &str) -> Option<String> {
    if attachment_id(file) == Some(id) {
        return Some(file.to_owned());
    }
    let valid = !id.is_empty()
        && id.len() <= MAX_ATTACHMENT_ID_BYTES
        && id.bytes().all(is_id_byte)
        && file.len() <= MAX_FOREIGN_FILE_BYTES
        && file.bytes().all(is_file_byte);
    valid.then(|| {
        if file.is_empty() {
            return format!("{FOREIGN_PREFIX}{id}");
        }
        let hex: String = file.bytes().map(|byte| format!("{byte:02x}")).collect();
        format!("{FOREIGN_PREFIX}{id}_{hex}")
    })
}

fn parse_local(name: &str) -> Option<(String, String)> {
    if let Some(id) = attachment_id(name) {
        return Some((id.to_owned(), name.to_owned()));
    }
    let rest = name.strip_prefix(FOREIGN_PREFIX)?;
    let (id, hex) = rest.split_once('_').unwrap_or((rest, ""));
    if hex.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|index| {
            hex.get(index..index + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    let file = String::from_utf8(bytes).ok()?;
    (local_name(id, &file).as_deref() == Some(name)).then(|| (id.to_owned(), file))
}

// The alt text of the Markdown image whose destination starts at `url_start`.
fn alt_range(content: &str, url_start: usize) -> Option<Range<usize>> {
    let before = content[..url_start].strip_suffix("](")?;
    let open = before.rfind('[')?;
    (open > 0 && before.as_bytes()[open - 1] == b'!').then(|| open + 1..before.len())
}

// Memos rejects managed image URLs with a fragment, so the width travels as `alt|<width>`.
fn split_alt_width(alt: &str) -> (&str, Option<u32>) {
    match alt.rsplit_once('|') {
        Some((text, digits))
            if (1..=5).contains(&digits.len())
                && digits.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            match digits.parse().ok().filter(|width| *width > 0) {
                Some(width) => (text, Some(width)),
                None => (alt, None),
            }
        }
        _ => (alt, None),
    }
}

fn local_refs(content: &str) -> Vec<ImageRef> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = content[from..].find(LOCAL_PREFIX) {
        let start = from + offset;
        let name_start = start + LOCAL_PREFIX.len();
        let mut end = name_start + run(content, name_start, is_name_byte);
        from = name_start;
        let name = &content[name_start..end];
        if let Some((id, file)) = parse_local(name) {
            let alt = alt_range(content, start);
            let mut width = None;
            if alt.is_some()
                && let Some(rest) = content[end..].strip_prefix(WIDTH_FRAGMENT)
            {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if let Some(parsed) = rest[..digits].parse().ok().filter(|width| *width > 0) {
                    width = Some(parsed);
                    end += WIDTH_FRAGMENT.len() + digits;
                }
            }
            found.push(ImageRef {
                range: start..end,
                alt,
                width,
                id,
                file,
                local: name.to_owned(),
            });
            from = end;
        }
    }
    found
}

fn remote_refs(content: &str, origin: &str) -> Vec<ImageRef> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = content[from..].find(REMOTE_PREFIX) {
        let position = from + offset;
        let id_start = position + REMOTE_PREFIX.len();
        from = id_start;
        let id_end = id_start + run(content, id_start, is_id_byte);
        if id_end == id_start {
            continue;
        }
        let mut end = id_end;
        let mut file = "";
        if content.as_bytes().get(id_end) == Some(&b'/') {
            let file_start = id_end + 1;
            let file_end = file_start + run(content, file_start, is_file_byte);
            if file_end > file_start {
                file = &content[file_start..file_end];
                end = file_end;
            }
        }
        let id = &content[id_start..id_end];
        let start = if !origin.is_empty() && content[..position].ends_with(origin) {
            position - origin.len()
        } else {
            position
        };
        // Only Markdown images are mapped; a plain link to an attachment stays a link.
        let Some(alt) = alt_range(content, start) else {
            continue;
        };
        if (file.is_empty() || mime_type(file).starts_with("image/"))
            && let Some(local) = local_name(id, file)
        {
            let width = split_alt_width(&content[alt.clone()]).1;
            found.push(ImageRef {
                range: start..end,
                alt: Some(alt),
                width,
                id: id.to_owned(),
                file: file.to_owned(),
                local,
            });
            from = end;
        }
    }
    found
}

fn apply(content: &str, edits: Vec<(Range<usize>, String)>) -> String {
    let mut output = String::with_capacity(content.len());
    let mut copied = 0;
    for (range, replacement) in edits {
        output.push_str(&content[copied..range.start]);
        output.push_str(&replacement);
        copied = range.end;
    }
    output.push_str(&content[copied..]);
    output
}

fn unique(images: Vec<ImageRef>) -> Vec<ImageRef> {
    let mut unique: Vec<ImageRef> = Vec::new();
    for image in images {
        if !unique.iter().any(|known| known.local == image.local) {
            unique.push(image);
        }
    }
    unique
}

pub fn local_images(content: &str) -> Vec<ImageRef> {
    unique(local_refs(content))
}

pub fn remote_images(content: &str, origin: &str) -> Vec<ImageRef> {
    unique(remote_refs(content, origin))
}

pub fn to_remote_content(content: &str) -> String {
    let mut edits = Vec::new();
    for image in local_refs(content) {
        if let (Some(alt), Some(width)) = (&image.alt, image.width) {
            edits.push((alt.clone(), format!("{}|{width}", &content[alt.clone()])));
        }
        let url = if image.file.is_empty() {
            format!("{REMOTE_PREFIX}{}", image.id)
        } else {
            format!("{REMOTE_PREFIX}{}/{}", image.id, image.file)
        };
        edits.push((image.range.clone(), url));
    }
    apply(content, edits)
}

// `origin` is the server's base URL, so absolute attachment URLs are recognised too.
pub fn to_local_content(content: &str, origin: &str) -> String {
    let mut edits = Vec::new();
    for image in remote_refs(content, origin) {
        let mut url = format!("{LOCAL_PREFIX}{}", image.local);
        if let (Some(alt), Some(width)) = (&image.alt, image.width) {
            edits.push((
                alt.clone(),
                split_alt_width(&content[alt.clone()]).0.to_owned(),
            ));
            url.push_str(&format!("{WIDTH_FRAGMENT}{width}"));
        }
        edits.push((image.range.clone(), url));
    }
    apply(content, edits)
}

// Earlier builds stored the remote `alt|width` form locally; fold it back into `#w=`.
pub fn normalize_local_content(content: &str) -> String {
    let mut edits = Vec::new();
    for image in local_refs(content) {
        let Some(alt) = &image.alt else {
            continue;
        };
        let (text, width) = split_alt_width(&content[alt.clone()]);
        if image.width.is_none()
            && let Some(width) = width
        {
            edits.push((alt.clone(), text.to_owned()));
            edits.push((
                image.range.clone(),
                format!("{LOCAL_PREFIX}{}{WIDTH_FRAGMENT}{width}", image.local),
            ));
        }
    }
    apply(content, edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_alt_widths_are_folded_into_the_fragment() {
        let legacy = "![it|564](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g) ![a|b](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g#w=5)";
        assert_eq!(
            normalize_local_content(legacy),
            "![it](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g#w=564) ![a|b](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g#w=5)"
        );
    }

    const NAME: &str = "550e8400-e29b-41d4-a716-446655440000.png";
    const ORIGIN: &str = "https://memos.example/base";

    #[test]
    fn local_references_map_to_attachment_urls_and_back() {
        let local = format!(
            "# Titel\n\n![foto](memonotes-image://{NAME}#w=320) en ![](memonotes-image://{NAME})"
        );
        let remote = to_remote_content(&local);
        assert_eq!(
            remote,
            format!(
                "# Titel\n\n![foto|320](/file/attachments/{0}/{1}) en ![](/file/attachments/{0}/{1})",
                &NAME[..36],
                NAME
            )
        );
        let images = local_images(&local);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].local, NAME);
        assert!(is_uploadable(&images[0]));
        assert_eq!(remote_images(&remote, ORIGIN)[0].local, NAME);
        assert_eq!(to_local_content(&remote, ORIGIN), local);
    }

    #[test]
    fn width_never_ends_up_in_a_url_fragment() {
        // Memos rejects managed image URLs with a fragment, query or percent-encoding.
        let local = "![it](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g#w=564)";
        let remote = to_remote_content(local);
        assert_eq!(
            remote,
            "![it|564](/file/attachments/Jd54omtoGCZew7ZpEiqu7g)"
        );
        assert_eq!(to_local_content(&remote, ORIGIN), local);
    }

    #[test]
    fn alt_text_with_a_pipe_survives_the_round_trip() {
        let remote =
            "![a|b](/file/attachments/abc123/p.png) ![c|12](/file/attachments/abc123/p.png)";
        let local = to_local_content(remote, ORIGIN);
        assert_eq!(
            local,
            "![a|b](memonotes-image://memos_abc123_702e706e67) \
             ![c](memonotes-image://memos_abc123_702e706e67#w=12)"
        );
        assert_eq!(to_remote_content(&local), remote);
    }

    #[test]
    fn web_attachments_round_trip_through_encoded_local_names() {
        let remote = "![a|100](/file/attachments/abc123/photo.png) en \
                      ![b](/file/attachments/Xy-9/IMG_1.JPG)";
        let local = to_local_content(remote, ORIGIN);
        assert!(local.starts_with("![a](memonotes-image://memos_abc123_70686f746f2e706e67#w=100)"));
        assert!(!local.contains("/file/attachments"));
        assert_eq!(to_remote_content(&local), remote);
        let images = local_images(&local);
        assert_eq!(images[0].id, "abc123");
        assert_eq!(images[0].file, "photo.png");
        assert!(!is_uploadable(&images[0]));
    }

    #[test]
    fn attachment_urls_without_a_filename_round_trip() {
        let remote = "![it](/file/attachments/Jd54omtoGCZew7ZpEiqu7g) [doc](/file/attachments/abc)";
        let local = to_local_content(remote, ORIGIN);
        assert_eq!(
            local,
            "![it](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g) [doc](/file/attachments/abc)"
        );
        assert_eq!(to_remote_content(&local), remote);
        let image = &local_images(&local)[0];
        assert_eq!(
            (image.id.as_str(), image.file.as_str()),
            ("Jd54omtoGCZew7ZpEiqu7g", "")
        );
    }

    #[test]
    fn absolute_urls_on_the_server_origin_become_local() {
        let remote = format!("![a]({ORIGIN}/file/attachments/abc123/p.jpg)");
        assert_eq!(
            to_local_content(&remote, ORIGIN),
            "![a](memonotes-image://memos_abc123_702e6a7067)"
        );
        assert_eq!(to_local_content(&remote, "https://other.example"), remote);
    }

    #[test]
    fn links_and_malformed_references_stay_untouched() {
        let content = format!(
            "![a](memonotes-image://../x.png) [doc](/file/attachments/abc/doc.png) \
             ![v](/file/attachments/abc/clip.mp4) ![n](/file/attachments/abc/noext) \
             ![d](memonotes-image://notauuid.png) ![e](memonotes-image://memos_abc_zz)"
        );
        assert_eq!(to_remote_content(&content), content);
        assert_eq!(to_local_content(&content, ORIGIN), content);
        assert!(local_images(&content).is_empty());
        assert!(remote_images(&content, ORIGIN).is_empty());
    }

    #[test]
    fn mime_types_follow_the_extension() {
        assert_eq!(mime_type("a.PNG"), "image/png");
        assert_eq!(mime_type("a.jpeg"), "image/jpeg");
        assert_eq!(mime_type("a.xyz"), "application/octet-stream");
    }
}

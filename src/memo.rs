pub type TimestampMillis = i64;

pub fn editor_parts(content: &str) -> (&str, &str) {
    let (first, body) = content.split_once('\n').unwrap_or((content, ""));
    let first = first.trim();
    let text = first.trim_start_matches('#');
    let count = first.len() - text.len();
    let title =
        if (1..=6).contains(&count) && (text.is_empty() || text.starts_with(char::is_whitespace)) {
            text.trim()
        } else {
            first
        };
    (title, body)
}

pub fn editor_content(title: &str, body: &str) -> String {
    let title = title.trim();
    if title.is_empty() && body.is_empty() {
        return String::new();
    }
    let heading = if title.is_empty() {
        String::new()
    } else {
        format!("# {title}")
    };
    if body.is_empty() {
        heading
    } else {
        format!("{heading}\n{body}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memo {
    pub id: String,
    pub content: String,
    pub title: Option<String>,
    pub folder_id: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub is_shared: bool,
    pub updated_at: Option<TimestampMillis>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoFolder {
    pub id: String,
    pub name: String,
    pub is_shared: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_title_round_trips_as_first_line_heading() {
        for (content, title, body) in [
            ("# Titel\n\nTekst", "Titel", "\nTekst"),
            ("Eerste regel\nTekst", "Eerste regel", "Tekst"),
            ("## Titel\nTekst", "Titel", "Tekst"),
            ("#hashtag\nTekst", "#hashtag", "Tekst"),
            ("", "", ""),
            ("\nTekst", "", "Tekst"),
        ] {
            assert_eq!(editor_parts(content), (title, body));
            let saved = editor_content(title, body);
            assert_eq!(editor_parts(&saved), (title, body));
            assert_eq!(
                editor_content(editor_parts(&saved).0, editor_parts(&saved).1),
                saved
            );
            if !title.is_empty() {
                assert!(saved.starts_with(&format!("# {title}")));
            }
        }
    }
}

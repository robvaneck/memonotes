use anyhow::{Result, bail, ensure};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorDocument {
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedMarkdown {
    pub text: String,
    pub spans: Vec<MarkupSpan>,
    pub headings: Vec<HeadingSpan>,
    pub links: Vec<LinkSpan>,
    pub images: Vec<ImageSpan>,
    pub tables: Vec<TableSpan>,
    pub rules: Vec<Range<usize>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAlignment {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// A top-level Markdown table; the first row is the header and cells keep their inline Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSpan {
    pub range: Range<usize>,
    pub alignments: Vec<TableAlignment>,
    pub rows: Vec<Vec<String>>,
}

fn markdown_options() -> Options {
    Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES
}

pub fn byte_to_char_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0; text.len() + 1];
    let mut char_offset = 0;
    for (byte_offset, character) in text.char_indices() {
        let end_offset = byte_offset + character.len_utf8();
        offsets[byte_offset..end_offset].fill(char_offset);
        char_offset += 1;
        offsets[end_offset] = char_offset;
    }
    offsets
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadingSpan {
    pub range: Range<usize>,
    pub level: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    pub range: Range<usize>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSpan {
    pub range: Range<usize>,
    pub url: String,
    pub alt: String,
}

pub fn render_markdown_source(markdown: &str) -> RenderedMarkdown {
    let source_tables = markdown_table_ranges(markdown);
    let in_table = |range: &Range<usize>| {
        source_tables
            .iter()
            .any(|table| table.range.start < range.end && range.start < table.range.end)
    };
    let mut source_spans = markdown_style_spans(markdown);
    source_spans.retain(|span| !in_table(&span.range));
    let (source_headings, heading_markers) = markdown_heading_ranges(markdown);
    let mut hidden_ranges: Vec<_> = source_spans
        .iter()
        .filter(|span| span.hidden)
        .map(|span| span.range.clone())
        .collect();
    hidden_ranges.extend(heading_markers);
    let mut source_links = markdown_link_ranges(markdown);
    source_links.retain(|link| !in_table(&link.content));
    hidden_ranges.extend(source_links.iter().flat_map(|link| link.hidden.clone()));
    let mut source_images = markdown_image_ranges(markdown);
    source_images.retain(|image| !in_table(&image.range));
    hidden_ranges.extend(source_images.iter().map(|image| image.range.clone()));
    hidden_ranges.extend(source_tables.iter().map(|table| table.range.clone()));
    let source_rules = markdown_rule_ranges(markdown);
    hidden_ranges.extend(source_rules.iter().cloned());
    hidden_ranges.sort_unstable_by_key(|range| range.start);
    let mut merged_hidden_ranges: Vec<Range<usize>> = Vec::with_capacity(hidden_ranges.len());
    for range in hidden_ranges {
        if let Some(previous) = merged_hidden_ranges.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged_hidden_ranges.push(range);
        }
    }

    let mut output = String::with_capacity(markdown.len());
    let mut output_offsets = vec![0; markdown.len() + 1];
    let mut hidden_range_index = 0;
    let mut image_index = 0;
    let mut table_index = 0;
    let mut rule_index = 0;

    for (byte_offset, character) in markdown.char_indices() {
        let end_offset = byte_offset + character.len_utf8();
        while merged_hidden_ranges
            .get(hidden_range_index)
            .is_some_and(|range| range.end <= byte_offset)
        {
            hidden_range_index += 1;
        }
        let hidden = merged_hidden_ranges
            .get(hidden_range_index)
            .is_some_and(|range| range.start <= byte_offset && byte_offset < range.end);
        output_offsets[byte_offset] = output.len();
        if source_images
            .get(image_index)
            .is_some_and(|image| image.range.start == byte_offset)
        {
            output.push('\u{fffc}');
            image_index += 1;
        }
        if source_tables
            .get(table_index)
            .is_some_and(|table| table.range.start == byte_offset)
        {
            output.push('\u{fffc}');
            table_index += 1;
        }
        if source_rules
            .get(rule_index)
            .is_some_and(|rule| rule.start == byte_offset)
        {
            output.push('\u{fffc}');
            rule_index += 1;
        }
        if !hidden {
            output.push(character);
        }
        for offset in byte_offset + 1..=end_offset {
            output_offsets[offset] = output.len();
        }
    }

    let spans = source_spans
        .into_iter()
        .filter(|span| !span.hidden)
        .filter_map(|span| {
            let start = output_offsets[span.range.start];
            let end = output_offsets[span.range.end];
            (start < end).then_some(MarkupSpan {
                range: start..end,
                mark: span.mark,
                hidden: false,
            })
        })
        .collect();
    let headings = source_headings
        .into_iter()
        .filter_map(|(range, level)| {
            let start = output_offsets[range.start];
            let end = output_offsets[range.end];
            (start < end).then_some(HeadingSpan {
                range: start..end,
                level,
            })
        })
        .collect();

    let links = source_links
        .into_iter()
        .filter_map(|link| {
            let start = output_offsets[link.content.start];
            let end = output_offsets[link.content.end];
            (start < end).then_some(LinkSpan {
                range: start..end,
                url: link.url,
            })
        })
        .collect();
    let images = source_images
        .into_iter()
        .map(|image| {
            let start = output_offsets[image.range.start];
            ImageSpan {
                range: start..start + '\u{fffc}'.len_utf8(),
                url: image.url,
                alt: image.alt,
            }
        })
        .collect();

    let tables = source_tables
        .into_iter()
        .map(|table| {
            let start = output_offsets[table.range.start];
            TableSpan {
                range: start..start + '\u{fffc}'.len_utf8(),
                alignments: table.alignments,
                rows: table.rows,
            }
        })
        .collect();

    let rules = source_rules
        .into_iter()
        .map(|rule| {
            let start = output_offsets[rule.start];
            start..start + '\u{fffc}'.len_utf8()
        })
        .collect();

    RenderedMarkdown {
        text: output,
        spans,
        headings,
        links,
        images,
        tables,
        rules,
    }
}

// Rules nested in lists or quotes stay plain text.
fn markdown_rule_ranges(markdown: &str) -> Vec<Range<usize>> {
    let mut rules = Vec::new();
    let mut container_depth = 0_usize;
    for (event, range) in Parser::new_ext(markdown, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => container_depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => {
                container_depth = container_depth.saturating_sub(1)
            }
            Event::Rule if container_depth == 0 => {
                let end = range.start + markdown[range.clone()].trim_end_matches(['\n', '\r']).len();
                rules.push(range.start..end);
            }
            _ => {}
        }
    }
    rules
}

/// A line of three or more `-`, `*` or `_` (spaces allowed) is a thematic break, not a list item.
pub fn is_thematic_break(line: &str) -> bool {
    let mut markers = line.chars().filter(|character| !matches!(character, ' ' | '\t'));
    let Some(first) = markers.next().filter(|first| matches!(first, '-' | '*' | '_')) else {
        return false;
    };
    let mut count = 1;
    for marker in markers {
        if marker != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

struct SourceTable {
    range: Range<usize>,
    alignments: Vec<TableAlignment>,
    rows: Vec<Vec<String>>,
}

// Tables nested in lists or quotes stay plain text.
fn markdown_table_ranges(markdown: &str) -> Vec<SourceTable> {
    let mut tables = Vec::new();
    let mut container_depth = 0_usize;
    let mut open: Option<SourceTable> = None;
    let mut row: Vec<String> = Vec::new();

    for (event, range) in Parser::new_ext(markdown, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => container_depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => {
                container_depth = container_depth.saturating_sub(1)
            }
            Event::Start(Tag::Table(alignments)) if container_depth == 0 => {
                let source = &markdown[range.clone()];
                let end = range.start + source.trim_end_matches(['\n', '\r']).len();
                open = Some(SourceTable {
                    range: range.start..end,
                    alignments: alignments
                        .iter()
                        .map(|alignment| match alignment {
                            pulldown_cmark::Alignment::None => TableAlignment::None,
                            pulldown_cmark::Alignment::Left => TableAlignment::Left,
                            pulldown_cmark::Alignment::Center => TableAlignment::Center,
                            pulldown_cmark::Alignment::Right => TableAlignment::Right,
                        })
                        .collect(),
                    rows: Vec::new(),
                });
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableCell) if open.is_some() => {
                row.push(markdown[range].trim().to_owned());
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                if let Some(table) = open.as_mut() {
                    table.rows.push(std::mem::take(&mut row));
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(mut table) = open.take() {
                    let columns = table.alignments.len().max(1);
                    table.alignments.resize(columns, TableAlignment::None);
                    for row in &mut table.rows {
                        row.resize(columns, String::new());
                    }
                    tables.push(table);
                }
            }
            _ => {}
        }
    }
    tables
}

/// Escapes a cell for a pipe table: unescaped pipes and line breaks would break the row.
fn escape_table_cell(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_backslash = false;
    for character in text.chars() {
        match character {
            '\n' | '\r' | '\u{fffc}' => output.push(' '),
            '|' if !previous_backslash => output.push_str("\\|"),
            other => output.push(other),
        }
        previous_backslash = character == '\\' && !previous_backslash;
    }
    output.trim().to_owned()
}

/// Parses spreadsheet clipboard text: tab-separated cells, with quoted cells for embedded breaks.
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut at_field_start = true;
    let mut in_quotes = false;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if in_quotes {
            if character != '"' {
                field.push(character);
            } else if characters.peek() == Some(&'"') {
                field.push('"');
                characters.next();
            } else {
                in_quotes = false;
            }
            continue;
        }
        match character {
            '"' if at_field_start => {
                in_quotes = true;
                at_field_start = false;
            }
            '\t' => {
                row.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\r' if characters.peek() == Some(&'\n') => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_field_start = true;
            }
            other => {
                field.push(other);
                at_field_start = false;
            }
        }
    }
    row.push(field);
    rows.push(row);
    rows
}

/// Returns the cells when clipboard text looks like a copied spreadsheet range, not ordinary text.
pub fn pasted_table_rows(text: &str) -> Option<Vec<Vec<String>>> {
    let mut rows = parse_tsv(text);
    let columns = rows.first()?.len();
    if columns < 2 || rows.iter().any(|row| row.len() != columns) {
        return None;
    }
    // A lone indented line also contains a tab.
    if rows.len() == 1 && rows[0].iter().any(|cell| cell.trim().is_empty()) {
        return None;
    }
    for cell in rows.iter_mut().flatten() {
        *cell = cell.replace(['\r', '\n', '\t'], " ").trim().to_owned();
    }
    Some(rows)
}

pub fn table_markdown(alignments: &[TableAlignment], rows: &[Vec<String>]) -> String {
    let columns = alignments
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    let format_row = |row: &[String]| {
        let cells: Vec<_> = (0..columns)
            .map(|column| escape_table_cell(row.get(column).map_or("", String::as_str)))
            .collect();
        format!("| {} |", cells.join(" | "))
    };
    let delimiter: Vec<_> = (0..columns)
        .map(|column| match alignments.get(column).copied().unwrap_or_default() {
            TableAlignment::None => "---",
            TableAlignment::Left => ":---",
            TableAlignment::Center => ":---:",
            TableAlignment::Right => "---:",
        })
        .collect();
    let mut lines = vec![format_row(rows.first().map_or(&[][..], Vec::as_slice))];
    lines.push(format!("| {} |", delimiter.join(" | ")));
    lines.extend(rows.iter().skip(1).map(|row| format_row(row)));
    lines.join("\n")
}

struct SourceImage {
    range: Range<usize>,
    url: String,
    alt: String,
}

fn markdown_image_ranges(markdown: &str) -> Vec<SourceImage> {
    let options = markdown_options();
    let mut images = Vec::new();
    let mut open: Option<SourceImage> = None;

    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                open = Some(SourceImage {
                    range: range.start..range.end,
                    url: dest_url.to_string(),
                    alt: String::new(),
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(image) = open.as_mut() {
                    image.alt.push_str(&text);
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some(mut image) = open.take() {
                    image.range.end = range.end;
                    images.push(image);
                }
            }
            _ => {}
        }
    }
    images
}

struct SourceLink {
    content: Range<usize>,
    hidden: [Range<usize>; 2],
    url: String,
}

fn markdown_link_ranges(markdown: &str) -> Vec<SourceLink> {
    let options = markdown_options();
    let mut links = Vec::new();
    // (url, first inner start, last inner end)
    let mut open: Option<(String, usize, usize)> = None;

    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                open = Some((dest_url.to_string(), usize::MAX, 0));
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, first, last)) = open.take()
                    && first < last
                {
                    links.push(SourceLink {
                        content: first..last,
                        hidden: [range.start..first, last..range.end],
                        url,
                    });
                }
            }
            _ => {
                if let Some((_, first, last)) = open.as_mut() {
                    *first = (*first).min(range.start);
                    *last = (*last).max(range.end);
                }
            }
        }
    }
    links
}

/// Returns an absolute http(s)/mailto URL without whitespace, or `None` if unsupported.
pub fn normalize_link_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let scheme_end = input.find(':').filter(|&index| {
        let scheme = &input[..index];
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    let url = match scheme_end {
        Some(_) => input.to_owned(),
        None if input.contains('@') && !input.contains('/') => format!("mailto:{input}"),
        None => format!("https://{input}"),
    };
    is_openable_link(&url).then(|| url.split_whitespace().collect::<Vec<_>>().join("%20"))
}

pub fn is_openable_link(url: &str) -> bool {
    let scheme = url
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(scheme.as_str(), "http" | "https" | "mailto") && url.len() > scheme.len() + 1
}

/// Wraps an already serialized label as an inline Markdown link.
pub fn markdown_link(label: &str, url: &str) -> String {
    let mut output = String::from("[");
    let mut previous_backslash = false;
    for character in label.chars() {
        if matches!(character, '[' | ']') && !previous_backslash {
            output.push('\\');
        }
        previous_backslash = character == '\\' && !previous_backslash;
        output.push(character);
    }
    output.push_str("](");
    escape_markdown_destination(url, &mut output);
    output.push(')');
    output
}

fn markdown_heading_ranges(markdown: &str) -> (Vec<(Range<usize>, u8)>, Vec<Range<usize>>) {
    let mut headings = Vec::new();
    let mut markers = Vec::new();
    let mut fenced_code = false;
    let mut offset = 0;

    for line_with_ending in markdown.split_inclusive('\n') {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let trimmed = line.trim_start_matches(' ');
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced_code = !fenced_code;
        } else if !fenced_code {
            let indentation = line.len() - trimmed.len();
            let marker_length = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            if indentation <= 3
                && (1..=6).contains(&marker_length)
                && trimmed
                    .as_bytes()
                    .get(marker_length)
                    .is_some_and(u8::is_ascii_whitespace)
            {
                let content_start = offset + indentation + marker_length + 1;
                let content_end = offset + line.len();
                markers.push(offset + indentation..content_start);
                headings.push((content_start..content_end, marker_length as u8));
            }
        }
        offset += line_with_ending.len();
    }

    (headings, markers)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Paragraph {
        content: Vec<Inline>,
    },
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    List {
        ordered: bool,
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Quote {
        blocks: Vec<Block>,
    },
    ThematicBreak,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inline {
    Text { text: String, marks: Vec<Mark> },
    Link { url: String, content: Vec<Inline> },
    Image { asset_id: String, alt: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Code,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkupSpan {
    pub range: Range<usize>,
    pub mark: Option<Mark>,
    pub hidden: bool,
}

pub fn markdown_preview_text(markdown: &str) -> String {
    let options = markdown_options();
    let mut output = String::new();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Text(text) | Event::Code(text) => output.push_str(&text),
            Event::TaskListMarker(checked) => {
                if !output.is_empty() && !output.ends_with(char::is_whitespace) {
                    output.push(' ');
                }
                output.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::SoftBreak | Event::HardBreak => output.push(' '),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::TableCell,
            ) => output.push(' '),
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn markdown_style_spans(markdown: &str) -> Vec<MarkupSpan> {
    let options = markdown_options();
    let mut active_marks = Vec::new();
    let mut spans = Vec::new();

    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Strong) => {
                start_markup(markdown, &mut active_marks, &mut spans, Mark::Bold, range)
            }
            Event::End(TagEnd::Strong) => {
                end_markup(markdown, &mut active_marks, &mut spans, Mark::Bold, range)
            }
            Event::Start(Tag::Emphasis) => {
                start_markup(markdown, &mut active_marks, &mut spans, Mark::Italic, range)
            }
            Event::End(TagEnd::Emphasis) => {
                end_markup(markdown, &mut active_marks, &mut spans, Mark::Italic, range)
            }
            Event::Start(Tag::Strikethrough) => start_markup(
                markdown,
                &mut active_marks,
                &mut spans,
                Mark::Strikethrough,
                range,
            ),
            Event::End(TagEnd::Strikethrough) => end_markup(
                markdown,
                &mut active_marks,
                &mut spans,
                Mark::Strikethrough,
                range,
            ),
            Event::InlineHtml(html) if html.as_ref() == "<u>" => {
                active_marks.push(Mark::Underline);
                spans.push(MarkupSpan {
                    range,
                    mark: None,
                    hidden: true,
                });
            }
            Event::InlineHtml(html) if html.as_ref() == "</u>" => {
                if active_marks.last() == Some(&Mark::Underline) {
                    active_marks.pop();
                }
                spans.push(MarkupSpan {
                    range,
                    mark: None,
                    hidden: true,
                });
            }
            Event::Text(_) => {
                spans.extend(active_marks.iter().map(|mark| MarkupSpan {
                    range: range.clone(),
                    mark: Some(*mark),
                    hidden: false,
                }));
            }
            _ => {}
        }
    }

    spans
}

fn start_markup(
    markdown: &str,
    active_marks: &mut Vec<Mark>,
    spans: &mut Vec<MarkupSpan>,
    mark: Mark,
    range: Range<usize>,
) {
    active_marks.push(mark);
    let length = delimiter_length(mark, markdown.get(range.clone()).unwrap_or_default());
    spans.push(MarkupSpan {
        range: range.start..(range.start + length).min(range.end),
        mark: None,
        hidden: true,
    });
}

fn end_markup(
    markdown: &str,
    active_marks: &mut Vec<Mark>,
    spans: &mut Vec<MarkupSpan>,
    mark: Mark,
    range: Range<usize>,
) {
    if active_marks.last() == Some(&mark) {
        active_marks.pop();
    }
    let length = delimiter_length(mark, markdown.get(range.clone()).unwrap_or_default());
    spans.push(MarkupSpan {
        range: range.end.saturating_sub(length)..range.end,
        mark: None,
        hidden: true,
    });
}

fn delimiter_length(mark: Mark, source: &str) -> usize {
    match mark {
        Mark::Bold if source.starts_with("**") || source.starts_with("__") => 2,
        Mark::Italic if source.starts_with('*') || source.starts_with('_') => 1,
        Mark::Strikethrough if source.starts_with("~~") => 2,
        Mark::Code if source.starts_with('`') => 1,
        Mark::Bold | Mark::Italic | Mark::Underline | Mark::Strikethrough | Mark::Code => 0,
    }
}

impl EditorDocument {
    pub fn from_markdown(
        markdown: &str,
        mut image_asset: impl FnMut(&str, &str) -> Result<String>,
    ) -> Result<Self> {
        let options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
        let mut frames = Vec::new();
        let mut blocks = Vec::new();
        let mut marks = Vec::new();

        for event in Parser::new_ext(markdown, options) {
            match event {
                Event::Start(Tag::Paragraph) => frames.push(Frame::Paragraph(Vec::new())),
                Event::End(TagEnd::Paragraph) => {
                    let Frame::Paragraph(content) = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched paragraph"))?
                    else {
                        bail!("mismatched paragraph")
                    };
                    append_block(&mut frames, &mut blocks, Block::Paragraph { content });
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    frames.push(Frame::Heading(heading_level(level), Vec::new()));
                }
                Event::End(TagEnd::Heading(_)) => {
                    let Frame::Heading(level, content) = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched heading"))?
                    else {
                        bail!("mismatched heading")
                    };
                    append_block(&mut frames, &mut blocks, Block::Heading { level, content });
                }
                Event::Start(Tag::List(start)) => frames.push(Frame::List {
                    ordered: start.is_some(),
                    start,
                    items: Vec::new(),
                }),
                Event::End(TagEnd::List(_)) => {
                    let Frame::List {
                        ordered,
                        start,
                        items,
                    } = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched list"))?
                    else {
                        bail!("mismatched list")
                    };
                    append_block(
                        &mut frames,
                        &mut blocks,
                        Block::List {
                            ordered,
                            start,
                            items,
                        },
                    );
                }
                Event::Start(Tag::Item) => frames.push(Frame::Item(ListItem {
                    checked: None,
                    blocks: Vec::new(),
                })),
                Event::End(TagEnd::Item) => {
                    let Frame::Item(item) = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched list item"))?
                    else {
                        bail!("mismatched list item")
                    };
                    let Some(Frame::List { items, .. }) = frames.last_mut() else {
                        bail!("list item outside a list")
                    };
                    items.push(item);
                }
                Event::TaskListMarker(checked) => {
                    let Some(Frame::Item(item)) = frames
                        .iter_mut()
                        .rev()
                        .find(|frame| matches!(frame, Frame::Item(_)))
                    else {
                        bail!("task marker outside a list item")
                    };
                    item.checked = Some(checked);
                }
                Event::Start(Tag::BlockQuote(_)) => frames.push(Frame::Quote(Vec::new())),
                Event::End(TagEnd::BlockQuote(_)) => {
                    let Frame::Quote(content) = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched quote"))?
                    else {
                        bail!("mismatched quote")
                    };
                    append_block(&mut frames, &mut blocks, Block::Quote { blocks: content });
                }
                Event::Start(Tag::CodeBlock(kind)) => frames.push(Frame::CodeBlock {
                    language: match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            Some(language.to_string())
                        }
                        _ => None,
                    },
                    code: String::new(),
                }),
                Event::End(TagEnd::CodeBlock) => {
                    let Frame::CodeBlock { language, code } = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched code block"))?
                    else {
                        bail!("mismatched code block")
                    };
                    append_block(
                        &mut frames,
                        &mut blocks,
                        Block::CodeBlock { language, code },
                    );
                }
                Event::Start(Tag::Link { dest_url, .. }) => frames.push(Frame::Link {
                    url: dest_url.to_string(),
                    content: Vec::new(),
                }),
                Event::End(TagEnd::Link) => {
                    let Frame::Link { url, content } = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched link"))?
                    else {
                        bail!("mismatched link")
                    };
                    append_inline(&mut frames, &mut blocks, Inline::Link { url, content })?;
                }
                Event::Start(Tag::Image { dest_url, .. }) => frames.push(Frame::Image {
                    url: dest_url.to_string(),
                    alt: String::new(),
                }),
                Event::End(TagEnd::Image) => {
                    let Frame::Image { url, alt } = frames
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("unmatched image"))?
                    else {
                        bail!("mismatched image")
                    };
                    let asset_id = image_asset(&url, &alt)?;
                    append_inline(&mut frames, &mut blocks, Inline::Image { asset_id, alt })?;
                }
                Event::Start(Tag::Emphasis) => marks.push(Mark::Italic),
                Event::End(TagEnd::Emphasis) => pop_mark(&mut marks, Mark::Italic)?,
                Event::Start(Tag::Strong) => marks.push(Mark::Bold),
                Event::End(TagEnd::Strong) => pop_mark(&mut marks, Mark::Bold)?,
                Event::Start(Tag::Strikethrough) => marks.push(Mark::Strikethrough),
                Event::End(TagEnd::Strikethrough) => pop_mark(&mut marks, Mark::Strikethrough)?,
                Event::Code(text) => append_inline(
                    &mut frames,
                    &mut blocks,
                    Inline::Text {
                        text: text.to_string(),
                        marks: vec![Mark::Code],
                    },
                )?,
                Event::Text(text) => {
                    if let Some(Frame::CodeBlock { code, .. }) = frames.last_mut() {
                        code.push_str(&text);
                    } else if let Some(Frame::Image { alt, .. }) = frames.last_mut() {
                        alt.push_str(&text);
                    } else {
                        append_inline(
                            &mut frames,
                            &mut blocks,
                            Inline::Text {
                                text: text.to_string(),
                                marks: marks.clone(),
                            },
                        )?;
                    }
                }
                Event::SoftBreak => append_inline(
                    &mut frames,
                    &mut blocks,
                    Inline::Text {
                        text: " ".to_owned(),
                        marks: marks.clone(),
                    },
                )?,
                Event::HardBreak => append_inline(
                    &mut frames,
                    &mut blocks,
                    Inline::Text {
                        text: "\n".to_owned(),
                        marks: marks.clone(),
                    },
                )?,
                Event::Rule => append_block(&mut frames, &mut blocks, Block::ThematicBreak),
                Event::InlineHtml(html) if html.as_ref() == "<u>" => {
                    marks.push(Mark::Underline);
                }
                Event::InlineHtml(html) if html.as_ref() == "</u>" => {
                    pop_mark(&mut marks, Mark::Underline)?;
                }
                Event::Html(_) | Event::InlineHtml(_) => {
                    bail!("raw HTML is not supported by the visual editor")
                }
                Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {
                    bail!("this Markdown construct is not supported by the visual editor")
                }
                Event::Start(_) | Event::End(_) => {
                    bail!("this Markdown construct is not supported by the visual editor")
                }
            }
        }

        ensure!(frames.is_empty(), "unclosed Markdown construct");
        ensure!(marks.is_empty(), "unclosed Markdown formatting");
        Ok(Self { blocks })
    }

    pub fn to_markdown(
        &self,
        mut image_reference: impl FnMut(&str) -> Result<String>,
    ) -> Result<String> {
        render_blocks(&self.blocks, &mut image_reference)
    }
}

fn render_blocks(
    blocks: &[Block],
    image_reference: &mut impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut output = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            output.push_str("\n\n");
        }
        match block {
            Block::Paragraph { content } => {
                render_inlines(content, &mut output, image_reference)?;
            }
            Block::Heading { level, content } => {
                ensure!(
                    (1..=6).contains(level),
                    "heading level must be between 1 and 6"
                );
                output.push_str(&"#".repeat(*level as usize));
                output.push(' ');
                render_inlines(content, &mut output, image_reference)?;
            }
            Block::List {
                ordered,
                start,
                items,
            } => {
                for (item_index, item) in items.iter().enumerate() {
                    if item_index > 0 {
                        output.push('\n');
                    }
                    let marker = match item.checked {
                        Some(true) => "- [x] ".to_owned(),
                        Some(false) => "- [ ] ".to_owned(),
                        None if *ordered => {
                            format!("{}. ", start.unwrap_or(1) + item_index as u64)
                        }
                        None => "- ".to_owned(),
                    };
                    let item_content = render_blocks(&item.blocks, image_reference)?;
                    for (line_index, line) in item_content.lines().enumerate() {
                        if line_index == 0 {
                            output.push_str(&marker);
                        } else {
                            output.push_str("  ");
                        }
                        output.push_str(line);
                        if line_index + 1 < item_content.lines().count() {
                            output.push('\n');
                        }
                    }
                }
            }
            Block::CodeBlock { language, code } => {
                let fence_size = code
                    .split('\n')
                    .map(|line| {
                        line.chars()
                            .take_while(|character| *character == '`')
                            .count()
                    })
                    .max()
                    .unwrap_or(0)
                    .max(2)
                    + 1;
                let fence = "`".repeat(fence_size);
                output.push_str(&fence);
                if let Some(language) = language {
                    output.push_str(language);
                }
                output.push('\n');
                output.push_str(code);
                if !code.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&fence);
            }
            Block::Quote { blocks } => {
                let content = render_blocks(blocks, image_reference)?;
                for (line_index, line) in content.lines().enumerate() {
                    if line_index > 0 {
                        output.push('\n');
                    }
                    output.push_str("> ");
                    output.push_str(line);
                }
            }
            Block::ThematicBreak => output.push_str("---"),
        }
    }
    Ok(output)
}

fn render_inlines(
    inlines: &[Inline],
    output: &mut String,
    image_reference: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    for inline in inlines {
        match inline {
            Inline::Text { text, marks } => {
                output.push_str(&markdown_inline_text(text, marks));
            }
            Inline::Link { url, content } => {
                output.push('[');
                render_inlines(content, output, image_reference)?;
                output.push_str("](");
                escape_markdown_destination(url, output);
                output.push(')');
            }
            Inline::Image { asset_id, alt } => {
                let reference = image_reference(asset_id)?;
                output.push_str("![");
                escape_markdown_text(alt, output);
                output.push_str("](");
                escape_markdown_destination(&reference, output);
                output.push(')');
            }
        }
    }
    Ok(())
}

enum Frame {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    List {
        ordered: bool,
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Item(ListItem),
    Quote(Vec<Block>),
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Link {
        url: String,
        content: Vec<Inline>,
    },
    Image {
        url: String,
        alt: String,
    },
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn append_block(frames: &mut [Frame], root: &mut Vec<Block>, block: Block) {
    match frames
        .iter_mut()
        .rev()
        .find(|frame| matches!(frame, Frame::Item(_) | Frame::Quote(_)))
    {
        Some(Frame::Item(item)) => item.blocks.push(block),
        Some(Frame::Quote(blocks)) => blocks.push(block),
        _ => root.push(block),
    }
}

fn append_inline(frames: &mut [Frame], root: &mut Vec<Block>, inline: Inline) -> Result<()> {
    if let Some(Frame::Image { alt, .. }) = frames.last_mut() {
        if let Inline::Text { text, .. } = inline {
            alt.push_str(&text);
            return Ok(());
        }
        bail!("images may only contain alt text")
    }
    if let Some(frame) = frames.iter_mut().rev().find(|frame| {
        matches!(
            frame,
            Frame::Paragraph(_) | Frame::Heading(_, _) | Frame::Link { .. }
        )
    }) {
        match frame {
            Frame::Paragraph(content)
            | Frame::Heading(_, content)
            | Frame::Link { content, .. } => {
                content.push(inline);
            }
            _ => unreachable!(),
        }
        return Ok(());
    }
    if let Some(Frame::Item(item)) = frames
        .iter_mut()
        .rev()
        .find(|frame| matches!(frame, Frame::Item(_)))
    {
        if let Some(Block::Paragraph { content }) = item.blocks.last_mut() {
            content.push(inline);
        } else {
            item.blocks.push(Block::Paragraph {
                content: vec![inline],
            });
        }
        return Ok(());
    }
    let _ = root;
    bail!("inline content outside a paragraph")
}

fn pop_mark(marks: &mut Vec<Mark>, mark: Mark) -> Result<()> {
    ensure!(marks.pop() == Some(mark), "mismatched Markdown formatting");
    Ok(())
}

fn mark_delimiters(mark: Mark) -> (&'static str, &'static str) {
    match mark {
        Mark::Bold => ("**", "**"),
        Mark::Italic => ("*", "*"),
        Mark::Underline => ("<u>", "</u>"),
        Mark::Strikethrough => ("~~", "~~"),
        Mark::Code => ("`", "`"),
    }
}

pub fn markdown_inline_text(text: &str, marks: &[Mark]) -> String {
    let leading_length = text.len() - text.trim_start().len();
    let trailing_length = text.len() - text.trim_end().len();
    let core_end = text.len().saturating_sub(trailing_length);
    if leading_length >= core_end {
        let mut output = String::new();
        escape_markdown_text(text, &mut output);
        return output;
    }

    let (leading, core_and_trailing) = text.split_at(leading_length);
    let (core, trailing) = core_and_trailing.split_at(core_end - leading_length);
    let mut output = leading.to_owned();
    if marks.is_empty() {
        escape_markdown_text(core, &mut output);
    } else {
        for mark in marks {
            output.push_str(mark_delimiters(*mark).0);
        }
        if marks.contains(&Mark::Code) {
            output.push_str(core);
        } else {
            escape_markdown_text(core, &mut output);
        }
        for mark in marks.iter().rev() {
            output.push_str(mark_delimiters(*mark).1);
        }
    }
    output.push_str(trailing);
    output
}

fn escape_markdown_text(text: &str, output: &mut String) {
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '*' | '_' | '[' | ']' | '`' | '<' | '>' | '#'
        ) {
            output.push('\\');
        }
        output.push(character);
    }
}

fn escape_markdown_destination(destination: &str, output: &mut String) {
    for character in destination.chars() {
        if matches!(character, '\\' | '(' | ')') {
            output.push('\\');
        }
        output.push(character);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_round_trips_with_checklists_marks_and_image_assets() {
        let document = EditorDocument {
            blocks: vec![
                Block::Heading {
                    level: 1,
                    content: vec![Inline::Text {
                        text: "Trip notes".to_owned(),
                        marks: Vec::new(),
                    }],
                },
                Block::List {
                    ordered: false,
                    start: None,
                    items: vec![ListItem {
                        checked: Some(true),
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::Text {
                                text: "Book tickets".to_owned(),
                                marks: vec![Mark::Bold],
                            }],
                        }],
                    }],
                },
                Block::Paragraph {
                    content: vec![Inline::Image {
                        asset_id: "asset-123".to_owned(),
                        alt: "Train platform".to_owned(),
                    }],
                },
            ],
        };

        let encoded = serde_json::to_string(&document).unwrap();
        let decoded: EditorDocument = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, document);
    }

    #[test]
    fn markdown_export_resolves_assets_without_coupling_the_document_to_memos() {
        let document = EditorDocument {
            blocks: vec![
                Block::Heading {
                    level: 1,
                    content: vec![Inline::Text {
                        text: "Trip notes".to_owned(),
                        marks: Vec::new(),
                    }],
                },
                Block::List {
                    ordered: false,
                    start: None,
                    items: vec![ListItem {
                        checked: Some(true),
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::Text {
                                text: "Book tickets".to_owned(),
                                marks: vec![Mark::Bold],
                            }],
                        }],
                    }],
                },
                Block::Paragraph {
                    content: vec![Inline::Image {
                        asset_id: "local-asset".to_owned(),
                        alt: "Train platform".to_owned(),
                    }],
                },
            ],
        };

        let markdown = document
            .to_markdown(|asset_id| {
                assert_eq!(asset_id, "local-asset");
                Ok("attachments/server-asset".to_owned())
            })
            .unwrap();

        assert_eq!(
            markdown,
            "# Trip notes\n\n- [x] **Book tickets**\n\n![Train platform](attachments/server-asset)"
        );
    }

    #[test]
    fn markdown_export_rejects_invalid_heading_levels() {
        let document = EditorDocument {
            blocks: vec![Block::Heading {
                level: 7,
                content: Vec::new(),
            }],
        };

        assert!(document.to_markdown(|_| Ok(String::new())).is_err());
    }

    #[test]
    fn marked_text_keeps_boundary_spaces_outside_markdown_delimiters() {
        assert_eq!(markdown_inline_text("Bold ", &[Mark::Bold]), "**Bold** ");
        assert_eq!(markdown_inline_text(" Bold", &[Mark::Bold]), " **Bold**");
        assert_eq!(markdown_inline_text("plain ", &[]), "plain ");
    }

    #[test]
    fn markdown_export_fails_instead_of_dropping_unresolved_images() {
        let document = EditorDocument {
            blocks: vec![Block::Paragraph {
                content: vec![Inline::Image {
                    asset_id: "local-asset".to_owned(),
                    alt: "photo".to_owned(),
                }],
            }],
        };

        assert!(
            document
                .to_markdown(|_| bail!("image is not uploaded"))
                .is_err()
        );
    }

    #[test]
    fn markdown_import_builds_portable_blocks_and_resolves_image_assets() {
        let document = EditorDocument::from_markdown(
            "# Trip notes\n\n- [x] **Book tickets**\n\n![Train platform](https://example.test/photo.png)",
            |url, alt| {
                assert_eq!(url, "https://example.test/photo.png");
                assert_eq!(alt, "Train platform");
                Ok("local-photo".to_owned())
            },
        )
        .unwrap();

        assert_eq!(
            document,
            EditorDocument {
                blocks: vec![
                    Block::Heading {
                        level: 1,
                        content: vec![Inline::Text {
                            text: "Trip notes".to_owned(),
                            marks: Vec::new(),
                        }],
                    },
                    Block::List {
                        ordered: false,
                        start: None,
                        items: vec![ListItem {
                            checked: Some(true),
                            blocks: vec![Block::Paragraph {
                                content: vec![Inline::Text {
                                    text: "Book tickets".to_owned(),
                                    marks: vec![Mark::Bold],
                                }],
                            }],
                        }],
                    },
                    Block::Paragraph {
                        content: vec![Inline::Image {
                            asset_id: "local-photo".to_owned(),
                            alt: "Train platform".to_owned(),
                        }],
                    },
                ],
            }
        );
    }

    #[test]
    fn markdown_import_rejects_raw_html_instead_of_losing_it() {
        assert!(
            EditorDocument::from_markdown("<div>content</div>", |_, _| { Ok("unused".to_owned()) })
                .is_err()
        );
    }

    #[test]
    fn underline_uses_only_the_explicit_html_extension() {
        let source = "Text <u>underlined</u>";
        let document = EditorDocument::from_markdown(source, |_, _| Ok(String::new())).unwrap();

        assert_eq!(document.to_markdown(|_| Ok(String::new())).unwrap(), source);
        assert!(
            EditorDocument::from_markdown("<span>not allowed</span>", |_, _| { Ok(String::new()) })
                .is_err()
        );
    }

    #[test]
    fn markdown_render_spans_separate_source_markers_from_styled_text() {
        let markdown = "**bold** and *italic* and <u>underlined</u>";
        let spans = markdown_style_spans(markdown);
        let bold_start = markdown.find("bold").unwrap();
        let italic_start = markdown.find("italic").unwrap();
        let underline_start = markdown.find("underlined").unwrap();

        for (start, text, mark) in [
            (bold_start, "bold", Mark::Bold),
            (italic_start, "italic", Mark::Italic),
            (underline_start, "underlined", Mark::Underline),
        ] {
            assert!(spans.contains(&MarkupSpan {
                range: start..start + text.len(),
                mark: Some(mark),
                hidden: false,
            }));
        }
        assert!(spans.iter().any(|span| span.hidden && span.range == (0..2)));
        assert!(spans.iter().any(|span| span.hidden && span.range == (6..8)));
        assert!(
            spans
                .iter()
                .any(|span| span.hidden && span.range == (26..29))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.hidden && span.range == (39..43))
        );
    }

    #[test]
    fn markdown_rendering_removes_markers_and_retains_visible_style_ranges() {
        let rendered = render_markdown_source("**bold** and *italic* and <u>underlined</u>");

        assert_eq!(rendered.text, "bold and italic and underlined");
        for (text, mark) in [
            ("bold", Mark::Bold),
            ("italic", Mark::Italic),
            ("underlined", Mark::Underline),
        ] {
            let start = rendered.text.find(text).unwrap();
            assert!(rendered.spans.contains(&MarkupSpan {
                range: start..start + text.len(),
                mark: Some(mark),
                hidden: false,
            }));
        }
    }

    #[test]
    fn markdown_rendering_hides_heading_markers_and_skips_fenced_code() {
        let rendered = render_markdown_source(
            "# Title\n\n## **Section**\n### Subsection\n```md\n#### literal\n```",
        );

        assert_eq!(
            rendered.text,
            "Title\n\nSection\nSubsection\n```md\n#### literal\n```"
        );
        assert_eq!(rendered.headings.len(), 3);
        for (level, text) in [(1, "Title"), (2, "Section"), (3, "Subsection")] {
            let heading = rendered
                .headings
                .iter()
                .find(|heading| heading.level == level)
                .unwrap();
            assert_eq!(&rendered.text[heading.range.clone()], text);
        }
    }

    #[test]
    fn markdown_render_ranges_remain_byte_aligned_after_unicode_text() {
        let rendered = render_markdown_source("é **bold**");

        assert_eq!(rendered.text, "é bold");
        let bold = rendered
            .spans
            .iter()
            .find(|span| span.mark == Some(Mark::Bold))
            .unwrap();
        assert_eq!(&rendered.text[bold.range.clone()], "bold");
        assert_eq!(rendered.text[..bold.range.start].chars().count(), 2);
    }

    #[test]
    fn byte_to_char_offsets_handles_multibyte_text() {
        let offsets = byte_to_char_offsets("éx");

        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[2], 1);
        assert_eq!(offsets[3], 2);
    }

    #[test]
    fn markdown_rendering_handles_twenty_kilobytes_with_many_formatting_spans() {
        let markdown = "**word** ".repeat(2_500);

        let rendered = render_markdown_source(&markdown);

        assert_eq!(rendered.text, "word ".repeat(2_500));
        assert_eq!(
            rendered
                .spans
                .iter()
                .filter(|span| span.mark == Some(Mark::Bold))
                .count(),
            2_500
        );
    }

    #[test]
    fn markdown_rendering_hides_link_syntax_and_reports_link_ranges() {
        let rendered =
            render_markdown_source("é [**site**](https://example.test/a_(b)) and <https://x.test>");

        assert_eq!(rendered.text, "é site and https://x.test");
        assert_eq!(rendered.links.len(), 2);
        assert_eq!(&rendered.text[rendered.links[0].range.clone()], "site");
        assert_eq!(rendered.links[0].url, "https://example.test/a_(b)");
        assert_eq!(
            &rendered.text[rendered.links[1].range.clone()],
            "https://x.test"
        );
    }

    #[test]
    fn link_urls_are_normalized_and_restricted_to_safe_schemes() {
        assert_eq!(
            normalize_link_url(" example.test/a b "),
            Some("https://example.test/a%20b".into())
        );
        assert_eq!(
            normalize_link_url("me@example.test"),
            Some("mailto:me@example.test".into())
        );
        assert_eq!(
            normalize_link_url("http://example.test"),
            Some("http://example.test".into())
        );
        assert_eq!(normalize_link_url("javascript:alert(1)"), None);
        assert_eq!(normalize_link_url("  "), None);
        assert!(!is_openable_link("file:///etc/passwd"));
    }

    #[test]
    fn markdown_link_escapes_brackets_and_parentheses_once() {
        assert_eq!(
            markdown_link("a [b] \\[c\\]", "https://x.test/(y)"),
            "[a \\[b\\] \\[c\\]](https://x.test/\\(y\\))"
        );
    }

    #[test]
    fn markdown_preview_hides_markup_and_keeps_task_status_and_alt_text() {
        assert_eq!(
            markdown_preview_text(
                "- [ ] **Pick up milk**\n\n![shopping list](photo.png) and [store](https://example.test)"
            ),
            "☐ Pick up milk shopping list and store"
        );
    }

    #[test]
    fn supported_markdown_round_trips_through_the_portable_document() {
        let source = "# Trip notes\n\n- [x] **Book tickets**\n\n3. First stop\n4. Second stop\n\n![Train platform](https://example.test/photo.png)";
        let document = EditorDocument::from_markdown(source, |url, _| Ok(url.to_owned())).unwrap();
        let exported = document.to_markdown(|url| Ok(url.to_owned())).unwrap();

        assert_eq!(exported, source);
    }
    #[test]
    fn pipe_tables_become_one_placeholder_with_their_cells() {
        let source = "# Title\n\nBefore\n\n| Name | Qty |\n| :--- | ---: |\n| **Milk** | 2 \\| 3 |\n| Eggs |\n\nAfter";
        let rendered = render_markdown_source(source);

        assert_eq!(rendered.text, "Title\n\nBefore\n\n\u{fffc}\n\nAfter");
        assert_eq!(rendered.tables.len(), 1);
        let table = &rendered.tables[0];
        assert_eq!(
            &rendered.text[table.range.clone()],
            "\u{fffc}"
        );
        assert_eq!(
            table.alignments,
            vec![TableAlignment::Left, TableAlignment::Right]
        );
        assert_eq!(
            table.rows,
            vec![
                vec!["Name".to_owned(), "Qty".to_owned()],
                vec!["**Milk**".to_owned(), "2 \\| 3".to_owned()],
                vec!["Eggs".to_owned(), String::new()],
            ]
        );
        assert!(rendered.spans.is_empty());
    }

    #[test]
    fn table_markdown_round_trips_and_escapes_pipes() {
        let rows = vec![
            vec!["A".to_owned(), "B | C".to_owned()],
            vec!["1".to_owned(), "two\nlines".to_owned()],
        ];
        let alignments = [TableAlignment::Center, TableAlignment::None];
        let markdown = table_markdown(&alignments, &rows);
        assert_eq!(
            markdown,
            "| A | B \\| C |\n| :---: | --- |\n| 1 | two lines |"
        );
        let rendered = render_markdown_source(&markdown);
        assert_eq!(rendered.tables[0].alignments, alignments);
        assert_eq!(table_markdown(&alignments, &rendered.tables[0].rows), markdown);
    }

    #[test]
    fn thematic_breaks_become_one_placeholder() {
        let rendered = render_markdown_source("# Title\n\nBefore\n\n---\n\nAfter\n\n* * *\n");

        assert_eq!(rendered.text, "Title\n\nBefore\n\n\u{fffc}\n\nAfter\n\n\u{fffc}\n");
        assert_eq!(rendered.rules.len(), 2);
        assert_eq!(&rendered.text[rendered.rules[0].clone()], "\u{fffc}");
        assert!(rendered.spans.is_empty());

        let nested = render_markdown_source("> quote\n>\n> ---\n");
        assert!(nested.rules.is_empty());
    }

    #[test]
    fn thematic_break_lines_are_recognized() {
        for line in ["---", "***", "___", "- - -", "  * * * *"] {
            assert!(is_thematic_break(line), "{line}");
        }
        for line in ["--", "- item", "-*-", "--- x", ""] {
            assert!(!is_thematic_break(line), "{line}");
        }
    }

    #[test]
    fn tables_inside_lists_stay_plain_text() {
        let rendered = render_markdown_source("- item\n\n  | a | b |\n  | - | - |\n  | 1 | 2 |");
        assert!(rendered.tables.is_empty());
    }

    #[test]
    fn table_preview_lists_cell_text() {
        assert_eq!(
            markdown_preview_text("| a | b |\n| - | - |\n| 1 | 2 |"),
            "a b 1 2"
        );
    }
    #[test]
    fn spreadsheet_text_becomes_table_rows() {
        let copied = "Name\tQty\r\nMilk\t2\r\n\"Eggs\nfree range\"\t\"12 \"\"large\"\"\"\r\n";
        assert_eq!(
            pasted_table_rows(copied).unwrap(),
            vec![
                vec!["Name".to_owned(), "Qty".to_owned()],
                vec!["Milk".to_owned(), "2".to_owned()],
                vec!["Eggs free range".to_owned(), "12 \"large\"".to_owned()],
            ]
        );
    }

    #[test]
    fn ordinary_text_is_not_mistaken_for_a_table() {
        assert!(pasted_table_rows("just a sentence").is_none());
        assert!(pasted_table_rows("one\ntwo\nthree").is_none());
        assert!(pasted_table_rows("\tindented code").is_none());
        assert!(pasted_table_rows("a\tb\nc").is_none());
        assert!(pasted_table_rows("fn main() {\n\tprintln!();\n}").is_none());
        assert!(pasted_table_rows("a\tb").is_some());
    }
}

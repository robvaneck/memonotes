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
}

pub fn render_markdown_source(markdown: &str) -> RenderedMarkdown {
    let source_spans = markdown_style_spans(markdown);
    let hidden_ranges: Vec<_> = source_spans
        .iter()
        .filter(|span| span.hidden)
        .map(|span| span.range.clone())
        .collect();
    let mut output = String::with_capacity(markdown.len());
    let mut output_offsets = vec![0; markdown.len() + 1];

    for (byte_offset, character) in markdown.char_indices() {
        let end_offset = byte_offset + character.len_utf8();
        let hidden = hidden_ranges
            .iter()
            .any(|range| range.start <= byte_offset && byte_offset < range.end);
        output_offsets[byte_offset] = output.len();
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

    RenderedMarkdown {
        text: output,
        spans,
    }
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
    let options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
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
                | TagEnd::CodeBlock,
            ) => output.push(' '),
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn markdown_style_spans(markdown: &str) -> Vec<MarkupSpan> {
    let options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
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
}

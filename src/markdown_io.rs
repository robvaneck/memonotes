use crate::images;
use crate::{Note, NoteFilter, Store, tr};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Serialize)]
struct ExportManifest<'a> {
    format_version: u32,
    account: &'a str,
    notes: Vec<ExportedNote<'a>>,
}

#[derive(Serialize)]
struct ExportedNote<'a> {
    id: &'a str,
    title: &'a str,
    folder: &'a str,
    markdown: String,
    updated_at: i64,
    pinned: bool,
    archived: bool,
}

pub fn read_markdown_files(paths: &[PathBuf]) -> Result<Vec<(String, String)>> {
    paths
        .iter()
        .map(|path| {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default();
            if !extension.eq_ignore_ascii_case("md") {
                bail!("{} is not a Markdown file", path.display());
            }
            let file_name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("a selected Markdown file has no valid name")?
                .to_owned();
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            Ok((file_name, content))
        })
        .collect()
}

pub fn export_account_zip(
    store: &Store,
    account_id: Option<&str>,
    account_label: &str,
    image_directory: &Path,
    destination: &Path,
) -> Result<()> {
    let account_folders: Vec<_> = store
        .folders()?
        .into_iter()
        .filter(|folder| folder.account_id.as_deref() == account_id)
        .collect();
    let mut used_folder_names = HashSet::from(["unfiled".to_owned()]);
    let mut folder_names = HashMap::new();
    let mut folders = HashMap::new();
    for folder in account_folders {
        let safe_name = unique_folder_name(&folder.name, &mut used_folder_names);
        folder_names.insert(folder.id.clone(), safe_name);
        folders.insert(folder.id, folder.name);
    }
    let mut notes = Vec::new();
    for archived in [false, true] {
        notes.extend(store.notes(&NoteFilter {
            account_id: account_id.map(str::to_owned),
            archived,
            ..NoteFilter::default()
        })?);
    }

    let mut temporary = TemporaryExport::new(destination)?;
    let output = File::create(&temporary.path)
        .with_context(|| format!("could not create {}", temporary.path.display()))?;
    let mut archive = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut written_directories = HashSet::new();
    let mut written_assets = HashSet::new();
    let mut used_paths = HashSet::new();
    let mut manifest_notes = Vec::with_capacity(notes.len());

    for note in &notes {
        let folder = note
            .folder_id
            .as_ref()
            .and_then(|folder_id| folders.get(folder_id))
            .map_or("Unfiled", String::as_str);
        let safe_folder = note
            .folder_id
            .as_ref()
            .and_then(|folder_id| folder_names.get(folder_id))
            .cloned()
            .unwrap_or_else(|| "Unfiled".to_owned());
        let directory = if note.archived {
            format!("_Archive/{safe_folder}")
        } else {
            safe_folder
        };
        let directory_entry = format!("{directory}/");
        if written_directories.insert(directory_entry.clone()) {
            archive.add_directory(&directory_entry, options)?;
        }
        let file_name = unique_note_filename(&note.title, &directory, &mut used_paths);
        let markdown_path = format!("{directory}/{file_name}");
        let markdown = portable_markdown(
            note,
            &directory,
            image_directory,
            &mut archive,
            options,
            &mut written_directories,
            &mut written_assets,
        )?;
        archive.start_file(&markdown_path, options)?;
        archive.write_all(markdown.as_bytes())?;
        manifest_notes.push(ExportedNote {
            id: &note.id,
            title: &note.title,
            folder,
            markdown: markdown_path,
            updated_at: note.updated_at,
            pinned: note.pinned,
            archived: note.archived,
        });
    }

    let manifest = ExportManifest {
        format_version: 1,
        account: account_label,
        notes: manifest_notes,
    };
    archive.start_file("_memonotes/manifest.json", options)?;
    archive.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    archive.finish()?;
    std::fs::rename(&temporary.path, destination)
        .with_context(|| format!("could not move ZIP to {}", destination.display()))?;
    temporary.keep = true;
    Ok(())
}

struct TemporaryExport {
    path: PathBuf,
    keep: bool,
}

impl TemporaryExport {
    fn new(destination: &Path) -> Result<Self> {
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("memonotes-export.zip");
        Ok(Self {
            path: destination.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4())),
            keep: false,
        })
    }
}

impl Drop for TemporaryExport {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn portable_markdown(
    note: &Note,
    directory: &str,
    image_directory: &Path,
    archive: &mut ZipWriter<File>,
    options: SimpleFileOptions,
    written_directories: &mut HashSet<String>,
    written_assets: &mut HashSet<String>,
) -> Result<String> {
    let content = content_with_title_heading(note);
    let images = images::local_images(&content);
    let mut edits = Vec::with_capacity(images.len());
    for image in images {
        let asset_path = image_directory.join(&image.local);
        let bytes = std::fs::read(&asset_path)
            .with_context(|| format!("missing image file {}", asset_path.display()))?;
        let relative_asset = format!("_assets/{}", safe_component(&image.local));
        let archive_asset = format!("{directory}/{relative_asset}");
        let asset_directory = format!("{directory}/_assets/");
        if written_directories.insert(asset_directory.clone()) {
            archive.add_directory(&asset_directory, options)?;
        }
        if written_assets.insert(archive_asset.clone()) {
            archive.start_file(&archive_asset, options)?;
            archive.write_all(&bytes)?;
        }
        let width = image
            .width
            .map_or_else(String::new, |width| format!("#w={width}"));
        edits.push((image.range, format!("{relative_asset}{width}")));
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut markdown = String::with_capacity(content.len());
    let mut copied = 0;
    for (range, replacement) in edits {
        markdown.push_str(&content[copied..range.start]);
        markdown.push_str(&replacement);
        copied = range.end;
    }
    markdown.push_str(&content[copied..]);
    if markdown.contains("memonotes-image://") {
        bail!(
            "note '{}' contains an image reference that cannot be exported",
            note.title
        );
    }
    Ok(markdown)
}

/// Ensures the exported document starts with the note title as a heading.
fn content_with_title_heading(note: &Note) -> String {
    let (title, body) = note.editor_parts();
    if title.is_empty() {
        return note.content.clone();
    }
    if body == note.content {
        return crate::memo::editor_content(title, body);
    }
    let first = note.content.lines().next().unwrap_or("").trim();
    if first.starts_with('#') && title != first {
        note.content.clone()
    } else {
        crate::memo::editor_content(title, body)
    }
}

fn safe_component(value: &str) -> String {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| {
            if matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    let cleaned: String = cleaned.chars().take(120).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "Untitled".to_owned()
    } else {
        cleaned
    }
}

fn unique_folder_name(name: &str, used: &mut HashSet<String>) -> String {
    let base = safe_component(name);
    let mut candidate = base.clone();
    let mut sequence = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{base} ({sequence})");
        sequence += 1;
    }
    candidate
}

fn unique_note_filename(title: &str, directory: &str, used: &mut HashSet<String>) -> String {
    let stem = safe_component(if title.trim().is_empty() {
        tr("note.new")
    } else {
        title
    });
    let mut name = format!("{stem}.md");
    let mut sequence = 2;
    while !used.insert(format!("{directory}/{name}").to_lowercase()) {
        name = format!("{stem} ({sequence}).md");
        sequence += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use std::io::Read;

    #[test]
    fn batch_markdown_import_reads_many_files_and_uses_file_stems() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = directory.path().join("First.md");
        let second = directory.path().join("second.MD");
        std::fs::write(&first, "# Heading\n\nBody")?;
        std::fs::write(&second, "Plain text")?;

        let imported = read_markdown_files(&[first, second])?;
        assert_eq!(imported[0].0, "First");
        assert_eq!(imported[0].1, "# Heading\n\nBody");
        assert_eq!(imported[1].0, "second");
        assert_eq!(imported[1].1, "Plain text");
        Ok(())
    }

    #[test]
    fn export_packages_folder_notes_and_relative_images() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("notes.sqlite");
        let images = directory.path().join("images");
        std::fs::create_dir(&images)?;
        let image_name = "2e4ba3c8-6d28-4fd0-bab1-1604b219b66c.png";
        std::fs::write(images.join(image_name), b"image-bytes")?;
        let store = Store::open(database)?;
        let folder_id = store.create_folder(None, "Travel / Trips")?;
        let content = format!("# Trip\n\n![Photo](memonotes-image://{image_name}#w=320)");
        let note_ids =
            store.import_markdown_notes(None, Some(&folder_id), &[("Trip".into(), content)])?;
        let destination = directory.path().join("export.zip");

        export_account_zip(&store, None, "On this device", &images, &destination)?;
        let mut archive = zip::ZipArchive::new(File::open(destination)?)?;
        let mut markdown = String::new();
        archive
            .by_name("Travel _ Trips/Trip.md")?
            .read_to_string(&mut markdown)?;
        assert!(markdown.contains("![Photo](_assets/"));
        assert!(markdown.contains("#w=320"));
        assert_eq!(
            archive
                .by_name(&("Travel _ Trips/_assets/".to_owned() + image_name))?
                .size(),
            11
        );
        let mut manifest = String::new();
        archive
            .by_name("_memonotes/manifest.json")?
            .read_to_string(&mut manifest)?;
        assert!(manifest.contains(&note_ids[0]));
        assert!(manifest.contains("Travel / Trips"));
        Ok(())
    }

    #[test]
    fn export_writes_the_title_into_the_markdown() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("notes.sqlite"))?;
        let ids = store.import_markdown_notes(
            None,
            None,
            &[
                ("plain".into(), "Plain first line\nBody".into()),
                ("heading".into(), "# Heading\n\nBody".into()),
                ("legacy".into(), "placeholder".into()),
            ],
        )?;
        store.save_note(&ids[2], "Only body", "Separate title")?;
        let destination = directory.path().join("export.zip");

        export_account_zip(&store, None, "On this device", directory.path(), &destination)?;
        let mut archive = zip::ZipArchive::new(File::open(destination)?)?;
        let mut read = |name: &str| -> Result<String> {
            let mut text = String::new();
            archive.by_name(name)?.read_to_string(&mut text)?;
            Ok(text)
        };
        assert_eq!(
            read("Unfiled/Plain first line.md")?,
            "# Plain first line\nBody"
        );
        assert_eq!(read("Unfiled/Heading.md")?, "# Heading\n\nBody");
        assert_eq!(
            read("Unfiled/Separate title.md")?,
            "# Separate title\nOnly body"
        );
        Ok(())
    }

    #[test]
    fn batch_import_targets_the_selected_account_and_queues_each_note() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("notes.sqlite"))?;
        store.add_backend_account("account", "Memos", "memos", "https://example.test")?;
        let folder_id = store.create_folder(Some("account"), "Inbox")?;
        let imported = store.import_markdown_notes(
            Some("account"),
            Some(&folder_id),
            &[
                ("one".to_owned(), "# First\n\nBody".to_owned()),
                ("two".to_owned(), "Plain text".to_owned()),
            ],
        )?;

        assert_eq!(imported.len(), 2);
        for id in &imported {
            let note = store.note(id)?.context("imported note missing")?;
            assert_eq!(note.account_id.as_deref(), Some("account"));
            assert_eq!(note.folder_id.as_deref(), Some(folder_id.as_str()));
        }
        let events = store.pending_sync_events(10)?;
        let imported_events: Vec<_> = events
            .iter()
            .filter(|entry| matches!(&entry.event, crate::SyncEvent::NoteCreated { .. }))
            .collect();
        assert_eq!(imported_events.len(), 2);
        Ok(())
    }

    #[test]
    fn remote_markdown_import_rejects_oversized_notes_without_partial_import() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("notes.sqlite"))?;
        store.add_backend_account("account", "Memos", "memos", "https://example.test")?;
        let oversized = "x".repeat(16 * 1024);

        let result = store.import_markdown_notes(
            Some("account"),
            None,
            &[
                ("valid".to_owned(), "Small note".to_owned()),
                ("too-large".to_owned(), oversized.clone()),
            ],
        );
        assert!(result.is_err());
        assert!(store.notes(&crate::NoteFilter::default())?.is_empty());
        assert!(store.pending_sync_events(10)?.is_empty());

        let local = store.import_markdown_notes(None, None, &[("local".to_owned(), oversized)])?;
        assert_eq!(local.len(), 1);
        assert_eq!(store.pending_sync_events(10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn failed_export_keeps_an_existing_zip_untouched() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let images = directory.path().join("images");
        std::fs::create_dir(&images)?;
        let store = Store::open(directory.path().join("notes.sqlite"))?;
        store.import_markdown_notes(
            None,
            None,
            &[(
                "Missing image".to_owned(),
                "![Missing](memonotes-image://2e4ba3c8-6d28-4fd0-bab1-1604b219b66c.png)".to_owned(),
            )],
        )?;
        let destination = directory.path().join("existing.zip");
        std::fs::write(&destination, b"previous archive")?;

        assert!(export_account_zip(&store, None, "Local", &images, &destination).is_err());
        assert_eq!(std::fs::read(destination)?, b"previous archive");
        Ok(())
    }
}

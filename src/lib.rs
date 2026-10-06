use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub id: String,
    pub folder_id: Option<String>,
    pub content: String,
    pub title: String,
    pub updated_at: i64,
    pub pinned: bool,
    pub archived: bool,
}

#[derive(Default)]
pub struct NoteFilter {
    pub folder_id: Option<String>,
    pub unfiled: bool,
    pub archived: bool,
    pub search: String,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut connection = Connection::open(path).context("Kan notitieopslag niet openen")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            bail!("Deze opslag is gemaakt door een nieuwere versie van Memonotes");
        }
        if version == 0 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE folders (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL CHECK(length(trim(name)) > 0)
                );
                CREATE TABLE notes (
                    id TEXT PRIMARY KEY,
                    folder_id TEXT REFERENCES folders(id),
                    content TEXT NOT NULL DEFAULT '',
                    title TEXT NOT NULL DEFAULT 'Nieuwe notitie',
                    updated_at INTEGER NOT NULL,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX notes_folder ON notes(folder_id);
                PRAGMA user_version = 1;",
            )?;
            transaction.commit()?;
        }
        Ok(Self { connection })
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut statement = self.connection.prepare(
            "SELECT folders.id, folders.name, count(notes.id)
             FROM folders LEFT JOIN notes ON notes.folder_id = folders.id AND notes.archived = 0
             GROUP BY folders.id ORDER BY folders.name COLLATE NOCASE, folders.id",
        )?;
        let folders = statement.query_map([], |row| {
            Ok(Folder { id: row.get(0)?, name: row.get(1)?, count: row.get(2)? })
        })?;
        Ok(folders.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_folder(&self, name: &str) -> Result<String> {
        if name.trim().is_empty() {
            bail!("Een folder heeft een naam nodig");
        }
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO folders (id, name) VALUES (?1, ?2)",
            params![id, name.trim()],
        )?;
        Ok(id)
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("Een folder heeft een naam nodig");
        }
        self.changed(self.connection.execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![id, name.trim()],
        )?)
    }

    pub fn delete_folder(&mut self, id: &str) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE notes SET folder_id = NULL WHERE folder_id = ?1",
            [id],
        )?;
        let deleted = transaction.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        if deleted != 1 {
            bail!("De folder bestaat niet meer");
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn create_note(&self, folder_id: Option<&str>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO notes (id, folder_id, title, updated_at) VALUES (?1, ?2, '', ?3)",
            params![id, folder_id, timestamp()?],
        )?;
        Ok(id)
    }

    pub fn save_note(&self, id: &str, content: &str, title: &str) -> Result<()> {
        self.changed(self.connection.execute(
            "UPDATE notes SET content = ?2, title = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, content, title.trim(), timestamp()?],
        )?)
    }

    pub fn move_note(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        self.changed(self.connection.execute(
            "UPDATE notes SET folder_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, folder_id, timestamp()?],
        )?)
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.changed(self.connection.execute(
            "UPDATE notes SET pinned = ?2 WHERE id = ?1", params![id, pinned],
        )?)
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<()> {
        self.changed(self.connection.execute(
            "UPDATE notes SET archived = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, archived, timestamp()?],
        )?)
    }

    pub fn delete_note(&self, id: &str) -> Result<()> {
        self.changed(self.connection.execute("DELETE FROM notes WHERE id = ?1", [id])?)
    }

    pub fn restore_note(&self, note: &Note) -> Result<()> {
        self.connection.execute(
            "INSERT INTO notes (id, folder_id, content, title, updated_at, pinned, archived)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                note.id,
                note.folder_id,
                note.content,
                note.title,
                note.updated_at,
                note.pinned,
                note.archived,
            ],
        )?;
        Ok(())
    }

    pub fn notes(&self, filter: &NoteFilter) -> Result<Vec<Note>> {
        let mut statement = self.connection.prepare(
            "SELECT id, folder_id, content, title, updated_at, pinned, archived FROM notes
             WHERE archived = ?1 AND (?2 IS NULL OR folder_id = ?2)
             AND (?3 = 0 OR folder_id IS NULL)
             AND (?4 = '' OR instr(lower(title || char(10) || content), lower(?4)) > 0)
             ORDER BY pinned DESC, updated_at DESC, id",
        )?;
        let notes = statement.query_map(
            params![filter.archived, filter.folder_id, filter.unfiled, filter.search],
            note_from_row,
        )?;
        Ok(notes.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_notes(&self, filter: &NoteFilter) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM notes
             WHERE archived = ?1 AND (?2 IS NULL OR folder_id = ?2)
             AND (?3 = 0 OR folder_id IS NULL)
             AND (?4 = '' OR instr(lower(title || char(10) || content), lower(?4)) > 0)",
            params![filter.archived, filter.folder_id, filter.unfiled, filter.search],
            |row| row.get(0),
        )?)
    }

    pub fn note(&self, id: &str) -> Result<Option<Note>> {
        Ok(self.connection.query_row(
            "SELECT id, folder_id, content, title, updated_at, pinned, archived FROM notes WHERE id = ?1",
            [id], note_from_row,
        ).optional()?)
    }

    fn changed(&self, count: usize) -> Result<()> {
        if count != 1 {
            bail!("De notitie of folder bestaat niet meer");
        }
        Ok(())
    }
}

fn timestamp() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?)
}

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?, folder_id: row.get(1)?, content: row.get(2)?,
        title: row.get(3)?, updated_at: row.get(4)?, pinned: row.get(5)?, archived: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_and_rich_content_survive_reopening() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let content = "# Boodschappen\n\n**Melk** en *brood*\n\n- [x] Koffie\n- [ ] Thee\n\n![Foto](assets/foto.png)";
        let id;
        {
            let store = Store::open(&path)?;
            let folder = store.create_folder("Privé")?;
            id = store.create_note(Some(&folder))?;
            store.save_note(&id, content, "Boodschappen")?;
        }
        let store = Store::open(&path)?;
        assert_eq!(store.folders()?[0].count, 1);
        assert_eq!(store.note(&id)?.unwrap().content, content);
        Ok(())
    }

    #[test]
    fn blank_note_title_remains_blank_when_saved() -> Result<()> {
        let store = Store::open(":memory:")?;
        let id = store.create_note(None)?;
        assert_eq!(store.note(&id)?.unwrap().title, "");
        store.save_note(&id, "Inhoud", "  ")?;
        let note = store.note(&id)?.unwrap();
        assert_eq!(note.title, "");
        assert_eq!(note.content, "Inhoud");
        Ok(())
    }

    #[test]
    fn note_counts_cover_all_unfiled_archived_and_folder_notes() -> Result<()> {
        let store = Store::open(":memory:")?;
        let folder = store.create_folder("Werk")?;
        let unfiled = store.create_note(None)?;
        let filed = store.create_note(Some(&folder))?;
        let archived = store.create_note(None)?;
        store.set_archived(&archived, true)?;

        assert_eq!(store.count_notes(&NoteFilter::default())?, 2);
        assert_eq!(
            store.count_notes(&NoteFilter {
                unfiled: true,
                ..Default::default()
            })?,
            1
        );
        assert_eq!(
            store.count_notes(&NoteFilter {
                archived: true,
                ..Default::default()
            })?,
            1
        );
        assert_eq!(store.folders()?[0].count, 1);
        assert!(store.note(&unfiled)?.is_some());
        assert!(store.note(&filed)?.is_some());
        Ok(())
    }

    #[test]
    fn deleting_folder_keeps_notes_and_moves_them_to_unfiled() -> Result<()> {
        let mut store = Store::open(":memory:")?;
        let folder_id = store.create_folder("Werk")?;
        let note_id = store.create_note(Some(&folder_id))?;
        let archived_note_id = store.create_note(Some(&folder_id))?;
        store.save_note(&note_id, "Belangrijk", "Plan")?;
        store.set_archived(&archived_note_id, true)?;

        store.delete_folder(&folder_id)?;

        assert!(store.folders()?.is_empty());
        let note = store.note(&note_id)?.unwrap();
        assert_eq!(note.folder_id, None);
        assert_eq!(note.title, "Plan");
        assert_eq!(note.content, "Belangrijk");
        assert_eq!(store.note(&archived_note_id)?.unwrap().folder_id, None);
        let replacement_folder_id = store.create_folder("Werk")?;
        assert_ne!(replacement_folder_id, folder_id);
        assert_eq!(store.folders()?[0].count, 0);
        assert_eq!(
            store.count_notes(&NoteFilter {
                unfiled: true,
                ..Default::default()
            })?,
            1
        );
        assert_eq!(
            store.count_notes(&NoteFilter {
                unfiled: true,
                archived: true,
                ..Default::default()
            })?,
            1
        );
        Ok(())
    }

    #[test]
    fn moving_pinning_archiving_and_searching_are_consistent() -> Result<()> {
        let store = Store::open(":memory:")?;
        let folder = store.create_folder("Werk")?;
        let id = store.create_note(None)?;
        store.save_note(&id, "**Planning** 100%", "Weekplan")?;
        assert_eq!(store.notes(&NoteFilter { search: "100%".into(), ..Default::default() })?.len(), 1);
        store.move_note(&id, Some(&folder))?;
        store.set_pinned(&id, true)?;
        assert!(store.note(&id)?.unwrap().pinned);
        assert!(store.notes(&NoteFilter { unfiled: true, ..Default::default() })?.is_empty());
        assert_eq!(store.folders()?[0].count, 1);
        store.set_archived(&id, true)?;
        assert!(store.notes(&NoteFilter::default())?.is_empty());
        assert_eq!(store.folders()?[0].count, 0);
        store.set_archived(&id, false)?;
        assert_eq!(store.notes(&NoteFilter::default())?.len(), 1);
        Ok(())
    }

    #[test]
    fn deleting_archived_note_removes_it_permanently() -> Result<()> {
        let store = Store::open(":memory:")?;
        let id = store.create_note(None)?;
        store.save_note(&id, "Weggooien", "Oude notitie")?;
        store.set_archived(&id, true)?;

        store.delete_note(&id)?;

        assert!(store.note(&id)?.is_none());
        assert!(store.notes(&NoteFilter { archived: true, ..Default::default() })?.is_empty());
        Ok(())
    }

    #[test]
    fn restoring_deleted_note_preserves_all_fields() -> Result<()> {
        let store = Store::open(":memory:")?;
        let folder_id = store.create_folder("Werk")?;
        let id = store.create_note(Some(&folder_id))?;
        store.save_note(&id, "Details", "Titel")?;
        store.set_pinned(&id, true)?;
        store.set_archived(&id, true)?;
        let note = store.note(&id)?.unwrap();

        store.delete_note(&id)?;
        store.restore_note(&note)?;

        let restored = store.note(&id)?.unwrap();
        assert_eq!(restored.folder_id.as_deref(), Some(folder_id.as_str()));
        assert_eq!(restored.content, "Details");
        assert_eq!(restored.title, "Titel");
        assert_eq!(restored.updated_at, note.updated_at);
        assert!(restored.pinned);
        assert!(restored.archived);
        Ok(())
    }

    #[test]
    fn invalid_mutations_do_not_destroy_content() -> Result<()> {
        let store = Store::open(":memory:")?;
        assert!(store.create_folder("  ").is_err());
        assert!(store.create_note(Some("missing")).is_err());
        let id = store.create_note(None)?;
        store.save_note(&id, "Bewaren", "Titel")?;
        assert!(store.move_note(&id, Some("missing")).is_err());
        assert_eq!(store.note(&id)?.unwrap().content, "Bewaren");
        assert!(store.save_note("missing", "", "").is_err());
        Ok(())
    }
}
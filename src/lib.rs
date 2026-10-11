use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub mod adapters;
pub mod memo;
pub mod memos;
pub mod credentials;

const NOTE_COLOR_IDS: [&str; 36] = [
    "default", "gray", "mauve", "slate", "sage", "olive", "sand", "tomato", "red", "ruby",
    "crimson", "pink", "plum", "purple", "violet", "iris", "indigo", "blue", "cyan", "teal",
    "jade", "green", "grass", "bronze", "gold", "brown", "orange", "amber", "yellow", "lime",
    "mint", "sky", "rose", "peach", "lemon", "lilac",
];

#[derive(Debug, Serialize)]
pub struct Folder {
    pub id: String,
    pub account_id: Option<String>,
    pub name: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub id: String,
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    pub content: String,
    pub title: String,
    pub updated_at: i64,
    pub pinned: bool,
    pub archived: bool,
}

impl Note {
    pub fn editor_document(&self) -> String {
        let (title, body) = self.editor_parts();
        if self.content.contains('\n') || !body.is_empty() {
            format!("{title}\n{body}")
        } else {
            title.to_owned()
        }
    }

    pub fn editor_parts(&self) -> (&str, &str) {
        let (title, body) = memo::editor_parts(&self.content);
        if self.title.is_empty() || title == self.title || self.content.starts_with("# ") {
            (title, body)
        } else {
            (&self.title, &self.content)
        }
    }
}

/// `account_id: None` selects notes stored only on this device.
#[derive(Default)]
pub struct NoteFilter {
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    pub unfiled: bool,
    pub archived: bool,
    pub search: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FolderSnapshot {
    pub id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NoteSnapshot {
    pub id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    pub content: String,
    pub title: String,
    pub updated_at: i64,
    pub pinned: bool,
    pub archived: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "snapshot", rename_all = "snake_case")]
pub enum SyncEvent {
    FolderCreated { folder: FolderSnapshot },
    FolderUpdated { folder: FolderSnapshot },
    FolderDeleted { folder_id: String },
    NoteCreated { note: NoteSnapshot },
    NoteUpdated { note: NoteSnapshot },
    NoteArchived { note: NoteSnapshot },
    NoteUnarchived { note: NoteSnapshot },
    NoteDeleted { note: NoteSnapshot },
    NoteRestored { note: NoteSnapshot },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SyncOutboxEntry {
    pub sequence: i64,
    pub event_id: String,
    pub created_at: i64,
    pub event: SyncEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackendAccount {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub server_url: String,
    pub enabled: bool,
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
        if version > 9 {
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
                    archived INTEGER NOT NULL DEFAULT 0,
                    color TEXT NOT NULL DEFAULT 'default'
                );
                CREATE INDEX notes_folder ON notes(folder_id);
                PRAGMA user_version = 3;",
            )?;
            transaction.commit()?;
        } else if version == 1 {
            connection.execute_batch(
                "ALTER TABLE notes ADD COLUMN color TEXT NOT NULL DEFAULT 'default';
                 PRAGMA user_version = 3;",
            )?;
        } else if version == 2 {
            connection.execute_batch(
                "UPDATE notes SET color = 'yellow' WHERE color = 'white';
                 PRAGMA user_version = 3;",
            )?;
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT OR IGNORE INTO app_settings (key, value)
            SELECT 'note_color', COALESCE(
                (SELECT color FROM notes
                 WHERE color IN ('yellow', 'rose', 'peach', 'lemon', 'mint', 'sky', 'lilac')
                 ORDER BY updated_at DESC, id LIMIT 1),
                'default'
            );
            CREATE TABLE IF NOT EXISTS sync_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS sync_outbox_created
                ON sync_outbox(sequence);
            CREATE TABLE IF NOT EXISTS backend_accounts (
                id TEXT PRIMARY KEY,
                label TEXT NOT NULL CHECK(length(trim(label)) > 0),
                provider TEXT NOT NULL,
                server_url TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS backend_resource_links (
                account_id TEXT NOT NULL REFERENCES backend_accounts(id) ON DELETE CASCADE,
                remote_name TEXT NOT NULL,
                local_id TEXT NOT NULL,
                PRIMARY KEY (account_id, remote_name)
            );",
        )?;
        if !has_column(&connection, "backend_accounts", "enabled")? {
            connection.execute_batch(
                "ALTER TABLE backend_accounts ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
        // Removing an account keeps its notes and folders on this device.
        for table in ["folders", "notes"] {
            if !has_column(&connection, table, "account_id")? {
                connection.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN account_id TEXT
                     REFERENCES backend_accounts(id) ON DELETE SET NULL;"
                ))?;
            }
        }
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS folders_account ON folders(account_id);
             CREATE INDEX IF NOT EXISTS notes_account ON notes(account_id);",
        )?;
        if version < 9 {
            connection.pragma_update(None, "user_version", 9)?;
        }
        Ok(Self { connection })
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut statement = self.connection.prepare(
            "SELECT folders.id, folders.account_id, folders.name, count(notes.id)
             FROM folders LEFT JOIN notes ON notes.folder_id = folders.id AND notes.archived = 0
             GROUP BY folders.id ORDER BY folders.name COLLATE NOCASE, folders.id",
        )?;
        let folders = statement.query_map([], |row| {
            Ok(Folder {
                id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                count: row.get(3)?,
            })
        })?;
        Ok(folders.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn has_local_data(&self) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM folders WHERE account_id IS NULL)
                 OR EXISTS(SELECT 1 FROM notes WHERE account_id IS NULL)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn create_folder(&self, account_id: Option<&str>, name: &str) -> Result<String> {
        if name.trim().is_empty() {
            bail!("Een folder heeft een naam nodig");
        }
        let id = Uuid::new_v4().to_string();
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO folders (id, account_id, name) VALUES (?1, ?2, ?3)",
            params![id, account_id, name.trim()],
        )?;
        append_sync_event(
            &transaction,
            SyncEvent::FolderCreated {
                folder: FolderSnapshot {
                    id: id.clone(),
                    account_id: account_id.map(str::to_owned),
                    name: name.trim().to_owned(),
                },
            },
        )?;
        transaction.commit()?;
        Ok(id)
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("Een folder heeft een naam nodig");
        }
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![id, name.trim()],
        )?;
        self.changed(changed)?;
        let folder = transaction.query_row(
            "SELECT id, account_id, name FROM folders WHERE id = ?1",
            [id],
            |row| {
                Ok(FolderSnapshot {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    name: row.get(2)?,
                })
            },
        )?;
        append_sync_event(&transaction, SyncEvent::FolderUpdated { folder })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_folder(&mut self, id: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "SELECT id, folder_id, content, title, updated_at, pinned, archived, account_id
             FROM notes WHERE folder_id = ?1 ORDER BY id",
        )?;
        let moved_notes = statement
            .query_map([id], note_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let changed_at = timestamp()?;
        transaction.execute(
            "UPDATE notes SET folder_id = NULL, updated_at = ?2 WHERE folder_id = ?1",
            params![id, changed_at],
        )?;
        let deleted = transaction.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        self.changed(deleted)?;
        for mut note in moved_notes {
            note.folder_id = None;
            note.updated_at = changed_at;
            append_sync_event(&transaction, SyncEvent::NoteUpdated { note: note.into() })?;
        }
        append_sync_event(
            &transaction,
            SyncEvent::FolderDeleted { folder_id: id.to_owned() },
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_note(&self, account_id: Option<&str>, folder_id: Option<&str>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let transaction = self.connection.unchecked_transaction()?;
        if let Some(folder_id) = folder_id {
            ensure_folder_in_account(&transaction, folder_id, account_id)?;
        }
        let updated_at = timestamp()?;
        transaction.execute(
            "INSERT INTO notes (id, account_id, folder_id, title, updated_at)
             VALUES (?1, ?2, ?3, '', ?4)",
            params![id, account_id, folder_id, updated_at],
        )?;
        let note = load_note(&transaction, &id)?;
        append_sync_event(&transaction, SyncEvent::NoteCreated { note: note.into() })?;
        transaction.commit()?;
        Ok(id)
    }

    pub fn save_note(&self, id: &str, content: &str, title: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let current = load_note(&transaction, id)?;
        if current.content == content && current.title == title.trim() {
            return Ok(());
        }
        let changed = transaction.execute(
            "UPDATE notes SET content = ?2, title = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, content, title.trim(), timestamp()?],
        )?;
        self.changed(changed)?;
        let note = load_note(&transaction, id)?;
        append_sync_event(&transaction, SyncEvent::NoteUpdated { note: note.into() })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_note_editor(&self, id: &str, title: &str, body: &str) -> Result<()> {
        let content = memo::editor_content(title, body);
        self.save_note(id, &content, memo::editor_parts(&content).0)
    }

    pub fn move_note(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        if let Some(folder_id) = folder_id {
            let note = load_note(&transaction, id)?;
            ensure_folder_in_account(&transaction, folder_id, note.account_id.as_deref())?;
        }
        let changed = transaction.execute(
            "UPDATE notes SET folder_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, folder_id, timestamp()?],
        )?;
        self.changed(changed)?;
        let note = load_note(&transaction, id)?;
        append_sync_event(&transaction, SyncEvent::NoteUpdated { note: note.into() })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE notes SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )?;
        self.changed(changed)?;
        let note = load_note(&transaction, id)?;
        append_sync_event(&transaction, SyncEvent::NoteUpdated { note: note.into() })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn note_color(&self) -> Result<String> {
        Ok(self.connection.query_row(
            "SELECT value FROM app_settings WHERE key = 'note_color'",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn set_note_color(&self, color: &str) -> Result<()> {
        if !NOTE_COLOR_IDS.contains(&color) {
            bail!("Ongeldige notitiekleur");
        }
        self.connection.execute(
            "INSERT INTO app_settings (key, value) VALUES ('note_color', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [color],
        )?;
        Ok(())
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE notes SET archived = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, archived, timestamp()?],
        )?;
        self.changed(changed)?;
        let note = load_note(&transaction, id)?;
        let event = if archived {
            SyncEvent::NoteArchived { note: note.into() }
        } else {
            SyncEvent::NoteUnarchived { note: note.into() }
        };
        append_sync_event(&transaction, event)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_note(&self, id: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let note = load_note(&transaction, id)?;
        self.changed(transaction.execute("DELETE FROM notes WHERE id = ?1", [id])?)?;
        append_sync_event(
            &transaction,
            SyncEvent::NoteDeleted { note: note.into() },
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn restore_note(&self, note: &Note) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO notes
                (id, folder_id, content, title, updated_at, pinned, archived, account_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                note.id,
                note.folder_id,
                note.content,
                note.title,
                note.updated_at,
                note.pinned,
                note.archived,
                note.account_id,
            ],
        )?;
        let note = load_note(&transaction, &note.id)?;
        append_sync_event(&transaction, SyncEvent::NoteRestored { note: note.into() })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pending_sync_events(&self, limit: usize) -> Result<Vec<SyncOutboxEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_id, payload, created_at
             FROM sync_outbox ORDER BY sequence LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.clamp(1, 1000) as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (sequence, event_id, payload, created_at) = row?;
            entries.push(SyncOutboxEntry {
                sequence,
                event_id,
                created_at,
                event: serde_json::from_str(&payload)?,
            });
        }
        Ok(entries)
    }

    pub fn acknowledge_sync_event(&self, sequence: i64) -> Result<()> {
        self.changed(
            self.connection
                .execute("DELETE FROM sync_outbox WHERE sequence = ?1", [sequence])?,
        )
    }

    pub fn sync_backend_events(
        &self,
        account: &BackendAccount,
        adapter: &dyn adapters::BackendAdapter,
    ) -> Result<usize> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, payload FROM sync_outbox ORDER BY sequence",
        )?;
        let pending = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut batches: Vec<(Vec<i64>, SyncEvent)> = Vec::new();
        for (sequence, payload) in pending {
            let event: SyncEvent = serde_json::from_str(&payload)?;
            let owner = match &event {
                SyncEvent::FolderCreated { folder } | SyncEvent::FolderUpdated { folder } => folder.account_id.clone(),
                SyncEvent::FolderDeleted { folder_id } => self.connection.query_row(
                    "SELECT account_id FROM backend_resource_links WHERE local_id = ?1 AND remote_name LIKE 'spaces/%'",
                    [folder_id], |row| row.get::<_, String>(0),
                ).optional()?,
                SyncEvent::NoteCreated { note } | SyncEvent::NoteUpdated { note }
                | SyncEvent::NoteArchived { note } | SyncEvent::NoteUnarchived { note }
                | SyncEvent::NoteDeleted { note } | SyncEvent::NoteRestored { note } => note.account_id.clone(),
            };
            if owner.as_deref() != Some(account.id.as_str()) {
                continue;
            }
            if let Some((sequences, previous)) = batches.last_mut() {
                if matches!((&*previous, &event),
                    (SyncEvent::NoteUpdated { note: before }, SyncEvent::NoteUpdated { note: after })
                    if before.id == after.id)
                {
                    sequences.push(sequence);
                    *previous = event;
                    continue;
                }
            }
            batches.push((vec![sequence], event));
        }
        let mut sent = 0;
        for (sequences, mut event) in batches {
            if !account.enabled || account.provider != "memos"
                || !self.backend_accounts()?.iter().any(|current| current == account)
            {
                bail!("Het Memos-account is gewijzigd of uitgeschakeld; verzending gestopt");
            }
            let creates_note = matches!(&event, SyncEvent::NoteCreated { .. } | SyncEvent::NoteRestored { .. });
            let created_link = match &mut event {
                SyncEvent::FolderCreated { folder } => {
                    let local_id = folder.id.clone();
                    folder.id = self.backend_resource_name(&account.id, &local_id, "spaces")?;
                    Some((local_id, folder.id.clone()))
                }
                SyncEvent::FolderUpdated { folder } => {
                    folder.id = self.backend_resource_name(&account.id, &folder.id, "spaces")?;
                    None
                }
                SyncEvent::FolderDeleted { folder_id } => {
                    *folder_id = self.backend_resource_name(&account.id, folder_id, "spaces")?;
                    None
                }
                SyncEvent::NoteCreated { note } | SyncEvent::NoteRestored { note }
                | SyncEvent::NoteUpdated { note } | SyncEvent::NoteArchived { note }
                | SyncEvent::NoteUnarchived { note } | SyncEvent::NoteDeleted { note } => {
                    let local_id = note.id.clone();
                    note.id = self.backend_resource_name(&account.id, &local_id, "memos")?;
                    if let Some(folder_id) = &mut note.folder_id {
                        *folder_id = self.backend_resource_name(&account.id, folder_id, "spaces")?;
                    }
                    if creates_note {
                        Some((local_id, note.id.clone()))
                    } else { None }
                }
            };
            adapter.apply_event(&event)?;
            let transaction = self.connection.unchecked_transaction()?;
            if let Some((local_id, remote_name)) = created_link {
                transaction.execute(
                    "INSERT INTO backend_resource_links (account_id, remote_name, local_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT(account_id, remote_name) DO UPDATE SET local_id = excluded.local_id",
                    params![account.id, remote_name, local_id],
                )?;
            }
            for sequence in sequences {
                self.changed(transaction.execute("DELETE FROM sync_outbox WHERE sequence = ?1", [sequence])?)?;
            }
            transaction.commit()?;
            sent += 1;
        }
        Ok(sent)
    }

    pub fn has_backend_events(&self, account_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_outbox
             WHERE json_extract(payload, '$.snapshot.note.account_id') = ?1
                OR json_extract(payload, '$.snapshot.folder.account_id') = ?1
                OR (json_extract(payload, '$.type') = 'folder_deleted' AND EXISTS(
                    SELECT 1 FROM backend_resource_links WHERE account_id = ?1
                    AND local_id = json_extract(payload, '$.snapshot.folder_id')
                    AND remote_name LIKE 'spaces/%')))",
            [account_id], |row| row.get(0),
        )?)
    }

    pub fn has_pending_note_events(&self, note_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_outbox
             WHERE json_extract(payload, '$.snapshot.note.id') = ?1)",
            [note_id],
            |row| row.get(0),
        )?)
    }

    pub fn outbox_revision(&self) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'sync_outbox'), 0)",
            [], |row| row.get(0),
        )?)
    }

    fn backend_resource_name(&self, account_id: &str, local_id: &str, collection: &str) -> Result<String> {
        Ok(self.connection.query_row(
            "SELECT remote_name FROM backend_resource_links
             WHERE account_id = ?1 AND local_id = ?2 AND remote_name LIKE ?3",
            params![account_id, local_id, format!("{collection}/%")],
            |row| row.get::<_, String>(0),
        ).optional()?.unwrap_or_else(|| {
            if local_id.starts_with(&format!("{collection}/")) { local_id.to_owned() }
            else { format!("{collection}/{local_id}") }
        }))
    }

    pub fn backend_memo_name(&self, account_id: &str, note_id: &str) -> Result<String> {
        self.backend_resource_name(account_id, note_id, "memos")
    }

    pub fn backend_folder_name(&self, account_id: &str, folder_id: &str) -> Result<String> {
        self.backend_resource_name(account_id, folder_id, "spaces")
    }

    pub fn backend_accounts(&self) -> Result<Vec<BackendAccount>> {
        let mut statement = self.connection.prepare(
            "SELECT id, label, provider, server_url, enabled FROM backend_accounts
             ORDER BY label COLLATE NOCASE, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(BackendAccount {
                id: row.get(0)?,
                label: row.get(1)?,
                provider: row.get(2)?,
                server_url: row.get(3)?,
                enabled: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_backend_account(
        &self,
        id: &str,
        label: &str,
        provider: &str,
        server_url: &str,
    ) -> Result<()> {
        self.add_backend_account_with_state(id, label, provider, server_url, true)
    }

    pub fn add_backend_account_with_state(
        &self,
        id: &str,
        label: &str,
        provider: &str,
        server_url: &str,
        enabled: bool,
    ) -> Result<()> {
        if id.trim().is_empty()
            || label.trim().is_empty()
            || provider.trim().is_empty()
            || server_url.trim().is_empty()
        {
            bail!("Accountnaam, backend en serveradres zijn verplicht");
        }
        self.connection.execute(
            "INSERT INTO backend_accounts (id, label, provider, server_url, created_at, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, label.trim(), provider, server_url.trim(), timestamp()?, enabled],
        )?;
        Ok(())
    }

    pub fn set_backend_account_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.changed(self.connection.execute(
            "UPDATE backend_accounts SET enabled = ?2 WHERE id = ?1",
            params![id, enabled],
        )?)
    }

    pub fn update_backend_account(&self, id: &str, label: &str, server_url: &str) -> Result<()> {
        if label.trim().is_empty() || server_url.trim().is_empty() {
            bail!("Accountnaam en serveradres zijn verplicht");
        }
        self.changed(self.connection.execute(
            "UPDATE backend_accounts SET label = ?2, server_url = ?3 WHERE id = ?1",
            params![id, label.trim(), server_url.trim()],
        )?)
    }

    pub fn remove_backend_account(&self, id: &str) -> Result<()> {
        self.changed(
            self.connection
                .execute("DELETE FROM backend_accounts WHERE id = ?1", [id])?,
        )
    }

    pub fn notes(&self, filter: &NoteFilter) -> Result<Vec<Note>> {
        let mut statement = self.connection.prepare(
            "SELECT id, folder_id, content, title, updated_at, pinned, archived, account_id
             FROM notes
             WHERE archived = ?1 AND (?2 IS NULL OR folder_id = ?2)
             AND (?3 = 0 OR folder_id IS NULL)
             AND (?4 = '' OR instr(lower(title || char(10) || content), lower(?4)) > 0)
             AND account_id IS ?5
             ORDER BY pinned DESC, updated_at DESC, id",
        )?;
        let notes = statement.query_map(
            params![
                filter.archived,
                filter.folder_id,
                filter.unfiled,
                filter.search,
                filter.account_id,
            ],
            note_from_row,
        )?;
        Ok(notes.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_notes(&self, filter: &NoteFilter) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM notes
             WHERE archived = ?1 AND (?2 IS NULL OR folder_id = ?2)
             AND (?3 = 0 OR folder_id IS NULL)
             AND (?4 = '' OR instr(lower(title || char(10) || content), lower(?4)) > 0)
             AND account_id IS ?5",
            params![
                filter.archived,
                filter.folder_id,
                filter.unfiled,
                filter.search,
                filter.account_id,
            ],
            |row| row.get(0),
        )?)
    }

    pub fn note(&self, id: &str) -> Result<Option<Note>> {
        Ok(self.connection.query_row(
            "SELECT id, folder_id, content, title, updated_at, pinned, archived, account_id
             FROM notes WHERE id = ?1",
            [id], note_from_row,
        ).optional()?)
    }

    pub fn import_backend_snapshot(
        &self,
        account_id: &str,
        folders: &[memo::MemoFolder],
        memos: &[memo::Memo],
    ) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let valid_account: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM backend_accounts WHERE id = ?1 AND enabled = 1 AND provider = 'memos')",
            [account_id], |row| row.get(0),
        )?;
        if !valid_account {
            bail!("Het Memos-account is verwijderd of uitgeschakeld");
        }
        let mut protected = HashSet::new();
        let mut statement = transaction.prepare("SELECT payload FROM sync_outbox")?;
        let payloads = statement.query_map([], |row| row.get::<_, String>(0))?;
        for payload in payloads {
            let event: SyncEvent = serde_json::from_str(&payload?)?;
            let id = match event {
                SyncEvent::FolderCreated { folder } | SyncEvent::FolderUpdated { folder } => {
                    if folder.account_id.as_deref() != Some(account_id) { continue; }
                    folder.id
                }
                SyncEvent::FolderDeleted { folder_id } => folder_id,
                SyncEvent::NoteCreated { note } | SyncEvent::NoteUpdated { note }
                | SyncEvent::NoteArchived { note } | SyncEvent::NoteUnarchived { note }
                | SyncEvent::NoteDeleted { note } | SyncEvent::NoteRestored { note } => {
                    if note.account_id.as_deref() != Some(account_id) { continue; }
                    note.id
                }
            };
            protected.insert(id);
        }
        drop(statement);
        let mut folder_ids = HashMap::new();
        for folder in folders {
            let id = cached_resource_id(&transaction, account_id, &folder.id, "folders", "spaces", &protected)?;
            if !protected.contains(&id) {
                transaction.execute(
                    "INSERT INTO folders (id, account_id, name) VALUES (?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                    params![id, account_id, folder.name],
                )?;
            }
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM folders WHERE id = ?1 AND account_id = ?2)",
                params![id, account_id], |row| row.get(0),
            )?;
            if exists {
                folder_ids.insert(folder.id.clone(), id);
            }
        }
        for memo in memos {
            let id = cached_resource_id(&transaction, account_id, &memo.id, "notes", "memos", &protected)?;
            if protected.contains(&id) {
                continue;
            }
            let folder_id = match memo.folder_id.as_deref() {
                Some(name) => match folder_ids.get(name) {
                    Some(id) => Some(id.clone()),
                    None => transaction.query_row(
                        "SELECT local_id FROM backend_resource_links WHERE account_id = ?1 AND remote_name = ?2",
                        params![account_id, name], |row| row.get::<_, String>(0),
                    ).optional()?.or(transaction.query_row(
                        "SELECT folder_id FROM notes WHERE id = ?1 AND account_id = ?2",
                        params![id, account_id], |row| row.get::<_, Option<String>>(0),
                    ).optional()?.flatten()),
                },
                None => None,
            };
            let updated_at = memo.updated_at.unwrap_or_default();
            transaction.execute(
                "INSERT INTO notes (id, account_id, folder_id, content, title, updated_at, pinned, archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET folder_id = excluded.folder_id,
                     content = excluded.content, title = excluded.title,
                     updated_at = excluded.updated_at, pinned = excluded.pinned, archived = excluded.archived",
                params![id, account_id, folder_id, memo.content, memo::editor_parts(&memo.content).0,
                    updated_at, memo.pinned, memo.archived],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn import_backend_memo(&self, account_id: &str, memo: &memo::Memo) -> Result<()> {
        self.import_backend_snapshot(account_id, &[], std::slice::from_ref(memo))
    }

    fn changed(&self, count: usize) -> Result<()> {
        if count != 1 {
            bail!("De notitie of folder bestaat niet meer");
        }
        Ok(())
    }
}

fn cached_resource_id(
    connection: &Connection,
    account_id: &str,
    remote_name: &str,
    table: &str,
    collection: &str,
    protected: &HashSet<String>,
) -> Result<String> {
    let suffix = remote_name.strip_prefix(&format!("{collection}/"))
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .context("Memos gaf een ongeldige resourcenaam terug")?;
    if let Some(id) = connection.query_row(
        "SELECT local_id FROM backend_resource_links WHERE account_id = ?1 AND remote_name = ?2",
        params![account_id, remote_name], |row| row.get::<_, String>(0),
    ).optional()? {
        return Ok(id);
    }
    let existing = connection.query_row(
        &format!("SELECT id FROM {table} WHERE id = ?1 AND account_id = ?2"),
        params![suffix, account_id], |row| row.get::<_, String>(0),
    ).optional()?;
    let id = existing.unwrap_or_else(|| {
        if protected.contains(suffix) { suffix.to_owned() } else { Uuid::new_v4().to_string() }
    });
    connection.execute(
        "INSERT INTO backend_resource_links (account_id, remote_name, local_id) VALUES (?1, ?2, ?3)",
        params![account_id, remote_name, id],
    )?;
    Ok(id)
}

impl From<Note> for NoteSnapshot {
    fn from(note: Note) -> Self {
        Self {
            id: note.id,
            account_id: note.account_id,
            folder_id: note.folder_id,
            content: note.content,
            title: note.title,
            updated_at: note.updated_at,
            pinned: note.pinned,
            archived: note.archived,
        }
    }
}

fn append_sync_event(connection: &Connection, event: SyncEvent) -> Result<()> {
    connection.execute(
        "INSERT INTO sync_outbox (event_id, payload, created_at) VALUES (?1, ?2, ?3)",
        params![Uuid::new_v4().to_string(), serde_json::to_string(&event)?, timestamp()?],
    )?;
    Ok(())
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn ensure_folder_in_account(
    connection: &Connection,
    folder_id: &str,
    account_id: Option<&str>,
) -> Result<()> {
    let folder_account: Option<Option<String>> = connection
        .query_row("SELECT account_id FROM folders WHERE id = ?1", [folder_id], |row| {
            row.get(0)
        })
        .optional()?;
    match folder_account {
        None => bail!("De map bestaat niet meer"),
        Some(folder_account) if folder_account.as_deref() != account_id => {
            bail!("De map hoort bij een ander account")
        }
        Some(_) => Ok(()),
    }
}

fn load_note(connection: &Connection, id: &str) -> Result<Note> {
    Ok(connection.query_row(
        "SELECT id, folder_id, content, title, updated_at, pinned, archived, account_id
         FROM notes WHERE id = ?1",
        [id],
        note_from_row,
    )?)
}

fn timestamp() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?)
}

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        content: row.get(2)?,
        title: row.get(3)?,
        updated_at: row.get(4)?,
        pinned: row.get(5)?,
        archived: row.get(6)?,
        account_id: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_title_and_body_share_one_synced_document() -> Result<()> {
        let store = Store::open(":memory:")?;
        let id = store.create_note(None, None)?;
        store.save_note(&id, "Bestaande tekst", "Losse titel")?;
        let legacy = store.note(&id)?.unwrap();
        assert_eq!(legacy.editor_parts(), ("Losse titel", "Bestaande tekst"));
        assert_eq!(legacy.editor_document(), "Losse titel\nBestaande tekst");
        store.save_note_editor(&id, legacy.editor_parts().0, legacy.editor_parts().1)?;
        let note = store.note(&id)?.unwrap();
        assert_eq!(note.content, "# Losse titel\nBestaande tekst");
        assert_eq!(note.editor_parts(), ("Losse titel", "Bestaande tekst"));
        let pending = store.pending_sync_events(100)?;
        store.save_note_editor(&id, "Losse titel", "Bestaande tekst")?;
        assert_eq!(store.pending_sync_events(100)?, pending);
        store.save_note_editor(&id, "Nieuwe titel", "Bestaande tekst")?;
        let note = store.note(&id)?.unwrap();
        assert_eq!(note.title, "Nieuwe titel");
        assert_eq!(note.content, "# Nieuwe titel\nBestaande tekst");
        assert_eq!(note.editor_document(), "Nieuwe titel\nBestaande tekst");
        let pending = store.pending_sync_events(100)?;
        assert!(matches!(&pending.last().unwrap().event, SyncEvent::NoteUpdated { note }
            if note.content == "# Nieuwe titel\nBestaande tekst"));
        let document = "Gewijzigd in teksteditor\n\nBestaande tekst";
        let (title, body) = memo::editor_parts(document);
        store.save_note_editor(&id, title, body)?;
        let note = store.note(&id)?.unwrap();
        assert_eq!(note.title, "Gewijzigd in teksteditor");
        assert_eq!(note.editor_document(), "Gewijzigd in teksteditor\n\nBestaande tekst");
        for document in ["Titel", "Titel\n\nTekst met #hashtag", "#hashtag\nTekst", "", "\nTekst"] {
            let (title, body) = memo::editor_parts(document);
            store.save_note_editor(&id, title, body)?;
            let note = store.note(&id)?.unwrap();
            assert_eq!(note.editor_document(), document);
            let pending = store.pending_sync_events(100)?;
            let loaded = note.editor_document();
            let (title, body) = memo::editor_parts(&loaded);
            store.save_note_editor(&id, title, body)?;
            assert_eq!(store.pending_sync_events(100)?, pending);
            assert_eq!(store.note(&id)?.unwrap().content, note.content);
        }
        Ok(())
    }

    #[test]
    fn imported_title_comes_from_first_line_not_api_property() -> Result<()> {
        let store = Store::open(":memory:")?;
        store.add_backend_account("one", "Werk", "memos", "https://memos.example.com")?;
        let mut remote = memo::Memo {
            id: "memos/title".to_owned(), content: "Eerste regel\n\n# Latere kop".to_owned(),
            title: Some("Latere kop".to_owned()), folder_id: None,
            pinned: false, archived: false, updated_at: None,
        };
        store.import_backend_memo("one", &remote)?;
        let note = store.notes(&NoteFilter { account_id: Some("one".to_owned()), ..Default::default() })?.remove(0);
        assert_eq!(note.title, "Eerste regel");
        assert_eq!(note.editor_parts(), ("Eerste regel", "\n# Latere kop"));
        assert_eq!(note.editor_document(), remote.content);
        assert!(store.pending_sync_events(100)?.is_empty());
        remote.content = "# Nieuwe titel\nTekst".to_owned();
        store.import_backend_memo("one", &remote)?;
        let note = store.note(&note.id)?.unwrap();
        assert_eq!(note.title, "Nieuwe titel");
        assert_eq!(note.editor_parts(), ("Nieuwe titel", "Tekst"));
        assert_eq!(note.editor_document(), "Nieuwe titel\nTekst");
        Ok(())
    }

    #[test]
    fn folders_and_rich_content_survive_reopening() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let content = "# Boodschappen\n\n**Melk** en *brood*\n\n- [x] Koffie\n- [ ] Thee\n\n![Foto](assets/foto.png)";
        let id;
        {
            let store = Store::open(&path)?;
            let folder = store.create_folder(None, "Privé")?;
            id = store.create_note(None, Some(&folder))?;
            store.save_note(&id, content, "Boodschappen")?;
            store.set_note_color("mint")?;
        }
        let store = Store::open(&path)?;
        let schema_version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(schema_version, 9);
        assert_eq!(store.folders()?[0].count, 1);
        let note = store.note(&id)?.unwrap();
        assert_eq!(note.content, content);
        assert_eq!(store.note_color()?, "mint");
        Ok(())
    }

    #[test]
    fn backend_import_is_persistent_account_scoped_and_preserves_pending_changes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let store = Store::open(&path)?;
        for account_id in ["one", "two"] {
            store.add_backend_account(account_id, account_id, "memos", "https://memos.example.com")?;
        }
        let folders = vec![memo::MemoFolder { id: "spaces/work".to_owned(), name: "Werk".to_owned() }];
        let mut memos = vec![memo::Memo {
            id: "memos/remote-one".to_owned(), content: "# Server\nInhoud".to_owned(),
            title: Some("Server".to_owned()), folder_id: Some("spaces/work".to_owned()),
            pinned: true, archived: false, updated_at: Some(1791633600000),
        }];
        for account_id in ["one", "two"] {
            store.import_backend_snapshot(account_id, &folders, &memos)?;
        }
        let second_account_memo = memo::Memo {
            updated_at: Some(1791720000000),
            ..memos[0].clone()
        };
        store.import_backend_memo("two", &second_account_memo)?;
        assert!(store.pending_sync_events(100)?.is_empty());
        let filter = NoteFilter { account_id: Some("one".to_owned()), ..Default::default() };
        let first = store.notes(&filter)?.remove(0);
        assert_eq!(first.updated_at, 1791633600000);
        assert!(first.pinned);
        let mut focused_memo = memos[0].clone();
        focused_memo.content = "Bijgewerkt via gerichte GET".to_owned();
        store.import_backend_memo("one", &focused_memo)?;
        assert_eq!(store.note(&first.id)?.unwrap().content, "Bijgewerkt via gerichte GET");
        assert_eq!(store.note(&first.id)?.unwrap().folder_id, first.folder_id);
        assert!(!store.has_pending_note_events(&first.id)?);
        let second = store.notes(&NoteFilter { account_id: Some("two".to_owned()), ..Default::default() })?.remove(0);
        assert_eq!(second.updated_at, 1791720000000);
        assert_ne!(first.updated_at, second.updated_at);
        assert_ne!(first.id, second.id);
        assert_ne!(first.folder_id, second.folder_id);
        assert_eq!(store.backend_folder_name("one", first.folder_id.as_deref().unwrap())?, "spaces/work");
        assert_eq!(store.backend_folder_name("two", second.folder_id.as_deref().unwrap())?, "spaces/work");
        store.save_note(&first.id, "Lokale wijziging", "Lokaal")?;
        assert!(store.has_pending_note_events(&first.id)?);
        store.rename_folder(first.folder_id.as_deref().unwrap(), "Lokale mapnaam")?;
        let pending = store.pending_sync_events(100)?;
        memos[0].content = "Gewijzigd op server".to_owned();
        store.import_backend_snapshot("one", &folders, &memos)?;
        assert_eq!(store.notes(&filter)?.len(), 1);
        assert_eq!(store.note(&first.id)?.unwrap().content, "Lokale wijziging");
        assert_eq!(store.folders()?.into_iter().find(|folder| folder.id == first.folder_id.as_deref().unwrap()).unwrap().name, "Lokale mapnaam");
        assert_eq!(store.pending_sync_events(100)?, pending);
        store.delete_note(&first.id)?;
        store.import_backend_snapshot("one", &folders, &memos)?;
        assert!(store.note(&first.id)?.is_none());
        assert!(store.notes(&filter)?.is_empty());
        drop(store);
        let store = Store::open(&path)?;
        store.import_backend_snapshot("two", &folders, &memos)?;
        assert_eq!(store.note(&second.id)?.unwrap().content, "Gewijzigd op server");
        assert_eq!(store.notes(&NoteFilter { account_id: Some("two".to_owned()), ..Default::default() })?.len(), 1);
        Ok(())
    }

    #[test]
    fn backend_import_rolls_back_invalid_data_and_rejects_disabled_accounts() -> Result<()> {
        let store = Store::open(":memory:")?;
        store.add_backend_account("one", "Werk", "memos", "https://memos.example.com")?;
        let folders = vec![memo::MemoFolder { id: "spaces/work".to_owned(), name: "Werk".to_owned() }];
        let invalid = memo::Memo {
            id: "invalid".to_owned(), content: "Inhoud".to_owned(), title: None,
            folder_id: None, pinned: false, archived: false, updated_at: None,
        };
        assert!(store.import_backend_snapshot("one", &folders, &[invalid]).is_err());
        assert!(store.folders()?.is_empty());
        let links: i64 = store.connection.query_row("SELECT count(*) FROM backend_resource_links", [], |row| row.get(0))?;
        assert_eq!(links, 0);
        store.set_backend_account_enabled("one", false)?;
        assert!(store.import_backend_snapshot("one", &folders, &[]).is_err());
        Ok(())
    }

    #[test]
    fn saving_unchanged_note_does_not_enqueue_event_or_change_timestamp() -> Result<()> {
        let store = Store::open(":memory:")?;
        let id = store.create_note(None, None)?;
        store.save_note(&id, "Inhoud", "Titel")?;
        let before = store.note(&id)?.unwrap();
        let pending = store.pending_sync_events(100)?;
        store.save_note(&id, "Inhoud", "  Titel  ")?;
        assert_eq!(store.pending_sync_events(100)?, pending);
        assert_eq!(store.note(&id)?.unwrap().updated_at, before.updated_at);
        store.save_note(&id, "Andere inhoud", "Titel")?;
        assert_eq!(store.pending_sync_events(100)?.len(), pending.len() + 1);
        store.save_note(&id, "Andere inhoud", "Andere titel")?;
        assert_eq!(store.pending_sync_events(100)?.len(), pending.len() + 2);
        assert!(store.save_note("missing", "", "").is_err());
        Ok(())
    }

    #[test]
    fn sync_events_persist_ordered_snapshots_and_acknowledgements() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let folder_id;
        {
            let store = Store::open(&path)?;
            folder_id = store.create_folder(None, "Werk")?;
            store.rename_folder(&folder_id, "Project")?;
            let note_id = store.create_note(None, Some(&folder_id))?;
            store.save_note(&note_id, "Inhoud", "Plan")?;
            store.set_pinned(&note_id, true)?;
            store.set_archived(&note_id, true)?;
            store.set_archived(&note_id, false)?;
            let note = store.note(&note_id)?.unwrap();
            store.delete_note(&note_id)?;
            store.restore_note(&note)?;

            let events = store.pending_sync_events(100)?;
            assert_eq!(events.len(), 9);
            assert!(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
            assert!(events.iter().all(|event| !event.event_id.is_empty()));
            assert!(matches!(events[0].event, SyncEvent::FolderCreated { .. }));
            assert!(matches!(events[1].event, SyncEvent::FolderUpdated { .. }));
            assert!(matches!(events[2].event, SyncEvent::NoteCreated { .. }));
            assert!(matches!(events[3].event, SyncEvent::NoteUpdated { .. }));
            assert!(matches!(events[4].event, SyncEvent::NoteUpdated { .. }));
            assert!(matches!(events[5].event, SyncEvent::NoteArchived { .. }));
            assert!(matches!(events[6].event, SyncEvent::NoteUnarchived { .. }));
            assert!(matches!(events[7].event, SyncEvent::NoteDeleted { .. }));
            assert!(matches!(events[8].event, SyncEvent::NoteRestored { .. }));

            store.acknowledge_sync_event(events[0].sequence)?;
            assert_eq!(store.pending_sync_events(100)?.len(), 8);
        }

        let store = Store::open(&path)?;
        let pending = store.pending_sync_events(100)?;
        assert_eq!(pending.len(), 8);
        assert!(matches!(pending[0].event, SyncEvent::FolderUpdated { .. }));
        assert!(matches!(pending[1].event, SyncEvent::NoteCreated { .. }));
        assert!(!folder_id.is_empty());
        Ok(())
    }

    #[test]
    fn backend_account_metadata_persists_without_secret_values() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        {
            let store = Store::open(&path)?;
            store.add_backend_account_with_state(
                "account-1",
                "Werk",
                "memos",
                "https://memos.example.com",
                false,
            )?;
        }

        let store = Store::open(&path)?;
        let account = store.backend_accounts()?.remove(0);
        assert!(!account.enabled);
        store.set_backend_account_enabled(&account.id, true)?;
        store.update_backend_account(
            &account.id,
            "Werk bijgewerkt",
            "https://memos-new.example.com",
        )?;
        assert_eq!(
            store.backend_accounts()?,
            vec![BackendAccount {
                id: "account-1".into(),
                label: "Werk bijgewerkt".into(),
                provider: "memos".into(),
                server_url: "https://memos-new.example.com".into(),
                enabled: true,
            }]
        );
        store.remove_backend_account("account-1")?;
        assert!(store.backend_accounts()?.is_empty());
        Ok(())
    }

    #[test]
    fn opening_version_six_database_adds_online_state_to_accounts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE folders (id TEXT PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                folder_id TEXT,
                content TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                color TEXT NOT NULL DEFAULT 'default'
             );
             CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sync_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE backend_accounts (
                id TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                provider TEXT NOT NULL,
                server_url TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             INSERT INTO backend_accounts VALUES
                ('account-1', 'Werk', 'memos', 'https://memos.example.com', 1);
             PRAGMA user_version = 6;",
        )?;
        drop(connection);

        let store = Store::open(&path)?;
        let account = store.backend_accounts()?.remove(0);
        assert!(account.enabled);
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 9);
        Ok(())
    }

    #[test]
    fn opening_version_six_database_with_existing_enabled_column_is_safe() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        {
            let store = Store::open(&path)?;
            store.add_backend_account_with_state(
                "account-1",
                "Werk",
                "memos",
                "https://memos.example.com",
                false,
            )?;
        }
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "user_version", 6)?;
        drop(connection);

        let store = Store::open(&path)?;
        let account = store.backend_accounts()?.remove(0);
        assert!(!account.enabled);
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 9);
        Ok(())
    }

    #[test]
    fn deleting_folder_queues_note_moves_before_folder_delete() -> Result<()> {
        let mut store = Store::open(":memory:")?;
        let folder_id = store.create_folder(None, "Werk")?;
        let note_id = store.create_note(None, Some(&folder_id))?;

        store.delete_folder(&folder_id)?;

        let events = store.pending_sync_events(100)?;
        assert!(matches!(events[2].event, SyncEvent::NoteUpdated { .. }));
        assert!(matches!(
            &events[2].event,
            SyncEvent::NoteUpdated { note } if note.id == note_id && note.folder_id.is_none()
        ));
        assert!(matches!(
            &events[3].event,
            SyncEvent::FolderDeleted { folder_id: deleted_id } if deleted_id == &folder_id
        ));
        Ok(())
    }

    #[test]
    fn folders_and_notes_are_scoped_per_account() -> Result<()> {
        let store = Store::open(":memory:")?;
        store.add_backend_account("work", "Werk", "memos", "https://work.example.com")?;
        store.add_backend_account("home", "Thuis", "memos", "https://home.example.com")?;
        let work_folder = store.create_folder(Some("work"), "Project")?;
        let work_note = store.create_note(Some("work"), Some(&work_folder))?;
        let home_note = store.create_note(Some("home"), None)?;

        assert!(store.create_note(Some("home"), Some(&work_folder)).is_err());
        assert!(store.move_note(&home_note, Some(&work_folder)).is_err());
        let work = NoteFilter { account_id: Some("work".into()), ..Default::default() };
        let home = NoteFilter { account_id: Some("home".into()), ..Default::default() };
        assert_eq!(store.notes(&work)?[0].id, work_note);
        assert_eq!(store.notes(&home)?[0].id, home_note);
        assert_eq!(store.count_notes(&NoteFilter::default())?, 0);
        assert!(!store.has_local_data()?);

        store.remove_backend_account("work")?;
        assert_eq!(store.note(&work_note)?.unwrap().account_id, None);
        assert_eq!(store.folders()?[0].account_id, None);
        assert!(store.has_local_data()?);
        Ok(())
    }

    #[test]
    fn note_color_accepts_palette_colors_and_rejects_unknown_values() -> Result<()> {
        let store = Store::open(":memory:")?;
        store.set_note_color("teal")?;
        assert_eq!(store.note_color()?, "teal");
        assert!(store.set_note_color("not-a-color").is_err());
        Ok(())
    }

    #[test]
    fn blank_note_title_remains_blank_when_saved() -> Result<()> {
        let store = Store::open(":memory:")?;
        let id = store.create_note(None, None)?;
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
        let folder = store.create_folder(None, "Werk")?;
        let unfiled = store.create_note(None, None)?;
        let filed = store.create_note(None, Some(&folder))?;
        let archived = store.create_note(None, None)?;
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
        let folder_id = store.create_folder(None, "Werk")?;
        let note_id = store.create_note(None, Some(&folder_id))?;
        let archived_note_id = store.create_note(None, Some(&folder_id))?;
        store.save_note(&note_id, "Belangrijk", "Plan")?;
        store.set_archived(&archived_note_id, true)?;

        store.delete_folder(&folder_id)?;

        assert!(store.folders()?.is_empty());
        let note = store.note(&note_id)?.unwrap();
        assert_eq!(note.folder_id, None);
        assert_eq!(note.title, "Plan");
        assert_eq!(note.content, "Belangrijk");
        assert_eq!(store.note(&archived_note_id)?.unwrap().folder_id, None);
        let replacement_folder_id = store.create_folder(None, "Werk")?;
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
        let folder = store.create_folder(None, "Werk")?;
        let id = store.create_note(None, None)?;
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
        let id = store.create_note(None, None)?;
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
        let folder_id = store.create_folder(None, "Werk")?;
        let id = store.create_note(None, Some(&folder_id))?;
        store.save_note(&id, "Details", "Titel")?;
        store.set_pinned(&id, true)?;
        store.set_archived(&id, true)?;
        store.set_note_color("rose")?;
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
        assert_eq!(store.note_color()?, "rose");
        Ok(())
    }

    #[test]
    fn opening_version_one_database_adds_default_note_color() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
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
            INSERT INTO notes (id, updated_at) VALUES ('existing-note', 1);
            PRAGMA user_version = 1;",
        )?;
        drop(connection);

        let store = Store::open(&path)?;
        assert_eq!(store.note_color()?, "default");
        store.set_note_color("yellow")?;
        assert_eq!(store.note_color()?, "yellow");
        Ok(())
    }

    #[test]
    fn opening_version_two_database_renames_white_note_color_to_yellow() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
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
                archived INTEGER NOT NULL DEFAULT 0,
                color TEXT NOT NULL DEFAULT 'default'
            );
            INSERT INTO notes (id, updated_at, color) VALUES ('existing-note', 1, 'white');
            PRAGMA user_version = 2;",
        )?;
        drop(connection);

        let store = Store::open(&path)?;
        assert_eq!(store.note_color()?, "yellow");
        Ok(())
    }

    #[test]
    fn opening_version_three_database_migrates_latest_note_color_to_global_setting() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("notes.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
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
                archived INTEGER NOT NULL DEFAULT 0,
                color TEXT NOT NULL DEFAULT 'default'
            );
            INSERT INTO notes (id, updated_at, color) VALUES
                ('older-note', 1, 'yellow'),
                ('newer-note', 2, 'mint');
            PRAGMA user_version = 3;",
        )?;
        drop(connection);

        let store = Store::open(&path)?;
        assert_eq!(store.note_color()?, "mint");
        store.set_note_color("rose")?;
        assert_eq!(store.note_color()?, "rose");
        Ok(())
    }

    #[test]
    fn invalid_mutations_do_not_destroy_content() -> Result<()> {
        let store = Store::open(":memory:")?;
        assert!(store.create_folder(None, "  ").is_err());
        assert!(store.create_note(None, Some("missing")).is_err());
        let id = store.create_note(None, None)?;
        store.save_note(&id, "Bewaren", "Titel")?;
        assert!(store.move_note(&id, Some("missing")).is_err());
        assert_eq!(store.note(&id)?.unwrap().content, "Bewaren");
        assert!(store.save_note("missing", "", "").is_err());
        assert_eq!(store.pending_sync_events(10)?.len(), 2);
        Ok(())
    }
}
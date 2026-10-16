//! IMAP backend: notes are mail messages in the `Notes` mailbox, in the layout Apple Notes uses.
//!
//! Each message is HTML with an embedded copy of the Markdown, so our own notes round-trip
//! exactly. Messages written by other clients are converted from their HTML.

use crate::adapters::BackendAdapter;
use crate::i18n::tr;
use crate::memo::{Memo, MemoFolder};
use crate::{NoteSnapshot, SyncEvent};
use anyhow::{Context, Result, bail, ensure};
use mail_parser::{DateTime, MessageParser, MimeHeaders};
use pulldown_cmark::{Event, Options, Parser, Tag, html};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const ROOT: &str = "Notes";
const NOTE_TYPE: &str = "com.apple.mail-note";
const MARKDOWN_MARKER: &str = "memonotes:markdown-v1:";
const IMAGE_PREFIX: &str = "memonotes-image://";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImapAccount {
    pub username: String,
    pub host: String,
    pub port: u16,
    pub starttls: bool,
}

impl ImapAccount {
    /// Parses `[imaps://|imap://]username@host[:port]`; `imap://` upgrades with STARTTLS.
    pub fn parse(spec: &str) -> Result<Self> {
        let invalid = || tr("api.imap.error.invalid_server");
        let spec = spec.trim();
        let (starttls, rest) = match spec.split_once("://") {
            Some(("imaps", rest)) => (false, rest),
            Some(("imap", rest)) => (true, rest),
            Some(_) => bail!(invalid()),
            None => (false, spec),
        };
        let (username, address) = rest.rsplit_once('@').context(invalid())?;
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok().context(invalid())?),
            None => (address, if starttls { 143 } else { 993 }),
        };
        ensure!(
            !username.is_empty()
                && !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
            invalid()
        );
        Ok(Self {
            username: username.to_owned(),
            host: host.to_owned(),
            port,
            starttls,
        })
    }

    /// The stored form of the account; `parse` reads it back.
    pub fn spec(&self) -> String {
        let scheme = if self.starttls { "imap" } else { "imaps" };
        format!("{scheme}://{}@{}:{}", self.username, self.host, self.port)
    }

    fn from_address(&self) -> String {
        if self.username.contains('@') {
            self.username.clone()
        } else {
            format!("{}@localhost", self.username)
        }
    }
}

pub struct StoredMessage {
    pub uid: u32,
    pub validity: u32,
    pub raw: Vec<u8>,
}

/// Mailbox operations the adapter needs. Names are UTF-8; wire encoding is the store's concern.
pub trait MailStore: Send {
    fn delimiter(&mut self) -> Result<String>;
    /// The mailbox called `prefix` and everything below it.
    fn mailboxes(&mut self, prefix: &str) -> Result<Vec<String>>;
    fn create_mailbox(&mut self, name: &str) -> Result<()>;
    /// Makes clients that only list subscribed mailboxes show `name`.
    fn subscribe_mailbox(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }
    fn delete_mailbox(&mut self, name: &str) -> Result<()>;
    fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()>;
    /// UIDVALIDITY and all UIDs of `mailbox`; `None` when it does not exist.
    fn uids(&mut self, mailbox: &str) -> Result<Option<(u32, Vec<u32>)>>;
    fn fetch(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<StoredMessage>>;
    fn append(&mut self, mailbox: &str, message: &[u8]) -> Result<()>;
    fn remove(&mut self, mailbox: &str, uids: &[u32]) -> Result<()>;
}

struct CachedNote {
    id: String,
    updated_ms: i64,
    memo: Memo,
}

// Messages never change, so a UID that was parsed once stays valid while UIDVALIDITY holds.
#[derive(Default)]
struct MailboxCache {
    validity: u32,
    notes: HashMap<u32, Option<CachedNote>>,
}

type NoteCache = HashMap<String, MailboxCache>;

fn shared_cache(key: &str) -> Arc<Mutex<NoteCache>> {
    static CACHES: OnceLock<Mutex<HashMap<String, Arc<Mutex<NoteCache>>>>> = OnceLock::new();
    let mut caches = CACHES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    Arc::clone(caches.entry(key.to_owned()).or_default())
}

/// Brings the cache of `mailbox` up to date, fetching only messages it has not seen.
fn sync_mailbox<'a>(
    store: &mut dyn MailStore,
    cache: &'a mut NoteCache,
    mailbox: &str,
    directory: Option<&Path>,
) -> Result<Option<&'a MailboxCache>> {
    let Some((validity, uids)) = store.uids(mailbox)? else {
        cache.remove(mailbox);
        return Ok(None);
    };
    let entry = cache.entry(mailbox.to_owned()).or_default();
    if entry.validity != validity {
        entry.notes.clear();
        entry.validity = validity;
    }
    let present: HashSet<u32> = uids.iter().copied().collect();
    entry.notes.retain(|uid, _| present.contains(uid));
    let missing: Vec<u32> = uids
        .into_iter()
        .filter(|uid| !entry.notes.contains_key(uid))
        .collect();
    if !missing.is_empty() {
        let folder = (mailbox != ROOT).then(|| mailbox.to_owned());
        for message in store.fetch(mailbox, &missing)? {
            let cached = parse_note(&message.raw, message.validity, message.uid).map(|note| {
                if let Some(directory) = directory {
                    store_images(directory, &note.images);
                }
                CachedNote {
                    id: note.id.clone(),
                    updated_ms: note.updated_ms,
                    memo: memo_from_note(note, folder.clone()),
                }
            });
            entry.notes.insert(message.uid, cached);
        }
    }
    Ok(Some(entry))
}

type Connector = Box<dyn Fn() -> Result<Box<dyn MailStore>> + Send>;

pub struct ImapAdapter {
    account: ImapAccount,
    connector: Connector,
    session: Mutex<Option<Box<dyn MailStore>>>,
    pool_key: Option<String>,
    cache: Arc<Mutex<NoteCache>>,
    image_directory: Option<PathBuf>,
}

// Adapters live for one request; an idle connection is kept so the next one skips the TLS login.
fn session_pool() -> std::sync::MutexGuard<'static, HashMap<String, Box<dyn MailStore>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Box<dyn MailStore>>>> = OnceLock::new();
    POOL.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

impl Drop for ImapAdapter {
    fn drop(&mut self) {
        let Some(key) = self.pool_key.take() else {
            return;
        };
        let session = self.session.get_mut().unwrap_or_else(|error| error.into_inner());
        if let Some(store) = session.take() {
            session_pool().insert(key, store);
        }
    }
}

impl ImapAdapter {
    pub fn new(server: &str, password: &str) -> Result<Self> {
        let account = ImapAccount::parse(server)?;
        let (connect_account, connect_password) = (account.clone(), password.to_owned());
        let mut adapter = Self::with_connector(
            account,
            Box::new(move || connect(&connect_account, &connect_password)),
            None,
        );
        // Adapters are created per request, so the cache outlives them.
        adapter.cache = shared_cache(&adapter.account.spec());
        let mut hasher = DefaultHasher::new();
        password.hash(&mut hasher);
        let key = format!("{}#{:x}", adapter.account.spec(), hasher.finish());
        adapter.session = Mutex::new(session_pool().remove(&key));
        adapter.pool_key = Some(key);
        Ok(adapter)
    }

    fn with_connector(
        account: ImapAccount,
        connector: Connector,
        image_directory: Option<PathBuf>,
    ) -> Self {
        Self {
            account,
            connector,
            session: Mutex::new(None),
            pool_key: None,
            cache: Arc::default(),
            image_directory,
        }
    }

    // A failed command may have left the connection unusable, so the next call reconnects.
    // A connection that sat idle may simply have timed out, so that case is retried once.
    fn with_store<T>(
        &self,
        mut action: impl FnMut(&mut dyn MailStore) -> Result<T>,
    ) -> Result<T> {
        let mut session = self.session.lock().unwrap_or_else(|error| error.into_inner());
        let mut reused = session.is_some();
        loop {
            if session.is_none() {
                *session = Some((self.connector)()?);
            }
            let result = action(session.as_mut().expect("session was just opened").as_mut());
            let Err(error) = &result else {
                return result;
            };
            let retry = reused && is_connection_error(error);
            *session = None;
            if !retry {
                return result;
            }
            reused = false;
        }
    }

    fn image_directory(&self) -> Result<PathBuf> {
        match &self.image_directory {
            Some(directory) => Ok(directory.clone()),
            None => crate::images::image_directory(),
        }
    }

    fn read_notes(&self, mailboxes: Option<&[String]>) -> Result<Vec<Memo>> {
        let directory = self.image_directory().ok();
        self.with_store(|store| {
            let names = match mailboxes {
                Some(names) => names.to_vec(),
                None => note_mailboxes(store)?,
            };
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            if mailboxes.is_none() {
                cache.retain(|name, _| names.contains(name));
            }
            let mut newest: HashMap<String, (i64, u32, Memo)> = HashMap::new();
            for mailbox in names {
                let Some(entry) = sync_mailbox(store, &mut cache, &mailbox, directory.as_deref())?
                else {
                    continue;
                };
                for (uid, cached) in &entry.notes {
                    let Some(cached) = cached else {
                        continue;
                    };
                    let key = (cached.updated_ms, *uid);
                    if newest
                        .get(&cached.id)
                        .is_some_and(|(ms, other, _)| (*ms, *other) >= key)
                    {
                        continue;
                    }
                    newest.insert(cached.id.clone(), (key.0, key.1, cached.memo.clone()));
                }
            }
            let mut memos: Vec<Memo> = newest.into_values().map(|(_, _, memo)| memo).collect();
            memos.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(memos)
        })
    }

    fn upsert(&self, note: &NoteSnapshot) -> Result<()> {
        let directory = self.image_directory().ok();
        let from = self.account.from_address();
        self.with_store(|store| {
            let target = match &note.folder_id {
                Some(folder) => {
                    ensure!(
                        folder.starts_with(&child_prefix(store)?),
                        tr("api.imap.error.invalid_folder")
                    );
                    folder.clone()
                }
                None => ROOT.to_owned(),
            };
            ensure_mailbox(store, &target)?;
            let message = build_message(note, &from, directory.as_deref().unwrap_or(Path::new("")))?;
            let existing = self.locate(store, &note.id)?;
            store.append(&target, &message).map_err(|error| {
                if has_no_response(&error) {
                    anyhow::Error::new(ImapRejected(format!("{error:#}")))
                } else {
                    error
                }
            })?;
            remove_located(store, existing)
        })
    }

    /// Every copy of a note across the note mailboxes, as (mailbox, uid, cached note).
    fn locate_cached<T>(
        &self,
        store: &mut dyn MailStore,
        id: &str,
        pick: impl Fn(&str, u32, &CachedNote) -> T,
    ) -> Result<Vec<T>> {
        ensure!(valid_note_id(id), tr("api.imap.error.invalid_note_id"));
        let directory = self.image_directory().ok();
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let mut found = Vec::new();
        for mailbox in note_mailboxes(store)? {
            let Some(entry) = sync_mailbox(store, &mut cache, &mailbox, directory.as_deref())?
            else {
                continue;
            };
            for (uid, cached) in &entry.notes {
                if let Some(cached) = cached.as_ref().filter(|cached| cached.id == id) {
                    found.push(pick(&mailbox, *uid, cached));
                }
            }
        }
        Ok(found)
    }

    fn locate(&self, store: &mut dyn MailStore, id: &str) -> Result<Vec<(String, u32)>> {
        self.locate_cached(store, id, |mailbox, uid, _| (mailbox.to_owned(), uid))
    }
}

impl BackendAdapter for ImapAdapter {
    fn check_connection(&self) -> Result<()> {
        self.with_store(|store| store.delimiter().map(|_| ()))
    }

    fn list_memos(&self) -> Result<Vec<Memo>> {
        self.read_notes(None)
    }

    fn list_folders(&self) -> Result<Vec<MemoFolder>> {
        self.with_store(|store| {
            let prefix = child_prefix(store)?;
            Ok(store
                .mailboxes(ROOT)?
                .into_iter()
                .filter_map(|name| {
                    let display = name.strip_prefix(&prefix)?.to_owned();
                    Some(MemoFolder {
                        id: name,
                        name: display,
                        is_shared: false,
                    })
                })
                .collect())
        })
    }

    fn get_memo(&self, name: &str) -> Result<Memo> {
        self.get_memo_if_exists(name)?
            .context(tr("api.imap.error.note_not_found"))
    }

    fn get_memo_if_exists(&self, name: &str) -> Result<Option<Memo>> {
        ensure!(valid_note_id(name), tr("api.imap.error.invalid_note_id"));
        let found = self.with_store(|store| {
            self.locate_cached(store, name, |_, uid, cached| {
                (cached.updated_ms, uid, cached.memo.clone())
            })
        })?;
        Ok(found
            .into_iter()
            .max_by_key(|(updated_ms, uid, _)| (*updated_ms, *uid))
            .map(|(_, _, memo)| memo))
    }

    fn get_folder(&self, name: &str) -> Result<MemoFolder> {
        self.list_folders()?
            .into_iter()
            .find(|folder| folder.id == name)
            .context(tr("api.imap.error.folder_not_found"))
    }

    fn list_folder_memos(&self, name: &str) -> Result<Vec<Memo>> {
        self.read_notes(Some(&[name.to_owned()]))
    }

    fn apply_event(&self, event: &SyncEvent) -> Result<()> {
        match event {
            SyncEvent::FolderCreated { folder } | SyncEvent::FolderUpdated { folder } => {
                let created = matches!(event, SyncEvent::FolderCreated { .. });
                self.with_store(|store| {
                    let delimiter = store.delimiter()?;
                    let wanted = folder_mailbox(&delimiter, &folder.name);
                    ensure_mailbox(store, ROOT)?;
                    let current = (!created && folder.id != wanted
                        && folder.id.starts_with(&format!("{ROOT}{delimiter}")))
                    .then(|| folder.id.clone());
                    match current {
                        Some(from) if store.mailboxes(&from)?.contains(&from) => {
                            store.rename_mailbox(&from, &wanted)?;
                            store.subscribe_mailbox(&wanted)
                        }
                        _ => ensure_mailbox(store, &wanted),
                    }
                })
            }
            SyncEvent::FolderDeleted { folder_id } => self.with_store(|store| {
                if folder_id.starts_with(&child_prefix(store)?)
                    && store.mailboxes(folder_id)?.contains(folder_id)
                {
                    store.delete_mailbox(folder_id)?;
                }
                Ok(())
            }),
            SyncEvent::NoteCreated { note }
            | SyncEvent::NoteRestored { note }
            | SyncEvent::NoteUpdated { note }
            | SyncEvent::NoteArchived { note }
            | SyncEvent::NoteUnarchived { note } => self.upsert(note),
            SyncEvent::NoteDeleted { note } => {
                ensure!(valid_note_id(&note.id), tr("api.imap.error.invalid_note_id"));
                self.with_store(|store| {
                    let existing = self.locate(store, &note.id)?;
                    remove_located(store, existing)
                })
            }
        }
    }

    fn applied_remote_name(&self, event: &SyncEvent) -> Option<String> {
        let (SyncEvent::FolderCreated { folder } | SyncEvent::FolderUpdated { folder }) = event
        else {
            return None;
        };
        let delimiter = self.with_store(|store| store.delimiter()).ok()?;
        Some(folder_mailbox(&delimiter, &folder.name))
    }
}

pub fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<::imap::Error>(),
            Some(::imap::Error::Io(_) | ::imap::Error::ConnectionLost | ::imap::Error::Bye(_))
        ) || cause.downcast_ref::<std::io::Error>().is_some()
    })
}

/// The server refused one specific message, so retrying it unchanged cannot succeed.
#[derive(Debug)]
struct ImapRejected(String);

impl std::fmt::Display for ImapRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ImapRejected {}

pub fn is_rejected_request(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ImapRejected>().is_some())
}

fn has_no_response(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| matches!(cause.downcast_ref::<::imap::Error>(), Some(::imap::Error::No(_))))
}

fn child_prefix(store: &mut dyn MailStore) -> Result<String> {
    Ok(format!("{ROOT}{}", store.delimiter()?))
}

fn note_mailboxes(store: &mut dyn MailStore) -> Result<Vec<String>> {
    let prefix = child_prefix(store)?;
    Ok(store
        .mailboxes(ROOT)?
        .into_iter()
        .filter(|name| name == ROOT || name.starts_with(&prefix))
        .collect())
}

fn ensure_mailbox(store: &mut dyn MailStore, name: &str) -> Result<()> {
    if store.mailboxes(name)?.iter().any(|existing| existing == name) {
        return store.subscribe_mailbox(name);
    }
    if name != ROOT {
        ensure_mailbox(store, ROOT)?;
    }
    store.create_mailbox(name)?;
    store.subscribe_mailbox(name)
}

fn folder_mailbox(delimiter: &str, name: &str) -> String {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | '*' | '%') || delimiter.contains(c) {
                '-'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    let clean = if clean.trim().is_empty() {
        "Folder".to_owned()
    } else {
        clean
    };
    format!("{ROOT}{delimiter}{clean}")
}

fn valid_note_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn remove_located(store: &mut dyn MailStore, located: Vec<(String, u32)>) -> Result<()> {
    let mut by_mailbox: HashMap<String, Vec<u32>> = HashMap::new();
    for (mailbox, uid) in located {
        by_mailbox.entry(mailbox).or_default().push(uid);
    }
    for (mailbox, uids) in by_mailbox {
        store.remove(&mailbox, &uids)?;
    }
    Ok(())
}

fn memo_from_note(note: ParsedNote, folder: Option<String>) -> Memo {
    Memo {
        title: Some(crate::memo::editor_parts(&note.content).0.to_owned()),
        id: note.id,
        content: note.content,
        folder_id: folder,
        pinned: note.pinned,
        archived: note.archived,
        is_shared: false,
        updated_at: (note.updated_ms > 0).then_some(note.updated_ms),
    }
}

fn store_images(directory: &Path, images: &[(String, Vec<u8>)]) {
    for (name, bytes) in images {
        let path = directory.join(name);
        if path.exists() || std::fs::create_dir_all(directory).is_err() {
            continue;
        }
        let partial = path.with_extension("part");
        if std::fs::write(&partial, bytes).is_ok() {
            let _ = std::fs::rename(partial, path);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Messages

pub struct ParsedNote {
    pub id: String,
    pub content: String,
    pub pinned: bool,
    pub archived: bool,
    pub updated_ms: i64,
    pub images: Vec<(String, Vec<u8>)>,
}

// Mail from other clients has no ID header: its position identifies it, and the subject/date hash
// guards against a mailbox that reuses a UIDVALIDITY.
fn note_id(message: &mail_parser::Message<'_>, validity: u32, uid: u32) -> String {
    if let Some(id) = raw_header(message, "x-universally-unique-identifier") {
        return id.to_owned();
    }
    let mut hash: u32 = 0x811c_9dc5;
    let subject = raw_header(message, "subject").unwrap_or_default();
    let date = raw_header(message, "date").unwrap_or_default();
    for byte in subject.bytes().chain(std::iter::once(0)).chain(date.bytes()) {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    format!("m{validity:x}-{uid}-{hash:08x}")
}

fn raw_header<'a>(message: &'a mail_parser::Message<'a>, name: &str) -> Option<&'a str> {
    message
        .headers_raw()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
}

pub fn parse_note(raw: &[u8], validity: u32, uid: u32) -> Option<ParsedNote> {
    let message = MessageParser::default().parse(raw)?;
    let id = note_id(&message, validity, uid);
    let subject = message.subject().unwrap_or_default().trim().to_owned();
    let html = message
        .body_html(0)
        .map(|html| html.into_owned())
        .unwrap_or_default();
    let embedded = embedded_markdown(&html);

    let mut images = Vec::new();
    let mut names = HashMap::new();
    for part in message.attachments() {
        let is_image = part
            .content_type()
            .is_some_and(|ctype| ctype.ctype().eq_ignore_ascii_case("image"));
        let key = part
            .content_id()
            .map(|id| id.trim_matches(['<', '>']).to_owned())
            .or_else(|| part.attachment_name().map(str::to_owned));
        let (true, Some(key)) = (is_image, key) else {
            continue;
        };
        let extension = part
            .attachment_name()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension.to_owned())
            .or_else(|| part.content_type()?.subtype().map(str::to_owned));
        let name = if embedded.is_some() {
            safe_file_name(&key)
        } else {
            foreign_image_name(&key, extension.as_deref())
        };
        names.insert(key.to_ascii_lowercase(), name.clone());
        images.push((name, part.contents().to_vec()));
    }

    let content = match embedded {
        Some(markdown) => markdown,
        None => {
            let body = if html.is_empty() {
                message.body_text(0).map(|text| text.into_owned()).unwrap_or_default()
            } else {
                html_to_markdown(&html, &names)
            };
            markdown_with_title(&subject, &body)
        }
    };
    let flag = |name: &str| raw_header(&message, name).is_some_and(|value| value.eq_ignore_ascii_case("yes"));
    Some(ParsedNote {
        id,
        content,
        pinned: flag("x-memonotes-pinned"),
        archived: flag("x-memonotes-archived"),
        updated_ms: message.date().map_or(0, |date| date.to_timestamp() * 1000),
        images,
    })
}

fn embedded_markdown(html: &str) -> Option<String> {
    let start = html.find(MARKDOWN_MARKER)? + MARKDOWN_MARKER.len();
    let end = start + html[start..].find("-->")?;
    String::from_utf8(base64_decode(&html[start..end])?).ok()
}

// Other clients repeat the title as the first line of the body; the editor keeps it as a heading.
fn markdown_with_title(title: &str, body: &str) -> String {
    let bare = |line: &str| line.trim_matches(|c: char| c == '#' || c == '*' || c.is_whitespace()).to_owned();
    let mut lines = body.trim().lines();
    let first = lines.clone().next().unwrap_or_default();
    let rest = if !title.is_empty() && bare(first) == bare(title) {
        lines.next();
        lines.collect::<Vec<_>>().join("\n")
    } else {
        body.trim().to_owned()
    };
    let rest = rest.trim();
    match (title.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (false, true) => format!("# {title}"),
        (false, false) => format!("# {title}\n\n{rest}"),
    }
}

fn safe_file_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(80)
        .collect();
    let clean = clean.trim_start_matches('.');
    if clean.is_empty() { "image".to_owned() } else { clean.to_owned() }
}

fn foreign_image_name(key: &str, extension: Option<&str>) -> String {
    let name = safe_file_name(key);
    let name = if name.contains('.') {
        name
    } else {
        let extension = match extension.unwrap_or("png").to_ascii_lowercase().as_str() {
            "jpeg" => "jpg".to_owned(),
            other => safe_file_name(other),
        };
        format!("{name}.{extension}")
    };
    format!("imap_{name}")
}

fn local_image_file(url: &str) -> Option<String> {
    let name = url.strip_prefix(IMAGE_PREFIX)?.split('#').next()?;
    (!name.is_empty() && safe_file_name(name) == name).then(|| name.to_owned())
}

fn render_html(markdown: &str, directory: &Path) -> (String, Vec<(String, Vec<u8>)>) {
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            let url = match local_image_file(&dest_url) {
                Some(name) => {
                    if !images.iter().any(|(known, _)| *known == name)
                        && let Ok(bytes) = std::fs::read(directory.join(&name))
                    {
                        images.push((name.clone(), bytes));
                    }
                    format!("cid:{name}")
                }
                None => dest_url.to_string(),
            };
            Event::Start(Tag::Image { link_type, dest_url: url.into(), title, id })
        }
        other => other,
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    (output, images)
}

pub fn build_message(note: &NoteSnapshot, from: &str, image_directory: &Path) -> Result<Vec<u8>> {
    ensure!(valid_note_id(&note.id), tr("api.imap.error.invalid_note_id"));
    let heading = crate::memo::editor_parts(&note.content).0.trim();
    let subject = if heading.is_empty() { note.title.trim() } else { heading };
    let (body, images) = render_html(&note.content, image_directory);
    let document = format!(
        "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\"></head><body>{body}<!--{MARKDOWN_MARKER}{}--></body></html>",
        base64_encode(note.content.as_bytes())
    );
    let date = DateTime::from_timestamp(note.updated_at / 1000).to_rfc822();
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut message = format!(
        "From: {from}\r\nSubject: {}\r\nDate: {date}\r\nMessage-Id: <{}@memonotes.invalid>\r\nMime-Version: 1.0\r\nX-Uniform-Type-Identifier: {NOTE_TYPE}\r\nX-Universally-Unique-Identifier: {}\r\nX-Mail-Created-Date: {date}\r\nX-Memonotes-Pinned: {}\r\nX-Memonotes-Archived: {}\r\n",
        encode_header(subject),
        uuid::Uuid::new_v4(),
        note.id,
        yes_no(note.pinned),
        yes_no(note.archived),
    );
    let html_part = format!(
        "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
        wrap_base64(document.as_bytes())
    );
    if images.is_empty() {
        message.push_str(&html_part);
        return Ok(message.into_bytes());
    }
    let boundary = format!("=-memonotes-{}", uuid::Uuid::new_v4().simple());
    message.push_str(&format!(
        "Content-Type: multipart/related; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n{html_part}\r\n"
    ));
    for (name, bytes) in &images {
        message.push_str(&format!(
            "--{boundary}\r\nContent-Type: {}; name=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <{name}>\r\nContent-Disposition: inline; filename=\"{name}\"\r\n\r\n{}\r\n",
            crate::images::mime_type(name),
            wrap_base64(bytes)
        ));
    }
    message.push_str(&format!("--{boundary}--\r\n"));
    Ok(message.into_bytes())
}

fn encode_header(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.is_ascii() {
        return clean;
    }
    let mut words = Vec::new();
    let mut current = String::new();
    for character in clean.chars() {
        if current.len() + character.len_utf8() > 30 {
            words.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    words.push(current);
    words
        .iter()
        .map(|word| format!("=?UTF-8?B?{}?=", base64_encode(word.as_bytes())))
        .collect::<Vec<_>>()
        .join("\r\n ")
}

// ---------------------------------------------------------------------------------------------
// Base64 and IMAP mailbox names

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0u32, |value, (index, byte)| value | u32::from(*byte) << (16 - 8 * index));
        for index in 0..4usize {
            if index <= chunk.len() {
                output.push(char::from(BASE64[(value >> (18 - 6 * index as u32) & 63) as usize]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

fn wrap_base64(bytes: &[u8]) -> String {
    let encoded = base64_encode(bytes);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|line| std::str::from_utf8(line).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(text.len() / 4 * 3);
    let (mut buffer, mut bits) = (0u32, 0);
    for character in text.bytes() {
        if character == b'=' || character.is_ascii_whitespace() {
            continue;
        }
        let value = BASE64.iter().position(|candidate| *candidate == character)? as u32;
        buffer = buffer << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits & 0xff) as u8);
        }
    }
    Some(output)
}

fn utf7_encode(name: &str) -> String {
    fn flush(output: &mut String, pending: &mut Vec<u16>) {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
        output.push('&');
        output.push_str(&base64_encode(&bytes).trim_end_matches('=').replace('/', ","));
        output.push('-');
        pending.clear();
    }
    let (mut output, mut pending) = (String::new(), Vec::new());
    for character in name.chars() {
        if ('\u{20}'..='\u{7e}').contains(&character) {
            flush(&mut output, &mut pending);
            if character == '&' {
                output.push_str("&-");
            } else {
                output.push(character);
            }
        } else {
            let mut units = [0u16; 2];
            pending.extend_from_slice(character.encode_utf16(&mut units));
        }
    }
    flush(&mut output, &mut pending);
    output
}

fn utf7_decode(name: &str) -> String {
    let (mut output, mut rest) = (String::new(), name);
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            output.push('&');
        } else if let Some(bytes) = base64_decode(&chunk.replace(',', "/")) {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            output.push_str(&String::from_utf16_lossy(&units));
        } else {
            output.push_str(&rest[start..start + end + 2]);
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
}

// ---------------------------------------------------------------------------------------------
// HTML from other clients

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let decoded = after.find(';').filter(|end| *end <= 8).and_then(|end| {
            let entity = &after[..end];
            let character = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => entity
                    .strip_prefix('#')
                    .and_then(|number| match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            character.map(|character| (character, end))
        });
        match decoded {
            Some((character, end)) => {
                output.push(character);
                rest = &after[end + 1..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

fn parse_attributes(text: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        let mut value = String::new();
        if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            if let Some(quote) = after.chars().next().filter(|c| matches!(c, '"' | '\'')) {
                let inner = &after[1..];
                let end = inner.find(quote).unwrap_or(inner.len());
                value = decode_entities(&inner[..end]);
                rest = inner[(end + 1).min(inner.len())..].trim_start();
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                value = decode_entities(&after[..end]);
                rest = after[end..].trim_start();
            }
        }
        if !name.is_empty() {
            attributes.push((name, value));
        }
        rest = rest.trim_start();
    }
    attributes
}

struct HtmlConverter<'a> {
    output: String,
    lists: Vec<(bool, u32)>,
    links: Vec<Option<String>>,
    preformatted: bool,
    images: &'a HashMap<String, String>,
}

impl HtmlConverter<'_> {
    fn breaks(&mut self, wanted: usize) {
        if self.output.is_empty() {
            return;
        }
        let existing = self.output.chars().rev().take_while(|c| *c == '\n').count();
        for _ in existing..wanted {
            self.output.push('\n');
        }
    }

    fn text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if self.preformatted {
            self.output.push_str(&decoded);
            return;
        }
        let mut collapsed = String::new();
        let mut last_space = false;
        for character in decoded.chars() {
            if character.is_whitespace() {
                if !last_space {
                    collapsed.push(' ');
                }
                last_space = true;
            } else {
                collapsed.push(character);
                last_space = false;
            }
        }
        if self.output.is_empty() || self.output.ends_with('\n') || self.output.ends_with(' ') {
            collapsed = collapsed.trim_start().to_owned();
        }
        self.output.push_str(&collapsed);
    }

    fn open(&mut self, name: &str, attributes: &[(String, String)]) {
        let attribute = |wanted: &str| {
            attributes
                .iter()
                .find(|(key, _)| key == wanted)
                .map(|(_, value)| value.clone())
        };
        match name {
            "br" => self.output.push('\n'),
            "p" | "blockquote" => {
                self.breaks(2);
                if name == "blockquote" {
                    self.output.push_str("> ");
                }
            }
            "div" | "tr" | "table" | "section" | "article" => self.breaks(1),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.breaks(2);
                self.output.push_str(&"#".repeat(usize::from(name.as_bytes()[1] - b'0')));
                self.output.push(' ');
            }
            "ul" | "ol" => {
                self.breaks(if self.lists.is_empty() { 2 } else { 1 });
                self.lists.push((name == "ol", 0));
            }
            "li" => {
                self.breaks(1);
                let indentation = "    ".repeat(self.lists.len().saturating_sub(1));
                let marker = match self.lists.last_mut() {
                    Some((true, number)) => {
                        *number += 1;
                        format!("{number}. ")
                    }
                    _ => "- ".to_owned(),
                };
                self.output.push_str(&indentation);
                self.output.push_str(&marker);
            }
            "b" | "strong" => self.output.push_str("**"),
            "i" | "em" => self.output.push('*'),
            "u" => self.output.push_str("<u>"),
            "s" | "strike" | "del" => self.output.push_str("~~"),
            "code" | "tt" if !self.preformatted => self.output.push('`'),
            "pre" => {
                self.breaks(2);
                self.output.push_str("```\n");
                self.preformatted = true;
            }
            "a" => {
                let href = attribute("href").filter(|href| !href.is_empty());
                if href.is_some() {
                    self.output.push('[');
                }
                self.links.push(href);
            }
            "hr" => {
                self.breaks(2);
                self.output.push_str("---\n\n");
            }
            "td" | "th" => {
                if !self.output.is_empty() && !self.output.ends_with('\n') {
                    self.output.push_str(" | ");
                }
            }
            "img" => {
                let source = attribute("src").unwrap_or_default();
                let target = match source.strip_prefix("cid:") {
                    Some(key) => self
                        .images
                        .get(&key.trim_matches(['<', '>']).to_ascii_lowercase())
                        .map(|name| format!("{IMAGE_PREFIX}{name}")),
                    None => (source.starts_with("http://") || source.starts_with("https://"))
                        .then_some(source),
                };
                if let Some(target) = target {
                    let alt = attribute("alt").unwrap_or_default();
                    self.output.push_str(&format!("![{alt}]({target})"));
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "p" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.breaks(2),
            "div" | "tr" | "li" | "table" | "section" | "article" => self.breaks(1),
            "ul" | "ol" => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.breaks(2);
                }
            }
            "b" | "strong" => self.output.push_str("**"),
            "i" | "em" => self.output.push('*'),
            "u" => self.output.push_str("</u>"),
            "s" | "strike" | "del" => self.output.push_str("~~"),
            "code" | "tt" if !self.preformatted => self.output.push('`'),
            "pre" => {
                if !self.output.ends_with('\n') {
                    self.output.push('\n');
                }
                self.output.push_str("```");
                self.preformatted = false;
                self.breaks(2);
            }
            "a" => {
                if let Some(Some(href)) = self.links.pop() {
                    self.output.push_str(&format!("]({href})"));
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn html_to_markdown(html: &str, images: &HashMap<String, String>) -> String {
    let mut converter = HtmlConverter {
        output: String::new(),
        lists: Vec::new(),
        links: Vec::new(),
        preformatted: false,
        images,
    };
    let mut position = 0;
    while position < html.len() {
        let Some(offset) = html[position..].find('<') else {
            converter.text(&html[position..]);
            break;
        };
        if offset > 0 {
            converter.text(&html[position..position + offset]);
        }
        position += offset;
        let rest = &html[position..];
        if rest.starts_with("<!--") {
            position += rest.find("-->").map_or(rest.len(), |end| end + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            position += rest.find('>').map_or(rest.len(), |end| end + 1);
            continue;
        }
        let mut quote: Option<char> = None;
        let mut end = None;
        for (index, character) in rest.char_indices().skip(1) {
            match (quote, character) {
                (Some(open), c) if c == open => quote = None,
                (None, '"' | '\'') => quote = Some(character),
                (None, '>') => {
                    end = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            converter.text(rest);
            break;
        };
        let tag = &rest[1..end];
        position += end + 1;
        let (closing, body) = match tag.strip_prefix('/') {
            Some(body) => (true, body),
            None => (false, tag),
        };
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        if closing {
            converter.close(&name);
            continue;
        }
        converter.open(&name, &parse_attributes(&body[name_end..]));
        if matches!(name.as_str(), "style" | "script" | "head")
            && !body.trim_end().ends_with('/')
        {
            let closing_tag = format!("</{name}");
            let lower = html[position..].to_ascii_lowercase();
            position += lower.find(&closing_tag).unwrap_or(lower.len());
        }
        if body.trim_end().ends_with('/') && !matches!(name.as_str(), "br" | "hr" | "img") {
            converter.close(&name);
        }
    }
    let mut lines: Vec<&str> = converter.output.lines().map(str::trim_end).collect();
    let mut blank = 0;
    lines.retain(|line| {
        blank = if line.is_empty() { blank + 1 } else { 0 };
        blank <= 1
    });
    lines.join("\n").trim().to_owned()
}

// ---------------------------------------------------------------------------------------------
// The real server

struct RealMail {
    session: ::imap::Session<::imap::Connection>,
    delimiter: Option<String>,
    uid_plus: Option<bool>,
}

fn connect(account: &ImapAccount, password: &str) -> Result<Box<dyn MailStore>> {
    let mode = if account.starttls {
        ::imap::ConnectionMode::StartTls
    } else {
        ::imap::ConnectionMode::Tls
    };
    let client = ::imap::ClientBuilder::new(account.host.as_str(), account.port)
        .mode(mode)
        .tls_kind(::imap::TlsKind::Rust)
        .connect()?;
    let session = client
        .login(&account.username, password)
        .map_err(|(error, _)| error)?;
    Ok(Box::new(RealMail {
        session,
        delimiter: None,
        uid_plus: None,
    }))
}

impl RealMail {
    fn select(&mut self, mailbox: &str) -> Result<Option<u32>> {
        match self.session.select(utf7_encode(mailbox)) {
            Ok(selected) => Ok(Some(selected.uid_validity.unwrap_or(0))),
            Err(::imap::Error::No(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn all_uids(&mut self) -> Result<Vec<u32>> {
        let mut uids: Vec<u32> = self.session.uid_search("ALL")?.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    // The server is asked for the whole body, headers included.
    fn fetch_selected(&mut self, validity: u32, uids: &[u32]) -> Result<Vec<StoredMessage>> {
        let mut messages = Vec::new();
        for chunk in uids.chunks(100) {
            let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let fetches = self.session.uid_fetch(set, "(UID BODY.PEEK[])")?;
            for fetch in fetches.iter() {
                if let (Some(uid), Some(body)) = (fetch.uid, fetch.body()) {
                    messages.push(StoredMessage { uid, validity, raw: body.to_vec() });
                }
            }
        }
        Ok(messages)
    }
}

impl MailStore for RealMail {
    fn delimiter(&mut self) -> Result<String> {
        if let Some(delimiter) = &self.delimiter {
            return Ok(delimiter.clone());
        }
        let names = self.session.list(Some(""), Some("\"\""))?;
        let delimiter = names
            .iter()
            .find_map(|name| name.delimiter().map(str::to_owned))
            .unwrap_or_else(|| "/".to_owned());
        self.delimiter = Some(delimiter.clone());
        Ok(delimiter)
    }

    fn mailboxes(&mut self, prefix: &str) -> Result<Vec<String>> {
        let delimiter = self.delimiter()?;
        // The crate sends the pattern verbatim, so it must be a quoted string.
        let pattern = format!(
            "\"{}*\"",
            utf7_encode(prefix).replace('\\', "\\\\").replace('"', "\\\"")
        );
        let names = self.session.list(Some(""), Some(&pattern))?;
        let below = format!("{prefix}{delimiter}");
        Ok(names
            .iter()
            .map(|name| utf7_decode(name.name()))
            .filter(|name| name == prefix || name.starts_with(&below))
            .collect())
    }

    fn create_mailbox(&mut self, name: &str) -> Result<()> {
        let created: Result<()> = match self.session.create(utf7_encode(name)) {
            Ok(()) => Ok(()),
            Err(::imap::Error::No(error)) => {
                if self.mailboxes(name)?.iter().any(|existing| existing == name) {
                    Ok(())
                } else {
                    Err(::imap::Error::No(error).into())
                }
            }
            Err(error) => Err(error.into()),
        };
        created
    }

    fn subscribe_mailbox(&mut self, name: &str) -> Result<()> {
        // Some servers answer NO when already subscribed; subscribing is only cosmetic.
        match self.session.subscribe(utf7_encode(name)) {
            Ok(()) | Err(::imap::Error::No(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn delete_mailbox(&mut self, name: &str) -> Result<()> {
        match self.session.delete(utf7_encode(name)) {
            Ok(()) | Err(::imap::Error::No(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()> {
        Ok(self.session.rename(utf7_encode(from), utf7_encode(to))?)
    }

    fn uids(&mut self, mailbox: &str) -> Result<Option<(u32, Vec<u32>)>> {
        let Some(validity) = self.select(mailbox)? else {
            return Ok(None);
        };
        Ok(Some((validity, self.all_uids()?)))
    }

    fn fetch(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<StoredMessage>> {
        let Some(validity) = self.select(mailbox)? else {
            return Ok(Vec::new());
        };
        self.fetch_selected(validity, uids)
    }

    fn append(&mut self, mailbox: &str, message: &[u8]) -> Result<()> {
        let encoded = utf7_encode(mailbox);
        let mut command = self.session.append(&encoded, message);
        command.flag(::imap::types::Flag::Seen);
        command.finish()?;
        Ok(())
    }

    fn remove(&mut self, mailbox: &str, uids: &[u32]) -> Result<()> {
        if uids.is_empty() || self.select(mailbox)?.is_none() {
            return Ok(());
        }
        let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        self.session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)")?;
        let uid_plus = match self.uid_plus {
            Some(known) => known,
            None => {
                let known = self.session.capabilities()?.has_str("UIDPLUS");
                self.uid_plus = Some(known);
                known
            }
        };
        if uid_plus {
            self.session.uid_expunge(&set)?;
        } else {
            self.session.expunge()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[derive(Default)]
    struct Server {
        mailboxes: BTreeMap<String, Vec<(u32, Vec<u8>)>>,
        next_uid: u32,
    }

    #[derive(Clone, Default)]
    struct FakeMail(Arc<Mutex<Server>>);

    impl MailStore for FakeMail {
        fn delimiter(&mut self) -> Result<String> {
            Ok("/".to_owned())
        }

        fn mailboxes(&mut self, prefix: &str) -> Result<Vec<String>> {
            let server = self.0.lock().unwrap();
            let below = format!("{prefix}/");
            Ok(server
                .mailboxes
                .keys()
                .filter(|name| *name == prefix || name.starts_with(&below))
                .cloned()
                .collect())
        }

        fn create_mailbox(&mut self, name: &str) -> Result<()> {
            self.0.lock().unwrap().mailboxes.entry(name.to_owned()).or_default();
            Ok(())
        }

        fn delete_mailbox(&mut self, name: &str) -> Result<()> {
            self.0.lock().unwrap().mailboxes.remove(name);
            Ok(())
        }

        fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()> {
            let mut server = self.0.lock().unwrap();
            let messages = server.mailboxes.remove(from).context("no such mailbox")?;
            server.mailboxes.insert(to.to_owned(), messages);
            Ok(())
        }

        fn uids(&mut self, mailbox: &str) -> Result<Option<(u32, Vec<u32>)>> {
            let server = self.0.lock().unwrap();
            Ok(server
                .mailboxes
                .get(mailbox)
                .map(|messages| (7, messages.iter().map(|(uid, _)| *uid).collect())))
        }

        fn fetch(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<StoredMessage>> {
            let server = self.0.lock().unwrap();
            let messages = server.mailboxes.get(mailbox).cloned().unwrap_or_default();
            Ok(messages
                .into_iter()
                .filter(|(uid, _)| uids.contains(uid))
                .map(|(uid, raw)| StoredMessage { uid, validity: 7, raw })
                .collect())
        }

        fn append(&mut self, mailbox: &str, message: &[u8]) -> Result<()> {
            let mut server = self.0.lock().unwrap();
            server.next_uid += 1;
            let uid = server.next_uid;
            server
                .mailboxes
                .get_mut(mailbox)
                .context("no such mailbox")?
                .push((uid, message.to_vec()));
            Ok(())
        }

        fn remove(&mut self, mailbox: &str, uids: &[u32]) -> Result<()> {
            if let Some(messages) = self.0.lock().unwrap().mailboxes.get_mut(mailbox) {
                messages.retain(|(uid, _)| !uids.contains(uid));
            }
            Ok(())
        }
    }

    fn adapter(server: &FakeMail, directory: &Path) -> ImapAdapter {
        let server = server.clone();
        ImapAdapter::with_connector(
            ImapAccount::parse("me@example.com@imap.example.com").unwrap(),
            Box::new(move || Ok(Box::new(server.clone()))),
            Some(directory.to_path_buf()),
        )
    }

    fn note(id: &str, content: &str, folder: Option<&str>) -> NoteSnapshot {
        NoteSnapshot {
            id: id.to_owned(),
            account_id: Some("account".to_owned()),
            folder_id: folder.map(str::to_owned),
            content: content.to_owned(),
            title: String::new(),
            updated_at: 1_790_000_000_000,
            pinned: false,
            archived: false,
        }
    }

    #[test]
    fn server_addresses_are_parsed() {
        let account = ImapAccount::parse("me@example.com@imap.gmail.com").unwrap();
        assert_eq!(
            account,
            ImapAccount {
                username: "me@example.com".to_owned(),
                host: "imap.gmail.com".to_owned(),
                port: 993,
                starttls: false,
            }
        );
        let account = ImapAccount::parse("imap://notes@mail.example.org:1143").unwrap();
        assert_eq!((account.port, account.starttls), (1143, true));
        assert_eq!(ImapAccount::parse("imap://notes@mail.example.org").unwrap().port, 143);
        for invalid in ["", "imap.gmail.com", "@host", "me@", "ftp://me@host", "me@host:x", "me@ho st"] {
            assert!(ImapAccount::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn mailbox_names_round_trip_through_modified_utf7() {
        for name in ["Notes", "Notes/Café", "Notes/日本語 & more", "A&B", "😀"] {
            assert_eq!(utf7_decode(&utf7_encode(name)), name);
        }
        assert_eq!(utf7_encode("Café"), "Caf&AOk-");
        assert_eq!(utf7_encode("a&b"), "a&-b");
    }

    #[test]
    fn base64_round_trips_without_padding() {
        for text in ["", "a", "ab", "abc", "abcd", "héllo wörld"] {
            assert_eq!(base64_decode(&base64_encode(text.as_bytes())).unwrap(), text.as_bytes());
            assert_eq!(
                base64_decode(base64_encode(text.as_bytes()).trim_end_matches('=')).unwrap(),
                text.as_bytes()
            );
        }
    }

    #[test]
    fn our_notes_round_trip_exactly_with_images() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("photo.png"), b"png-bytes")?;
        let mut original = note(
            "550e8400-e29b-41d4-a716-446655440000",
            "# Café ☕\n\nSome **bold** text\n\n![foto](memonotes-image://photo.png#w=320)\n\n- [ ] item",
            None,
        );
        original.pinned = true;
        original.archived = true;

        let raw = build_message(&original, "me@example.com", directory.path())?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.contains("X-Uniform-Type-Identifier: com.apple.mail-note\r\n"));
        assert!(text.contains("Subject: =?UTF-8?B?"));
        assert!(text.contains("multipart/related"));
        let parsed = parse_note(&raw, 1, 1).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.content, original.content);
        assert!(parsed.pinned && parsed.archived);
        assert_eq!(parsed.updated_ms, 1_790_000_000_000);
        assert_eq!(parsed.images, [("photo.png".to_owned(), b"png-bytes".to_vec())]);
        Ok(())
    }

    #[test]
    fn notes_from_apple_notes_are_converted_to_markdown() {
        let html = "<html><head><style>p{}</style></head><body>\
            <div><b><span style=\"font-size: 24px\">Shopping &amp; more</span></b></div>\
            <div>Buy <i>fresh</i> milk</div><div><br></div>\
            <ul><li>Eggs</li><li>Bread<ul><li>Rye</li></ul></li></ul>\
            <div>See <a href=\"https://example.com\">the site</a></div>\
            <div><img src=\"cid:ABC-1\" alt=\"pic\"></div></body></html>";
        let raw = format!(
            "From: me@example.com\r\nSubject: Shopping & more\r\nDate: Thu, 1 Oct 2026 12:00:00 +0000\r\n\
             X-Uniform-Type-Identifier: com.apple.mail-note\r\nX-Universally-Unique-Identifier: APPLE-ID-1\r\n\
             MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"b\"\r\n\r\n--b\r\n\
             Content-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n--b\r\n\
             Content-Type: image/png; name=\"pic.png\"\r\nContent-ID: <ABC-1>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}\r\n--b--\r\n",
            base64_encode(b"pixels")
        );

        let parsed = parse_note(raw.as_bytes(), 1, 1).unwrap();

        assert_eq!(parsed.id, "APPLE-ID-1");
        assert_eq!(
            parsed.content,
            "# Shopping & more\n\nBuy *fresh* milk\n\n- Eggs\n- Bread\n    - Rye\n\nSee [the site](https://example.com)\n![pic](memonotes-image://imap_ABC-1.png)"
        );
        assert_eq!(parsed.images, [("imap_ABC-1.png".to_owned(), b"pixels".to_vec())]);
        assert!(!parsed.pinned);
    }

    #[test]
    fn plain_mail_without_note_headers_is_a_note_with_a_stable_id() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let server = FakeMail::default();
        let raw = b"From: a@b.c\r\nSubject: Garage\r\nDate: Tue, 25 Aug 2020 10:58:34 +0200\r\n\r\n# Garage\n\nBanden goed";
        let parsed = parse_note(raw, 7, 59).unwrap();
        assert_eq!(parsed.content, "# Garage\n\nBanden goed");
        assert_eq!(parse_note(raw, 7, 59).unwrap().id, parsed.id);
        assert_ne!(parse_note(raw, 7, 60).unwrap().id, parsed.id);

        server.clone().create_mailbox(ROOT)?;
        server.0.lock().unwrap().mailboxes.get_mut(ROOT).unwrap().push((59, raw.to_vec()));
        let adapter = adapter(&server, directory.path());
        assert_eq!(adapter.list_memos()?.len(), 1);
        assert_eq!(adapter.get_memo(&parsed.id)?.content, parsed.content);

        let edited = note(&parsed.id, "# Garage\n\nBanden en olie goed", None);
        adapter.apply_event(&SyncEvent::NoteUpdated { note: edited })?;
        let memos = adapter.list_memos()?;
        assert_eq!(memos.len(), 1);
        assert_eq!(memos[0].id, parsed.id);
        assert_eq!(memos[0].content, "# Garage\n\nBanden en olie goed");
        Ok(())
    }

    #[test]
    fn events_manage_folders_and_notes_on_the_server() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let server = FakeMail::default();
        let adapter = adapter(&server, directory.path());
        let folder = |id: &str, name: &str| crate::FolderSnapshot {
            id: id.to_owned(),
            account_id: Some("account".to_owned()),
            name: name.to_owned(),
        };

        adapter.apply_event(&SyncEvent::FolderCreated { folder: folder("local-id", "Work/Plans") })?;
        let created = SyncEvent::FolderCreated { folder: folder("local-id", "Work/Plans") };
        assert_eq!(adapter.applied_remote_name(&created).as_deref(), Some("Notes/Work-Plans"));
        assert_eq!(adapter.list_folders()?[0].name, "Work-Plans");

        let first = note("note-1", "# One\n\nbody", Some("Notes/Work-Plans"));
        adapter.apply_event(&SyncEvent::NoteCreated { note: first.clone() })?;
        adapter.apply_event(&SyncEvent::NoteCreated { note: note("note-2", "# Two", None) })?;
        let memos = adapter.list_memos()?;
        assert_eq!(memos.len(), 2);
        assert_eq!(memos[0].folder_id.as_deref(), Some("Notes/Work-Plans"));
        assert_eq!(memos[1].folder_id, None);
        assert_eq!(memos[0].title.as_deref(), Some("One"));

        let mut moved = first.clone();
        moved.folder_id = None;
        moved.content = "# One\n\nedited".to_owned();
        moved.archived = true;
        adapter.apply_event(&SyncEvent::NoteArchived { note: moved })?;
        let memo = adapter.get_memo("note-1")?;
        assert_eq!((memo.folder_id, memo.archived), (None, true));
        assert_eq!(memo.content, "# One\n\nedited");
        assert_eq!(adapter.list_memos()?.len(), 2);
        assert!(adapter.list_folder_memos("Notes/Work-Plans")?.is_empty());

        adapter.apply_event(&SyncEvent::FolderUpdated {
            folder: folder("Notes/Work-Plans", "Projects"),
        })?;
        assert_eq!(adapter.list_folders()?[0].id, "Notes/Projects");

        adapter.apply_event(&SyncEvent::NoteDeleted { note: first })?;
        assert!(adapter.get_memo_if_exists("note-1")?.is_none());
        adapter.apply_event(&SyncEvent::FolderDeleted { folder_id: "Notes/Projects".to_owned() })?;
        assert!(adapter.list_folders()?.is_empty());
        assert!(adapter.get_memo("note-1").is_err());
        Ok(())
    }

    #[test]
    fn note_lookups_require_an_exact_id() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let server = FakeMail::default();
        let adapter = adapter(&server, directory.path());
        adapter.apply_event(&SyncEvent::NoteCreated { note: note("abcdef", "# Long", None) })?;

        assert!(adapter.get_memo_if_exists("abc")?.is_none());
        assert!(adapter.get_memo_if_exists("abcdef")?.is_some());
        assert!(adapter.get_memo_if_exists("bad id").is_err());
        Ok(())
    }

    #[test]
    fn a_leftover_duplicate_resolves_to_the_newest_version() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let server = FakeMail::default();
        let adapter = adapter(&server, directory.path());
        let mut old = note("dup", "# Old", None);
        old.updated_at = 1_000_000_000_000;
        let mut new = note("dup", "# New", None);
        new.updated_at = 1_700_000_000_000;
        {
            let mut mail = server.clone();
            mail.create_mailbox(ROOT)?;
            mail.append(ROOT, &build_message(&new, "me@example.com", directory.path())?)?;
            mail.append(ROOT, &build_message(&old, "me@example.com", directory.path())?)?;
        }

        let memos = adapter.list_memos()?;

        assert_eq!(memos.len(), 1);
        assert_eq!(memos[0].content, "# New");
        Ok(())
    }

    struct DroppedConnection;

    impl MailStore for DroppedConnection {
        fn delimiter(&mut self) -> Result<String> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into())
        }
        fn mailboxes(&mut self, _: &str) -> Result<Vec<String>> {
            self.delimiter().map(|_| Vec::new())
        }
        fn create_mailbox(&mut self, _: &str) -> Result<()> {
            self.delimiter().map(|_| ())
        }
        fn delete_mailbox(&mut self, _: &str) -> Result<()> {
            self.delimiter().map(|_| ())
        }
        fn rename_mailbox(&mut self, _: &str, _: &str) -> Result<()> {
            self.delimiter().map(|_| ())
        }
        fn uids(&mut self, _: &str) -> Result<Option<(u32, Vec<u32>)>> {
            self.delimiter().map(|_| None)
        }
        fn fetch(&mut self, _: &str, _: &[u32]) -> Result<Vec<StoredMessage>> {
            self.delimiter().map(|_| Vec::new())
        }
        fn append(&mut self, _: &str, _: &[u8]) -> Result<()> {
            self.delimiter().map(|_| ())
        }
        fn remove(&mut self, _: &str, _: &[u32]) -> Result<()> {
            self.delimiter().map(|_| ())
        }
    }

    #[test]
    fn a_connection_that_timed_out_while_idle_is_replaced() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let adapter = adapter(&FakeMail::default(), directory.path());
        *adapter.session.lock().unwrap() = Some(Box::new(DroppedConnection));

        adapter.check_connection()?;
        Ok(())
    }

    #[test]
    fn connection_and_rejection_errors_are_classified() {
        let io = anyhow::Error::new(::imap::Error::Io(std::io::Error::other("down")));
        assert!(is_connection_error(&io));
        assert!(!is_rejected_request(&io));
        let rejected = anyhow::Error::new(ImapRejected("quota".to_owned()));
        assert!(is_rejected_request(&rejected));
        assert!(!is_connection_error(&rejected));
        assert!(!is_connection_error(&anyhow::anyhow!("other")));
    }
}

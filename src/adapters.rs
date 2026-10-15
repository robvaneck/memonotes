use crate::i18n::tr;
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::memo::{Memo, MemoFolder, TimestampMillis};
use crate::memos::{MemosClient, RemoteMemo};
use crate::{NoteSnapshot, SyncEvent};

pub trait BackendAdapter: Send {
    fn check_connection(&self) -> Result<()>;
    fn list_memos(&self) -> Result<Vec<Memo>>;
    fn list_folders(&self) -> Result<Vec<MemoFolder>>;
    fn get_memo(&self, name: &str) -> Result<Memo>;
    fn get_memo_if_exists(&self, name: &str) -> Result<Option<Memo>>;
    fn get_folder(&self, name: &str) -> Result<MemoFolder>;
    fn list_folder_memos(&self, name: &str) -> Result<Vec<Memo>>;
    fn apply_event(&self, event: &SyncEvent) -> Result<()>;
    // `base_content` is the last synced content of the note, when known.
    fn apply_event_with_base(&self, event: &SyncEvent, _base_content: Option<&str>) -> Result<()> {
        self.apply_event(event)
    }
    fn uses_plain_http(&self) -> bool {
        false
    }
    // The name the backend gave a folder that an event created or renamed, when it differs from
    // the name the event carried.
    fn applied_remote_name(&self, _event: &SyncEvent) -> Option<String> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Note,
    Folder,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Folder => "folder",
        }
    }
}

pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub available: bool,
}

const MEMOS: &str = "memos";
const IMAP: &str = "imap";
const MEMOS_CONTENT_LIMIT_BYTES: usize = 8 * 1024;

pub const PROVIDERS: [ProviderInfo; 2] = [
    ProviderInfo {
        id: MEMOS,
        name: "Memos",
        available: true,
    },
    ProviderInfo {
        id: IMAP,
        name: "IMAP",
        available: true,
    },
];

pub fn provider_name(provider: &str) -> &'static str {
    PROVIDERS
        .iter()
        .find(|info| info.id == provider)
        .map_or("Backend", |info| info.name)
}

pub fn is_supported(provider: &str) -> bool {
    PROVIDERS
        .iter()
        .any(|info| info.id == provider && info.available)
}

pub fn connect(provider: &str, server_url: &str, token: &str) -> Result<Box<dyn BackendAdapter>> {
    match provider {
        MEMOS => Ok(Box::new(MemosAdapter::new(server_url, token)?)),
        IMAP => Ok(Box::new(crate::imap_notes::ImapAdapter::new(
            server_url, token,
        )?)),
        _ => bail!(tr("account.error.backend_unavailable")),
    }
}

// Remote name for a local item that the backend has not seen yet.
pub fn default_remote_name(provider: &str, kind: ResourceKind, local_id: &str) -> String {
    match provider {
        MEMOS => {
            let collection = memos_collection(kind);
            if local_id.starts_with(&format!("{collection}/")) {
                local_id.to_owned()
            } else {
                format!("{collection}/{local_id}")
            }
        }
        _ => local_id.to_owned(),
    }
}

// The local ID a remote item reuses when a local item with that ID already exists.
pub fn local_id_from_remote_name(
    provider: &str,
    kind: ResourceKind,
    remote_name: &str,
) -> Result<String> {
    match provider {
        MEMOS => remote_name
            .strip_prefix(&format!("{}/", memos_collection(kind)))
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .map(str::to_owned)
            .context(tr("api.memos.error.invalid_resource_name")),
        _ => Ok(remote_name.to_owned()),
    }
}

// The size the backend would store and its limit, when the content does not fit.
pub fn content_limit_violation(provider: &str, content: &str) -> Option<(usize, usize)> {
    match provider {
        MEMOS => {
            let size = crate::images::to_remote_content(content).len();
            (size > MEMOS_CONTENT_LIMIT_BYTES).then_some((size, MEMOS_CONTENT_LIMIT_BYTES))
        }
        _ => None,
    }
}

pub fn is_connection_error(error: &anyhow::Error) -> bool {
    crate::memos::is_connection_error(error) || crate::imap_notes::is_connection_error(error)
}

// The server rejected this particular request, so retrying it unchanged cannot succeed.
pub fn is_rejected_request(error: &anyhow::Error) -> bool {
    crate::memos::is_bad_request(error) || crate::imap_notes::is_rejected_request(error)
}

fn memos_collection(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Note => "memos",
        ResourceKind::Folder => "spaces",
    }
}

struct MemosMutation {
    method: Method,
    resource: String,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
}

impl MemosMutation {
    fn bind_attachments(&mut self, attachments: &[String]) {
        if attachments.is_empty() {
            return;
        }
        let Some(body) = self.body.as_mut() else {
            return;
        };
        body["attachments"] = attachments
            .iter()
            .map(|name| json!({"name": name}))
            .collect();
        for (key, value) in &mut self.query {
            if *key == "updateMask" {
                *value = format!("{value},attachments");
            }
        }
    }

    fn from_event(event: &SyncEvent) -> Result<Self> {
        match event {
            SyncEvent::FolderCreated { folder } | SyncEvent::FolderUpdated { folder } => {
                ensure!(
                    folder.account_id.is_some(),
                    tr("api.memos.error.local_folder_upload_blocked")
                );
                let id = resource_id(&folder.id, "spaces")?;
                ensure!(
                    !folder.name.trim().is_empty(),
                    tr("storage.error.folder_name_required")
                );
                if matches!(event, SyncEvent::FolderCreated { .. }) {
                    Ok(Self {
                        method: Method::POST,
                        resource: "spaces".to_owned(),
                        query: vec![("spaceId", id.to_owned())],
                        body: Some(json!({"title": folder.name})),
                    })
                } else {
                    Ok(Self {
                        method: Method::PATCH,
                        resource: format!("spaces/{id}"),
                        query: vec![("updateMask", "title".to_owned())],
                        body: Some(json!({"name": format!("spaces/{id}"), "title": folder.name})),
                    })
                }
            }
            SyncEvent::FolderDeleted { folder_id } => {
                let id = resource_id(folder_id, "spaces")?;
                Ok(Self {
                    method: Method::DELETE,
                    resource: format!("spaces/{id}"),
                    query: Vec::new(),
                    body: None,
                })
            }
            SyncEvent::NoteCreated { note }
            | SyncEvent::NoteRestored { note }
            | SyncEvent::NoteUpdated { note }
            | SyncEvent::NoteArchived { note }
            | SyncEvent::NoteUnarchived { note }
            | SyncEvent::NoteDeleted { note } => {
                ensure!(
                    note.account_id.is_some(),
                    tr("api.memos.error.local_note_upload_blocked")
                );
                let id = resource_id(&note.id, "memos")?;
                let name = format!("memos/{id}");
                if matches!(event, SyncEvent::NoteDeleted { .. }) {
                    return Ok(Self {
                        method: Method::DELETE,
                        resource: name,
                        query: Vec::new(),
                        body: None,
                    });
                }
                if matches!(
                    event,
                    SyncEvent::NoteArchived { .. } | SyncEvent::NoteUnarchived { .. }
                ) {
                    let archived = matches!(event, SyncEvent::NoteArchived { .. });
                    return Ok(Self {
                        method: Method::PATCH,
                        resource: name.clone(),
                        query: vec![("updateMask", "state".to_owned())],
                        body: Some(json!({"name": name, "state": memo_state(archived)})),
                    });
                }
                let mut body = memo_body(note)?;
                if matches!(
                    event,
                    SyncEvent::NoteCreated { .. } | SyncEvent::NoteRestored { .. }
                ) {
                    body["visibility"] = json!("PRIVATE");
                    Ok(Self {
                        method: Method::POST,
                        resource: "memos".to_owned(),
                        query: vec![("memoId", id.to_owned())],
                        body: Some(body),
                    })
                } else {
                    body["name"] = json!(name);
                    Ok(Self {
                        method: Method::PATCH,
                        resource: name,
                        query: vec![("updateMask", "content,pinned,state,space".to_owned())],
                        body: Some(body),
                    })
                }
            }
        }
    }
}

fn resource_id<'value>(value: &'value str, collection: &str) -> Result<&'value str> {
    let prefix = format!("{collection}/");
    let id = value.strip_prefix(&prefix).unwrap_or(value);
    ensure!(
        !id.is_empty()
            && id.len() <= 36
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id.as_bytes()[id.len() - 1].is_ascii_alphanumeric(),
        tr("api.memos.error.invalid_resource_id")
    );
    Ok(id)
}

fn memo_state(archived: bool) -> &'static str {
    if archived { "ARCHIVED" } else { "NORMAL" }
}

fn memo_body(note: &NoteSnapshot) -> Result<Value> {
    let space = note
        .folder_id
        .as_deref()
        .map(|folder_id| resource_id(folder_id, "spaces").map(|id| format!("spaces/{id}")))
        .transpose()?;
    Ok(json!({
        "content": crate::images::to_remote_content(&note.content),
        "pinned": note.pinned,
        "state": memo_state(note.archived),
        "space": space,
    }))
}

pub struct MemosAdapter {
    client: MemosClient,
}

impl MemosAdapter {
    pub fn new(server_url: &str, token: &str) -> Result<Self> {
        Ok(Self {
            client: MemosClient::new(server_url, token)?,
        })
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self {
            client: MemosClient::from_env()?,
        })
    }

    // Serves remote attachments as local image files so stored content keeps local references.
    fn localize_images(&self, remote: RemoteMemo) -> RemoteMemo {
        if crate::images::remote_images(&remote.content, &self.client.origin()).is_empty() {
            return remote;
        }
        match crate::images::image_directory() {
            Ok(directory) => self.localize_images_into(&directory, remote),
            Err(error) => {
                eprintln!("{error:#}");
                remote
            }
        }
    }

    fn localize_images_into(
        &self,
        directory: &std::path::Path,
        mut remote: RemoteMemo,
    ) -> RemoteMemo {
        let origin = self.client.origin();
        for image in crate::images::remote_images(&remote.content, &origin) {
            if let Err(error) = self.download_image(directory, &image) {
                eprintln!("{error:#}");
            }
        }
        remote.content = crate::images::to_local_content(&remote.content, &origin);
        remote
    }

    fn download_image(
        &self,
        directory: &std::path::Path,
        image: &crate::images::ImageRef,
    ) -> Result<()> {
        let path = directory.join(&image.local);
        if path.exists() {
            return Ok(());
        }
        let bytes = match self.client.download_attachment(&image.id, &image.file) {
            Ok(bytes) => bytes,
            // Without a filename in the URL, retry with the name the server stores.
            Err(error) if image.file.is_empty() => {
                let filename = self.client.attachment_filename(&image.id).ok().flatten();
                match filename {
                    Some(filename) => self
                        .client
                        .download_attachment(&image.id, &filename)
                        .map_err(|_| error)?,
                    None => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        std::fs::create_dir_all(directory)?;
        let partial = path.with_extension("part");
        std::fs::write(&partial, bytes)?;
        std::fs::rename(partial, path)?;
        Ok(())
    }

    // Returns the attachment names that the memo should link to.
    fn upload_images(&self, content: &str) -> Result<Vec<String>> {
        if crate::images::local_images(content).is_empty() {
            return Ok(Vec::new());
        }
        self.upload_images_from(&crate::images::image_directory()?, content)
    }

    fn upload_images_from(
        &self,
        directory: &std::path::Path,
        content: &str,
    ) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for image in crate::images::local_images(content) {
            // Attachments made elsewhere are already bound to the memo.
            if !crate::images::is_uploadable(&image) {
                continue;
            }
            let (id, name) = (image.id.as_str(), image.local.as_str());
            if !self.client.attachment_exists(id)? {
                let path = directory.join(name);
                if !path.is_file() {
                    continue;
                }
                let bytes =
                    std::fs::read(&path).context(tr("api.memos.error.image_read_failed"))?;
                self.client
                    .upload_attachment(id, name, crate::images::mime_type(name), &bytes)?;
            }
            names.push(format!("attachments/{id}"));
        }
        Ok(names)
    }

    // Memos rejects content that embeds an attachment not bound to the memo in the same mutation.
    fn memo_attachments(&self, event: &SyncEvent, attachments: &[String]) -> Result<Vec<String>> {
        let SyncEvent::NoteUpdated { note } = event else {
            return Ok(attachments.to_vec());
        };
        let id = resource_id(&note.id, "memos")?;
        let mut bound = self.client.memo_attachment_names(id)?;
        for name in attachments {
            if !bound.contains(name) {
                bound.push(name.clone());
            }
        }
        Ok(bound)
    }

    fn apply_with_base(&self, event: &SyncEvent, base_content: Option<&str>) -> Result<()> {
        let content_note = match event {
            SyncEvent::NoteCreated { note }
            | SyncEvent::NoteRestored { note }
            | SyncEvent::NoteUpdated { note } => Some(note),
            _ => None,
        };
        let attachments = match content_note {
            Some(note) => {
                let uploaded = self.upload_images(&note.content)?;
                if uploaded.is_empty() {
                    uploaded
                } else {
                    self.memo_attachments(event, &uploaded)?
                }
            }
            None => Vec::new(),
        };
        self.send_event(event, &attachments, base_content)
    }

    fn send_event(
        &self,
        event: &SyncEvent,
        attachments: &[String],
        base_content: Option<&str>,
    ) -> Result<()> {
        let mut mutation = MemosMutation::from_event(event)?;
        mutation.bind_attachments(attachments);
        let created_note = match event {
            SyncEvent::NoteCreated { note } | SyncEvent::NoteRestored { note } => Some(note),
            _ => None,
        };
        if let Some(note) = created_note {
            return match self.client.send_mutation(
                mutation.method,
                &mutation.resource,
                &mutation.query,
                mutation.body.as_ref(),
            ) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let id = resource_id(&note.id, "memos")?;
                    match self.client.get_memo(id) {
                        Ok(remote) if remote_matches_note(&remote, note) => Ok(()),
                        Ok(_) => Err(error.context(tr("api.memos.error.create_id_conflict"))),
                        Err(_) => Err(error),
                    }
                }
            };
        }
        let response = self.client.send_mutation_json(
            mutation.method,
            &mutation.resource,
            &mutation.query,
            mutation.body.as_ref(),
        )?;
        if let SyncEvent::NoteUpdated { note } = event {
            self.remove_stale_attachments(note, &response, base_content)?;
        }
        Ok(())
    }

    // Attachments are pruned when they were in the last synced content and the note no longer
    // uses them, or when Memonotes made them itself (`<id>/<id>.<ext>`). Others stay bound.
    fn remove_stale_attachments(
        &self,
        note: &NoteSnapshot,
        memo: &Value,
        base_content: Option<&str>,
    ) -> Result<()> {
        let Some(bound) = memo.get("attachments").and_then(Value::as_array) else {
            return Ok(());
        };
        let removed_from_text: HashSet<String> = base_content
            .map(|content| {
                crate::images::local_images(content)
                    .into_iter()
                    .map(|image| image.id)
                    .collect()
            })
            .unwrap_or_default();
        let referenced: HashSet<String> = crate::images::local_images(&note.content)
            .into_iter()
            .map(|image| image.id)
            .collect();
        let mut kept = Vec::new();
        let mut stale = Vec::new();
        for attachment in bound {
            let Some(name) = attachment.get("name").and_then(Value::as_str) else {
                continue;
            };
            let id = name.strip_prefix("attachments/").unwrap_or(name);
            let managed = attachment
                .get("filename")
                .and_then(Value::as_str)
                .and_then(crate::images::attachment_id)
                == Some(id);
            if (managed || removed_from_text.contains(id)) && !referenced.contains(id) {
                stale.push(id.to_owned());
            } else {
                kept.push(json!({"name": name}));
            }
        }
        if stale.is_empty() {
            return Ok(());
        }
        let memo_name = format!("memos/{}", resource_id(&note.id, "memos")?);
        self.client.send_mutation(
            Method::PATCH,
            &memo_name,
            &[("updateMask", "attachments".to_owned())],
            Some(&json!({"name": memo_name, "attachments": kept})),
        )?;
        for id in stale {
            // A failed delete only leaves an unbound attachment behind.
            if let Err(error) =
                self.client
                    .send_mutation(Method::DELETE, &format!("attachments/{id}"), &[], None)
            {
                eprintln!("{error:#}");
            }
        }
        Ok(())
    }

    fn remote_space_is_shared(&self, name: Option<&str>) -> Result<bool> {
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return Ok(false);
        };
        let id = resource_id(name, "spaces")?;
        Ok(self.client.get_space(id)?.member_count != 1)
    }
}

impl BackendAdapter for MemosAdapter {
    fn uses_plain_http(&self) -> bool {
        self.client.uses_plain_http()
    }

    fn get_memo(&self, name: &str) -> Result<Memo> {
        let id = resource_id(name, "memos")?;
        let remote = self.localize_images(self.client.get_memo(id)?);
        let shared_space = self.remote_space_is_shared(remote.space.as_deref())?;
        memo_from_remote(remote, shared_space)
    }

    fn get_memo_if_exists(&self, name: &str) -> Result<Option<Memo>> {
        let id = resource_id(name, "memos")?;
        let Some(remote) = self.client.get_memo_if_exists(id)? else {
            return Ok(None);
        };
        let remote = self.localize_images(remote);
        let shared_space = self.remote_space_is_shared(remote.space.as_deref())?;
        Ok(Some(memo_from_remote(remote, shared_space)?))
    }

    fn get_folder(&self, name: &str) -> Result<MemoFolder> {
        let id = resource_id(name, "spaces")?;
        let space = self.client.get_space(id)?;
        Ok(MemoFolder {
            id: space.name,
            name: space.title,
            is_shared: space.member_count != 1,
        })
    }

    fn list_folder_memos(&self, name: &str) -> Result<Vec<Memo>> {
        let id = resource_id(name, "spaces")?;
        let is_shared = self.client.get_space(id)?.member_count != 1;
        self.client
            .list_space_memos(id)?
            .into_iter()
            .map(|remote| memo_from_remote(self.localize_images(remote), is_shared))
            .collect()
    }

    fn apply_event(&self, event: &SyncEvent) -> Result<()> {
        self.apply_with_base(event, None)
    }

    fn apply_event_with_base(&self, event: &SyncEvent, base_content: Option<&str>) -> Result<()> {
        self.apply_with_base(event, base_content)
    }

    fn check_connection(&self) -> Result<()> {
        self.client.check_connection()
    }

    fn list_memos(&self) -> Result<Vec<Memo>> {
        let personal_spaces: HashSet<_> = self
            .client
            .list_spaces()?
            .into_iter()
            .filter(|space| space.member_count == 1)
            .map(|space| space.name)
            .collect();
        self.client
            .list_memos()?
            .into_iter()
            .map(|remote| {
                let remote = self.localize_images(remote);
                let shared_space = remote
                    .space
                    .as_ref()
                    .is_some_and(|space| !personal_spaces.contains(space));
                memo_from_remote(remote, shared_space)
            })
            .collect()
    }

    fn list_folders(&self) -> Result<Vec<MemoFolder>> {
        Ok(self
            .client
            .list_spaces()?
            .into_iter()
            .map(|space| MemoFolder {
                id: space.name,
                name: space.title,
                is_shared: space.member_count != 1,
            })
            .collect())
    }
}

fn remote_matches_note(remote: &RemoteMemo, note: &NoteSnapshot) -> bool {
    let state_matches = if note.archived {
        remote.state == "ARCHIVED"
    } else {
        remote.state.is_empty() || remote.state == "NORMAL"
    };
    let remote_space = remote.space.as_deref().filter(|space| !space.is_empty());
    let expected_space = note.folder_id.as_deref().filter(|space| !space.is_empty());
    let expected_name = if note.id.starts_with("memos/") {
        note.id.clone()
    } else {
        format!("memos/{}", note.id)
    };
    remote.name == expected_name
        && remote.content == crate::images::to_remote_content(&note.content)
        && remote.pinned == note.pinned
        && state_matches
        && remote_space == expected_space
        && (remote.visibility.is_empty() || remote.visibility == "PRIVATE")
}

fn memo_from_remote(remote: RemoteMemo, shared_space: bool) -> Result<Memo> {
    let archived = match remote.state.as_str() {
        "" | "NORMAL" => false,
        "ARCHIVED" => true,
        state => bail!(crate::tr_format!(
            "api.memos.error.unknown_note_state",
            state = state
        )),
    };
    Ok(Memo {
        id: remote.name,
        title: Some(crate::memo::editor_parts(&remote.content).0.to_owned()),
        content: remote.content,
        folder_id: remote.space.filter(|space| !space.is_empty()),
        pinned: remote.pinned,
        archived,
        is_shared: shared_space || remote.visibility != "PRIVATE",
        updated_at: remote
            .update_time
            .as_deref()
            .map(parse_memos_update_time)
            .transpose()?,
    })
}

fn parse_memos_update_time(value: &str) -> Result<TimestampMillis> {
    gio::glib::DateTime::from_iso8601(value, None)
        .context(tr("api.memos.error.invalid_update_time"))?
        .to_unix()
        .checked_mul(1000)
        .context(tr("api.memos.error.update_time_out_of_range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderSnapshot;
    use anyhow::Context;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    struct CapturedRequest {
        method: String,
        url: reqwest::Url,
        authorization: String,
        body: Option<Value>,
    }

    fn mock_server(
        statuses: Vec<u16>,
    ) -> Result<(String, thread::JoinHandle<Result<Vec<CapturedRequest>>>)> {
        mock_server_with_responses(
            statuses
                .into_iter()
                .map(|status| (status, String::new()))
                .collect(),
        )
    }

    fn mock_server_with_responses(
        responses: Vec<(u16, String)>,
    ) -> Result<(String, thread::JoinHandle<Result<Vec<CapturedRequest>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<Vec<CapturedRequest>> {
            let mut requests = Vec::new();
            for (status, response_body) in responses {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut first_line = String::new();
                reader.read_line(&mut first_line)?;
                let mut parts = first_line.split_whitespace();
                let method = parts.next().context("Request mist methode")?.to_owned();
                let path = parts.next().context("Request mist URL")?;
                let url = reqwest::Url::parse(&format!("http://{address}{path}"))?;
                let mut length = 0;
                let mut authorization = String::new();
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header)?;
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>()?;
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = value.trim().to_owned();
                        }
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes)?;
                let body = if bytes.is_empty() {
                    None
                } else {
                    Some(serde_json::from_slice(&bytes)?)
                };
                requests.push(CapturedRequest {
                    method,
                    url,
                    authorization,
                    body,
                });
                stream.write_all(
                        format!(
                            "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        )
                        .as_bytes(),
                    )?;
            }
            Ok(requests)
        });
        Ok((format!("http://{address}/memos-server"), server))
    }

    #[test]
    fn create_conflict_with_matching_stable_id_is_reconciled() -> Result<()> {
        let note = note_snapshot();
        let body = json!({
            "name": format!("memos/{}", note.id),
            "content": note.content,
            "state": "NORMAL",
            "visibility": "PRIVATE",
            "space": note.folder_id,
            "pinned": note.pinned,
        })
        .to_string();
        let (url, server) = mock_server_with_responses(vec![(409, String::new()), (200, body)])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;

        adapter.apply_event(&SyncEvent::NoteCreated { note: note.clone() })?;

        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].url.path(),
            format!("/memos-server/api/v1/memos/{}", note.id)
        );
        assert_eq!(requests[1].authorization, "Bearer test-token");
        Ok(())
    }

    #[test]
    fn create_conflict_with_different_remote_content_is_not_acknowledged() -> Result<()> {
        let note = note_snapshot();
        let body = json!({
            "name": format!("memos/{}", note.id),
            "content": "# Andere inhoud",
            "state": "NORMAL",
            "visibility": "PRIVATE",
            "space": note.folder_id,
            "pinned": note.pinned,
        })
        .to_string();
        let (url, server) = mock_server_with_responses(vec![(409, String::new()), (200, body)])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;

        assert!(
            adapter
                .apply_event(&SyncEvent::NoteCreated { note })
                .is_err()
        );
        assert_eq!(
            server.join().expect("mock server thread panicked")?.len(),
            2
        );
        Ok(())
    }

    fn note_snapshot() -> NoteSnapshot {
        NoteSnapshot {
            id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            account_id: Some("account-one".to_owned()),
            folder_id: Some("spaces/work".to_owned()),
            content: "# Titel\n\nInhoud".to_owned(),
            title: "Lokale weergavetitel".to_owned(),
            updated_at: 123,
            pinned: true,
            archived: false,
        }
    }

    fn remote_memo(update_time: Option<&str>) -> RemoteMemo {
        RemoteMemo {
            name: "memos/one".to_owned(),
            content: String::new(),
            state: "NORMAL".to_owned(),
            visibility: String::new(),
            pinned: false,
            space: None,
            update_time: update_time.map(ToOwned::to_owned),
            property: None,
        }
    }

    #[test]
    fn memos_adapter_normalizes_its_update_time_format() -> Result<()> {
        let memo = memo_from_remote(remote_memo(Some("2026-10-10T12:00:00Z")), false)?;
        assert_eq!(memo.updated_at, Some(1791633600000));

        let offset_memo = memo_from_remote(remote_memo(Some("2026-10-10T14:00:00+02:00")), false)?;
        assert_eq!(offset_memo.updated_at, Some(1791633600000));
        assert!(memo_from_remote(remote_memo(Some("not a timestamp")), false).is_err());
        Ok(())
    }

    #[test]
    fn events_send_expected_memos_api_requests() -> Result<()> {
        let folder = FolderSnapshot {
            id: "work".to_owned(),
            account_id: Some("account-one".to_owned()),
            name: "Werk".to_owned(),
        };
        let note = note_snapshot();
        let memo_name = format!("memos/{}", note.id);
        let create_body = json!({
            "content": note.content, "pinned": true, "state": "NORMAL",
            "space": "spaces/work", "visibility": "PRIVATE",
        });
        let mut unfiled = note.clone();
        unfiled.folder_id = None;
        let cases = vec![
            (
                SyncEvent::FolderCreated {
                    folder: folder.clone(),
                },
                "POST",
                "spaces".to_owned(),
                vec![("spaceId", "work".to_owned())],
                Some(json!({"title": "Werk"})),
            ),
            (
                SyncEvent::FolderUpdated { folder },
                "PATCH",
                "spaces/work".to_owned(),
                vec![("updateMask", "title".to_owned())],
                Some(json!({"name": "spaces/work", "title": "Werk"})),
            ),
            (
                SyncEvent::FolderDeleted {
                    folder_id: "work".to_owned(),
                },
                "DELETE",
                "spaces/work".to_owned(),
                Vec::new(),
                None,
            ),
            (
                SyncEvent::NoteCreated { note: note.clone() },
                "POST",
                "memos".to_owned(),
                vec![("memoId", note.id.clone())],
                Some(create_body.clone()),
            ),
            (
                SyncEvent::NoteRestored { note: note.clone() },
                "POST",
                "memos".to_owned(),
                vec![("memoId", note.id.clone())],
                Some(create_body),
            ),
            (
                SyncEvent::NoteUpdated { note: unfiled },
                "PATCH",
                memo_name.clone(),
                vec![("updateMask", "content,pinned,state,space".to_owned())],
                Some(
                    json!({"name": memo_name, "content": note.content, "pinned": true, "state": "NORMAL", "space": null}),
                ),
            ),
            (
                SyncEvent::NoteArchived { note: note.clone() },
                "PATCH",
                memo_name.clone(),
                vec![("updateMask", "state".to_owned())],
                Some(json!({"name": memo_name, "state": "ARCHIVED"})),
            ),
            (
                SyncEvent::NoteUnarchived { note: note.clone() },
                "PATCH",
                memo_name.clone(),
                vec![("updateMask", "state".to_owned())],
                Some(json!({"name": memo_name, "state": "NORMAL"})),
            ),
            (
                SyncEvent::NoteDeleted { note: note.clone() },
                "DELETE",
                memo_name,
                Vec::new(),
                None,
            ),
        ];
        let (url, server) = mock_server(vec![204; cases.len()])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let backend: &dyn BackendAdapter = &adapter;
        for (event, ..) in &cases {
            backend.apply_event(event)?;
        }
        let requests = server.join().expect("mock server thread panicked")?;
        for (request, (_, method, resource, query, body)) in requests.iter().zip(&cases) {
            assert_eq!(&request.method, method);
            assert_eq!(
                request.url.path(),
                format!("/memos-server/api/v1/{resource}")
            );
            let actual_query: Vec<_> = request
                .url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected_query: Vec<_> = query
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            assert_eq!(actual_query, expected_query);
            assert_eq!(&request.body, body);
            assert_eq!(request.authorization, "Bearer test-token");
        }
        Ok(())
    }

    #[test]
    fn local_images_are_uploaded_and_bound_in_the_memo_mutation() -> Result<()> {
        const NAME: &str = "550e8400-e29b-41d4-a716-446655440000.png";
        let id = &NAME[..36];
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join(NAME), b"fo")?;
        let mut note = note_snapshot();
        note.content = format!("# Titel\n\n![foto](memonotes-image://{NAME}#w=320)");
        let (url, server) = mock_server_with_responses(vec![
            (404, String::new()),
            (200, "{}".to_owned()),
            (
                200,
                r#"{"attachments":[{"name":"attachments/other"}]}"#.to_owned(),
            ),
            (200, "{}".to_owned()),
        ])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let event = SyncEvent::NoteUpdated { note: note.clone() };

        let uploaded = adapter.upload_images_from(directory.path(), &note.content)?;
        assert_eq!(uploaded, [format!("attachments/{id}")]);
        let bound = adapter.memo_attachments(&event, &uploaded)?;
        adapter.send_event(&event, &bound, None)?;

        let requests = server.join().expect("mock server thread panicked")?;
        let base = "/memos-server/api/v1";
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].url.path(), format!("{base}/attachments/{id}"));
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].url.path(), format!("{base}/attachments"));
        assert_eq!(
            requests[1].url.query_pairs().next().unwrap(),
            ("attachmentId".into(), id.into())
        );
        assert_eq!(
            requests[1].body,
            Some(json!({"filename": NAME, "type": "image/png", "content": "Zm8="}))
        );
        assert_eq!(requests[2].method, "GET");
        assert_eq!(
            requests[2].url.path(),
            format!("{base}/memos/{}/attachments", note.id)
        );
        assert_eq!(requests[3].method, "PATCH");
        assert_eq!(
            requests[3].url.query_pairs().next().unwrap(),
            (
                "updateMask".into(),
                "content,pinned,state,space,attachments".into()
            )
        );
        let body = requests[3].body.as_ref().unwrap();
        assert_eq!(
            body["content"],
            format!("# Titel\n\n![foto|320](/file/attachments/{id}/{NAME})")
        );
        assert_eq!(
            body["attachments"],
            json!([{"name": "attachments/other"}, {"name": format!("attachments/{id}")}])
        );
        Ok(())
    }

    #[test]
    fn removed_images_are_unbound_and_deleted_but_foreign_attachments_stay() -> Result<()> {
        const NAME: &str = "550e8400-e29b-41d4-a716-446655440000.png";
        let id = &NAME[..36];
        let mut note = note_snapshot();
        note.content = "# Titel".to_owned();
        let updated = json!({
            "name": format!("memos/{}", note.id),
            "attachments": [
                {"name": format!("attachments/{id}"), "filename": NAME},
                {"name": "attachments/web", "filename": "photo.png"},
            ],
        })
        .to_string();
        let (url, server) = mock_server_with_responses(vec![
            (200, updated),
            (200, "{}".to_owned()),
            (200, "{}".to_owned()),
        ])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;

        adapter.apply_event(&SyncEvent::NoteUpdated { note: note.clone() })?;

        let requests = server.join().expect("mock server thread panicked")?;
        let base = "/memos-server/api/v1";
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(requests[1].url.path(), format!("{base}/memos/{}", note.id));
        assert_eq!(
            requests[1].url.query_pairs().next().unwrap(),
            ("updateMask".into(), "attachments".into())
        );
        assert_eq!(
            requests[1].body.as_ref().unwrap()["attachments"],
            json!([{"name": "attachments/web"}])
        );
        assert_eq!(requests[2].method, "DELETE");
        assert_eq!(requests[2].url.path(), format!("{base}/attachments/{id}"));
        Ok(())
    }

    #[test]
    fn created_memos_carry_their_attachments_in_the_create_body() -> Result<()> {
        let mut mutation = MemosMutation::from_event(&SyncEvent::NoteCreated {
            note: note_snapshot(),
        })?;
        mutation.bind_attachments(&["attachments/one".to_owned()]);
        assert_eq!(
            mutation.body.unwrap()["attachments"],
            json!([{"name": "attachments/one"}])
        );
        assert_eq!(mutation.query, [("memoId", note_snapshot().id)]);
        Ok(())
    }

    #[test]
    fn attachments_without_filename_in_the_url_are_downloaded_with_a_fallback() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (url, server) = mock_server_with_responses(vec![
            (404, String::new()),
            (200, r#"{"filename":"my photo.png"}"#.to_owned()),
            (200, "bytes".to_owned()),
        ])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let remote = RemoteMemo {
            content: "![it](/file/attachments/Jd54omtoGCZew7ZpEiqu7g)".to_owned(),
            ..remote_memo(None)
        };

        let localized = adapter.localize_images_into(directory.path(), remote);

        assert_eq!(
            localized.content,
            "![it](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g)"
        );
        assert_eq!(
            std::fs::read(directory.path().join("memos_Jd54omtoGCZew7ZpEiqu7g"))?,
            b"bytes"
        );
        let requests = server.join().expect("mock server thread panicked")?;
        let paths: Vec<_> = requests.iter().map(|request| request.url.path()).collect();
        assert_eq!(
            paths,
            [
                "/memos-server/file/attachments/Jd54omtoGCZew7ZpEiqu7g",
                "/memos-server/api/v1/attachments/Jd54omtoGCZew7ZpEiqu7g",
                "/memos-server/file/attachments/Jd54omtoGCZew7ZpEiqu7g/my%20photo.png",
            ]
        );
        Ok(())
    }

    #[test]
    fn web_attachments_are_downloaded_under_encoded_local_names() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (url, server) = mock_server_with_responses(vec![(200, "bytes".to_owned())])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let remote = RemoteMemo {
            content: "![foto](/file/attachments/abc123/photo.png)".to_owned(),
            ..remote_memo(None)
        };

        let localized = adapter.localize_images_into(directory.path(), remote);

        let local = "memos_abc123_70686f746f2e706e67";
        assert_eq!(
            localized.content,
            format!("![foto](memonotes-image://{local})")
        );
        assert_eq!(std::fs::read(directory.path().join(local))?, b"bytes");
        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(
            requests[0].url.path(),
            "/memos-server/file/attachments/abc123/photo.png"
        );
        Ok(())
    }

    #[test]
    fn remote_images_are_downloaded_and_mapped_to_local_references() -> Result<()> {
        const NAME: &str = "550e8400-e29b-41d4-a716-446655440000.png";
        let id = &NAME[..36];
        let directory = tempfile::tempdir()?;
        let (url, server) = mock_server_with_responses(vec![(200, "bytes".to_owned())])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let remote = RemoteMemo {
            content: format!("![foto|320](/file/attachments/{id}/{NAME})"),
            ..remote_memo(None)
        };

        let localized = adapter.localize_images_into(directory.path(), remote);

        assert_eq!(
            localized.content,
            format!("![foto](memonotes-image://{NAME}#w=320)")
        );
        let stored = directory.path().join(NAME);
        assert_eq!(std::fs::read(stored)?, b"bytes");
        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(
            requests[0].url.path(),
            format!("/memos-server/file/attachments/{id}/{NAME}")
        );
        Ok(())
    }

    #[test]
    fn blocks_local_events_and_invalid_resource_ids() -> Result<()> {
        let adapter = MemosAdapter::new("http://127.0.0.1:9", "test-token")?;
        assert!(
            adapter
                .apply_event(&SyncEvent::FolderDeleted {
                    folder_id: "../auth/me".to_owned(),
                })
                .is_err()
        );
        let mut note = note_snapshot();
        note.account_id = None;
        assert!(MemosMutation::from_event(&SyncEvent::NoteCreated { note: note.clone() }).is_err());
        let folder = FolderSnapshot {
            id: "work".to_owned(),
            account_id: None,
            name: "Werk".to_owned(),
        };
        assert!(MemosMutation::from_event(&SyncEvent::FolderCreated { folder }).is_err());
        note.account_id = Some("account-one".to_owned());
        for invalid in [
            "",
            "../auth/me",
            "memos/../one",
            "-one",
            "one-",
            "wrong/one",
            "1234567890123456789012345678901234567",
        ] {
            note.id = invalid.to_owned();
            assert!(
                MemosMutation::from_event(&SyncEvent::NoteDeleted { note: note.clone() }).is_err()
            );
        }
        note.id = "memos/one".to_owned();
        note.folder_id = Some("../auth/me".to_owned());
        assert!(MemosMutation::from_event(&SyncEvent::NoteUpdated { note }).is_err());
        Ok(())
    }

    #[test]
    fn outbox_updates_remote_ids_and_keeps_failed_and_newer_edits() -> Result<()> {
        let store = crate::Store::open(":memory:")?;
        let (url, server) = mock_server(vec![204, 503])?;
        store.add_backend_account("one", "Werk", "memos", &url)?;
        store.add_backend_account("two", "Ander", "memos", "https://other.example.com")?;
        let folder = MemoFolder {
            id: "spaces/remote-folder".to_owned(),
            name: "Werk".to_owned(),
            is_shared: false,
        };
        let memo = Memo {
            id: "memos/remote-memo".to_owned(),
            content: "Origineel".to_owned(),
            title: None,
            folder_id: Some(folder.id.clone()),
            pinned: false,
            archived: false,
            is_shared: false,
            updated_at: None,
        };
        store.import_backend_snapshot("one", &[folder], &[memo])?;
        let note = store
            .notes(&crate::NoteFilter {
                account_id: Some("one".to_owned()),
                ..Default::default()
            })?
            .remove(0);
        store.rename_folder(note.folder_id.as_deref().unwrap(), "Hernoemd")?;
        store.save_note(&note.id, "Mijn wijziging", "")?;
        let local_id = store.create_note(None, None)?;
        let other_id = store.create_note(Some("two"), None)?;
        assert!(store.has_backend_events("one")?);
        let account = store
            .backend_accounts()?
            .into_iter()
            .find(|account| account.id == "one")
            .unwrap();
        let adapter = MemosAdapter::new(&url, "test-token")?;
        assert!(store.sync_backend_events(&account, &adapter).is_err());
        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(
            requests[0].url.path(),
            "/memos-server/api/v1/spaces/remote-folder"
        );
        assert_eq!(
            requests[1].url.path(),
            "/memos-server/api/v1/memos/remote-memo"
        );
        assert_eq!(
            requests[1].body.as_ref().unwrap()["content"],
            "Mijn wijziging"
        );
        assert_eq!(
            requests[1].body.as_ref().unwrap()["space"],
            "spaces/remote-folder"
        );
        assert_eq!(store.pending_sync_events(100)?.len(), 3);
        store.save_note(&note.id, "Nieuwere wijziging", "")?;
        let (retry_url, retry_server) = mock_server(vec![204])?;
        let retry_adapter = MemosAdapter::new(&retry_url, "test-token")?;
        assert_eq!(store.sync_backend_events(&account, &retry_adapter)?, 1);
        let requests = retry_server.join().expect("mock server thread panicked")?;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].body.as_ref().unwrap()["content"],
            "Nieuwere wijziging"
        );
        assert!(!store.has_backend_events("one")?);
        let remaining = store.pending_sync_events(100)?;
        assert_eq!(remaining.len(), 2);
        assert!(
            matches!(&remaining[0].event, SyncEvent::NoteCreated { note } if note.id == local_id)
        );
        assert!(
            matches!(&remaining[1].event, SyncEvent::NoteCreated { note } if note.id == other_id)
        );
        assert_eq!(store.note(&note.id)?.unwrap().content, "Nieuwere wijziging");
        store.set_backend_account_enabled("one", false)?;
        store.save_note(&note.id, "Offline wijziging", "")?;
        assert!(store.sync_backend_events(&account, &retry_adapter).is_err());
        assert!(store.has_backend_events("one")?);
        Ok(())
    }

    #[test]
    fn rejected_event_does_not_block_other_notes() -> Result<()> {
        let store = crate::Store::open(":memory:")?;
        let (url, server) = mock_server(vec![400, 204])?;
        store.add_backend_account("one", "Werk", "memos", &url)?;
        let memos = ["a", "b"].map(|name| Memo {
            id: format!("memos/{name}"),
            content: "Origineel".to_owned(),
            title: None,
            folder_id: None,
            pinned: false,
            archived: false,
            is_shared: false,
            updated_at: None,
        });
        store.import_backend_snapshot("one", &[], &memos)?;
        let notes = store.notes(&crate::NoteFilter {
            account_id: Some("one".to_owned()),
            ..Default::default()
        })?;
        let (first, second) = (&notes[0], &notes[1]);
        store.save_note(&first.id, "Eerste", "")?;
        store.save_note(&second.id, "Tweede", "")?;
        let account = store.backend_accounts()?.remove(0);
        let adapter = MemosAdapter::new(&url, "test-token")?;

        let error = store.sync_backend_events(&account, &adapter).unwrap_err();

        assert!(crate::memos::is_bad_request(&error));
        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].body.as_ref().unwrap()["content"], "Tweede");
        let remaining = store.pending_sync_events(100)?;
        assert_eq!(remaining.len(), 1);
        assert!(matches!(&remaining[0].event,
            SyncEvent::NoteUpdated { note } if note.id == first.id));
        let rejections = store.sync_rejections("one")?;
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].item_id, first.id);
        assert!(rejections[0].is_note);

        store.discard_sync_rejection("one", &first.id)?;
        assert!(store.sync_rejections("one")?.is_empty());
        assert!(store.pending_sync_events(100)?.is_empty());
        assert!(store.note(&first.id)?.is_some());
        store.save_note(&first.id, "Korter", "")?;
        let (retry_url, retry_server) = mock_server(vec![204])?;
        let retry_adapter = MemosAdapter::new(&retry_url, "test-token")?;
        assert_eq!(store.sync_backend_events(&account, &retry_adapter)?, 1);
        let requests = retry_server.join().expect("mock server thread panicked")?;
        assert_eq!(requests[0].body.as_ref().unwrap()["content"], "Korter");
        assert!(store.pending_sync_events(100)?.is_empty());
        assert!(store.sync_rejections("one")?.is_empty());
        Ok(())
    }

    #[test]
    fn images_removed_from_the_text_are_pruned_even_when_added_by_another_client() -> Result<()> {
        let mut note = note_snapshot();
        note.content = "# Titel".to_owned();
        let updated = json!({
            "name": format!("memos/{}", note.id),
            "attachments": [
                {"name": "attachments/Jd54omtoGCZew7ZpEiqu7g", "filename": "photo.png"},
                {"name": "attachments/gallery", "filename": "other.png"},
            ],
        })
        .to_string();
        let (url, server) = mock_server_with_responses(vec![
            (200, updated),
            (200, "{}".to_owned()),
            (200, "{}".to_owned()),
        ])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;

        adapter.apply_event_with_base(
            &SyncEvent::NoteUpdated { note: note.clone() },
            Some("# Titel\n\n![it](memonotes-image://memos_Jd54omtoGCZew7ZpEiqu7g)"),
        )?;

        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(
            requests[1].body.as_ref().unwrap()["attachments"],
            json!([{"name": "attachments/gallery"}])
        );
        assert_eq!(requests[2].method, "DELETE");
        assert_eq!(
            requests[2].url.path(),
            "/memos-server/api/v1/attachments/Jd54omtoGCZew7ZpEiqu7g"
        );
        Ok(())
    }

    #[test]
    fn only_pending_notes_that_exist_remotely_are_prechecked() -> Result<()> {
        let store = crate::Store::open(":memory:")?;
        store.add_backend_account("one", "Werk", "memos", "http://127.0.0.1:9")?;
        let memo = Memo {
            id: "memos/remote-memo".to_owned(),
            content: "Origineel".to_owned(),
            title: None,
            folder_id: None,
            pinned: false,
            archived: false,
            is_shared: false,
            updated_at: None,
        };
        store.import_backend_snapshot("one", &[], &[memo])?;
        let existing = store
            .notes(&crate::NoteFilter {
                account_id: Some("one".to_owned()),
                ..Default::default()
            })?
            .remove(0);
        store.save_note(&existing.id, "Gewijzigd", "")?;
        store.save_note(&existing.id, "Nogmaals gewijzigd", "")?;
        let created = store.create_note(Some("one"), None)?;
        store.save_note(&created, "Nieuw", "")?;
        store.create_note(None, None)?;

        assert_eq!(
            store.pending_existing_note_resources("one")?,
            [(existing.id.clone(), "memos/remote-memo".to_owned())]
        );
        assert!(store.pending_existing_note_resources("other")?.is_empty());
        Ok(())
    }

    #[test]
    fn archived_note_deletion_retries_remote_delete_without_restoring_cache() -> Result<()> {
        let store = crate::Store::open(":memory:")?;
        let (url, server) = mock_server(vec![503, 204])?;
        store.add_backend_account("one", "Werk", "memos", &url)?;
        store.add_backend_account("two", "Ander", "memos", &url)?;
        let memo = Memo {
            id: "memos/archived-memo".to_owned(),
            content: "Gearchiveerde notitie".to_owned(),
            title: None,
            folder_id: None,
            pinned: false,
            archived: true,
            is_shared: false,
            updated_at: None,
        };
        store.import_backend_snapshot("one", &[], std::slice::from_ref(&memo))?;
        store.import_backend_snapshot("two", &[], std::slice::from_ref(&memo))?;
        let filter = crate::NoteFilter {
            account_id: Some("one".to_owned()),
            archived: true,
            ..Default::default()
        };
        let note = store.notes(&filter)?.remove(0);
        store.delete_note(&note.id)?;
        assert!(store.note(&note.id)?.is_none());
        let pending = store.pending_sync_events(100)?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(&pending[0].event,
            SyncEvent::NoteDeleted { note: snapshot }
            if snapshot.id == note.id && snapshot.archived
                && snapshot.account_id.as_deref() == Some("one")));

        let account = store
            .backend_accounts()?
            .into_iter()
            .find(|account| account.id == "one")
            .unwrap();
        let adapter = MemosAdapter::new(&url, "test-token")?;
        assert!(store.sync_backend_events(&account, &adapter).is_err());
        assert_eq!(store.pending_sync_events(100)?, pending);
        store.import_backend_snapshot("one", &[], std::slice::from_ref(&memo))?;
        assert!(store.note(&note.id)?.is_none());
        assert_eq!(store.sync_backend_events(&account, &adapter)?, 1);
        assert!(store.pending_sync_events(100)?.is_empty());
        assert!(store.note(&note.id)?.is_none());
        assert_eq!(
            store
                .notes(&crate::NoteFilter {
                    account_id: Some("two".to_owned()),
                    archived: true,
                    ..Default::default()
                })?
                .len(),
            1
        );

        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(request.method, "DELETE");
            assert_eq!(
                request.url.path(),
                "/memos-server/api/v1/memos/archived-memo"
            );
            assert!(request.url.query().is_none());
            assert!(request.body.is_none());
            assert_eq!(request.authorization, "Bearer test-token");
        }
        Ok(())
    }

    #[test]
    fn offline_outbox_replays_a_batch_in_order_after_reopening() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("offline.sqlite3");
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let offline_url = format!("http://{}", listener.local_addr()?);
        drop(listener);
        let store = crate::Store::open(&path)?;
        store.add_backend_account("one", "Werk", "memos", &offline_url)?;
        let folder_id = store.create_folder(Some("one"), "Werk")?;
        let note_id = store.create_note(Some("one"), Some(&folder_id))?;
        store.save_note(&note_id, "Eerste offline wijziging", "")?;
        store.save_note(&note_id, "Laatste offline wijziging", "")?;
        store.set_archived(&note_id, true)?;
        let pending = store.pending_sync_events(100)?;
        let account = store.backend_accounts()?.remove(0);
        let offline_adapter = MemosAdapter::new(&offline_url, "test-token")?;
        let error = store
            .sync_backend_events(&account, &offline_adapter)
            .unwrap_err();
        assert!(crate::memos::is_connection_error(&error));
        assert_eq!(store.pending_sync_events(100)?, pending);
        drop(store);

        let store = crate::Store::open(&path)?;
        assert_eq!(store.pending_sync_events(100)?, pending);
        let (url, server) = mock_server(vec![204; 4])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        assert_eq!(store.sync_backend_events(&account, &adapter)?, 4);
        assert!(store.pending_sync_events(100)?.is_empty());
        let requests = server.join().expect("mock server thread panicked")?;
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].url.path(), "/memos-server/api/v1/spaces");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].url.path(), "/memos-server/api/v1/memos");
        assert_eq!(requests[2].method, "PATCH");
        assert_eq!(
            requests[2].body.as_ref().unwrap()["content"],
            "Laatste offline wijziging"
        );
        assert_eq!(requests[3].method, "PATCH");
        assert_eq!(requests[3].body.as_ref().unwrap()["state"], "ARCHIVED");
        let note = store.note(&note_id)?.unwrap();
        assert_eq!(note.content, "Laatste offline wijziging");
        assert!(note.archived);
        Ok(())
    }

    #[test]
    fn failed_api_request_does_not_confirm_event() -> Result<()> {
        let (url, server) = mock_server(vec![503])?;
        let adapter = MemosAdapter::new(&url, "test-token")?;
        let error = adapter
            .apply_event(&SyncEvent::NoteCreated {
                note: note_snapshot(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("503"));
        assert!(!crate::memos::is_connection_error(&error));
        assert_eq!(
            server.join().expect("mock server thread panicked")?.len(),
            1
        );
        Ok(())
    }
}

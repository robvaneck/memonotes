use crate::i18n::tr;
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use serde_json::{Value, json};

use crate::memo::{Memo, MemoFolder, TimestampMillis};
use crate::memos::{MemosClient, RemoteMemo};
use crate::{NoteSnapshot, SyncEvent};

pub trait BackendAdapter: Send {
    fn check_connection(&self) -> Result<()>;
    fn list_memos(&self) -> Result<Vec<Memo>>;
    fn list_folders(&self) -> Result<Vec<MemoFolder>>;
    fn apply_event(&self, event: &SyncEvent) -> Result<()>;
}

struct MemosMutation {
    method: Method,
    resource: String,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
}

impl MemosMutation {
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
        "content": note.content,
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

    pub fn uses_plain_http(&self) -> bool {
        self.client.uses_plain_http()
    }

    pub fn get_memo(&self, name: &str) -> Result<Memo> {
        let id = resource_id(name, "memos")?;
        memo_from_remote(self.client.get_memo(id)?)
    }

    pub fn get_folder(&self, name: &str) -> Result<MemoFolder> {
        let id = resource_id(name, "spaces")?;
        let space = self.client.get_space(id)?;
        Ok(MemoFolder {
            id: space.name,
            name: space.title,
        })
    }

    pub fn list_folder_memos(&self, name: &str) -> Result<Vec<Memo>> {
        let id = resource_id(name, "spaces")?;
        self.client
            .list_space_memos(id)?
            .into_iter()
            .map(memo_from_remote)
            .collect()
    }
}

impl BackendAdapter for MemosAdapter {
    fn apply_event(&self, event: &SyncEvent) -> Result<()> {
        let mutation = MemosMutation::from_event(event)?;
        self.client.send_mutation(
            mutation.method,
            &mutation.resource,
            &mutation.query,
            mutation.body.as_ref(),
        )
    }

    fn check_connection(&self) -> Result<()> {
        self.client.check_connection()
    }

    fn list_memos(&self) -> Result<Vec<Memo>> {
        self.client
            .list_memos()?
            .into_iter()
            .map(memo_from_remote)
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
            })
            .collect())
    }
}

fn memo_from_remote(remote: RemoteMemo) -> Result<Memo> {
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
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server =
            thread::spawn(move || -> Result<Vec<CapturedRequest>> {
                let mut requests = Vec::new();
                for status in statuses {
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
                    stream.write_all(format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ).as_bytes())?;
                }
                Ok(requests)
            });
        Ok((format!("http://{address}/memos-server"), server))
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
        let memo = memo_from_remote(remote_memo(Some("2026-10-10T12:00:00Z")))?;
        assert_eq!(memo.updated_at, Some(1791633600000));

        let offset_memo = memo_from_remote(remote_memo(Some("2026-10-10T14:00:00+02:00")))?;
        assert_eq!(offset_memo.updated_at, Some(1791633600000));
        assert!(memo_from_remote(remote_memo(Some("not a timestamp"))).is_err());
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
        };
        let memo = Memo {
            id: "memos/remote-memo".to_owned(),
            content: "Origineel".to_owned(),
            title: None,
            folder_id: Some(folder.id.clone()),
            pinned: false,
            archived: false,
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

use crate::i18n::tr;
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTACHMENT_BYTES: usize = 30 * 1024 * 1024;

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4usize {
            if index <= chunk.len() {
                output.push(char::from(
                    ALPHABET[(value >> (18 - 6 * index as u32) & 63) as usize],
                ));
            } else {
                output.push('=');
            }
        }
    }
    output
}

pub fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout())
    })
}

pub struct MemosClient {
    client: Client,
    api_base: Url,
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMemo {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub update_time: Option<String>,
    #[serde(default)]
    pub property: Option<MemoProperty>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoProperty {
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteAttachment {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSpace {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub current_user_role: String,
    #[serde(default)]
    pub member_count: u32,
}

impl MemosClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let mut api_base = Url::parse(base_url.trim_end_matches('/'))
            .context(tr("api.memos.error.invalid_server_url"))?;
        ensure!(
            matches!(api_base.scheme(), "https" | "http"),
            tr("api.memos.error.unsupported_url_scheme")
        );
        ensure!(
            api_base.host_str().is_some(),
            tr("api.memos.error.server_host_missing")
        );
        ensure!(
            api_base.username().is_empty()
                && api_base.password().is_none()
                && api_base.query().is_none()
                && api_base.fragment().is_none(),
            tr("api.memos.error.server_url_contains_credentials_query_or_fragment")
        );
        ensure!(!token.trim().is_empty(), tr("api.memos.error.empty_token"));

        let base_path = api_base.path().trim_end_matches('/');
        api_base.set_path(&format!("{base_path}/api/v1"));

        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .context(tr("api.memos.error.http_client_initialization_failed"))?;

        Ok(Self {
            client,
            api_base,
            token: token.to_owned(),
        })
    }

    pub fn from_env() -> Result<Self> {
        let base_url = env::var("MEMONOTES_MEMOS_URL")
            .context(tr("api.memos.error.server_url_environment_missing"))?;
        let token = env::var("MEMONOTES_MEMOS_TOKEN")
            .context(tr("api.memos.error.token_environment_missing"))?;
        let client = Self::new(&base_url, &token)?;
        ensure!(
            !client.uses_plain_http()
                || env::var("MEMONOTES_ALLOW_INSECURE_HTTP").ok().as_deref() == Some("1"),
            tr("api.memos.error.insecure_http_environment_confirmation_required")
        );
        Ok(client)
    }

    pub fn check_connection(&self) -> Result<()> {
        let user = self.get_json(self.endpoint("auth/me")?)?;
        ensure!(user.is_object(), tr("api.memos.error.user_object_missing"));
        Ok(())
    }

    pub fn list_memos(&self) -> Result<Vec<RemoteMemo>> {
        self.list_pages("memos", "memos")
    }

    pub fn get_memo(&self, id: &str) -> Result<RemoteMemo> {
        serde_json::from_value(self.get_json(self.endpoint(&format!("memos/{id}"))?)?)
            .context(tr("api.memos.error.invalid_note_response"))
    }

    pub fn get_memo_if_exists(&self, id: &str) -> Result<Option<RemoteMemo>> {
        let response = self
            .client
            .get(self.endpoint(&format!("memos/{id}"))?)
            .bearer_auth(&self.token)
            .send()
            .context(tr("api.memos.error.server_unreachable"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!(crate::tr_format!(
                "api.memos.error.request_http_status",
                status = status
            ));
        }
        Ok(Some(
            response
                .json()
                .context(tr("api.memos.error.invalid_note_response"))?,
        ))
    }

    pub fn list_spaces(&self) -> Result<Vec<RemoteSpace>> {
        self.list_pages("spaces", "spaces")
    }

    pub fn get_space(&self, id: &str) -> Result<RemoteSpace> {
        serde_json::from_value(self.get_json(self.endpoint(&format!("spaces/{id}"))?)?)
            .context(tr("api.memos.error.invalid_space_response"))
    }

    pub fn list_space_memos(&self, id: &str) -> Result<Vec<RemoteMemo>> {
        let filter = format!(
            "space == {}",
            serde_json::to_string(&format!("spaces/{id}"))?
        );
        self.list_pages_with_query(
            "memos",
            "memos",
            &[("filter", filter), ("state", "NORMAL".to_owned())],
        )
    }

    pub fn attachment_exists(&self, id: &str) -> Result<bool> {
        let response = self
            .client
            .get(self.endpoint(&format!("attachments/{id}"))?)
            .bearer_auth(&self.token)
            .send()
            .context(tr("api.memos.error.server_unreachable"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        ensure!(
            status.is_success(),
            crate::tr_format!("api.memos.error.request_http_status", status = status)
        );
        Ok(true)
    }

    pub fn upload_attachment(
        &self,
        id: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_ATTACHMENT_BYTES,
            tr("api.memos.error.image_too_large")
        );
        let body = serde_json::json!({
            "filename": filename,
            "type": mime,
            "content": base64_encode(bytes),
        });
        self.send_mutation_with_timeout(
            Method::POST,
            "attachments",
            &[("attachmentId", id.to_owned())],
            Some(&body),
            ATTACHMENT_TIMEOUT,
        )
        .map(|_| ())
    }

    // `filename` may be empty (`/file/attachments/<id>`) and is used verbatim otherwise.
    pub fn download_attachment(&self, id: &str, filename: &str) -> Result<Vec<u8>> {
        let mut endpoint = self.api_base.clone();
        let base_path = self.api_base.path().trim_end_matches("/api/v1");
        let suffix = if filename.is_empty() {
            String::new()
        } else {
            format!("/{filename}")
        };
        endpoint.set_path(&format!("{base_path}/file/attachments/{id}{suffix}"));
        let response = self
            .client
            .get(endpoint)
            .bearer_auth(&self.token)
            .timeout(ATTACHMENT_TIMEOUT)
            .send()
            .context(tr("api.memos.error.server_unreachable"))?;
        let status = response.status();
        ensure!(
            status.is_success(),
            crate::tr_format!("api.memos.error.request_http_status", status = status)
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_ATTACHMENT_BYTES as u64),
            tr("api.memos.error.image_too_large")
        );
        let bytes = response
            .bytes()
            .context(tr("api.memos.error.server_unreachable"))?;
        ensure!(
            bytes.len() <= MAX_ATTACHMENT_BYTES,
            tr("api.memos.error.image_too_large")
        );
        Ok(bytes.to_vec())
    }

    // Percent-encoded filename, usable as a URL path segment.
    pub fn attachment_filename(&self, id: &str) -> Result<Option<String>> {
        let attachment = self.get_json(self.endpoint(&format!("attachments/{id}"))?)?;
        Ok(attachment
            .get("filename")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| {
                name.bytes()
                    .map(|byte| match byte {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'~' | b'-' => {
                            char::from(byte).to_string()
                        }
                        _ => format!("%{byte:02X}"),
                    })
                    .collect()
            }))
    }

    pub fn memo_attachment_names(&self, memo_id: &str) -> Result<Vec<String>> {
        let attachments: Vec<RemoteAttachment> =
            self.list_pages(&format!("memos/{memo_id}/attachments"), "attachments")?;
        Ok(attachments.into_iter().map(|item| item.name).collect())
    }

    pub fn uses_plain_http(&self) -> bool {
        self.api_base.scheme() == "http"
    }

    // Server base URL without trailing slash, as used in absolute attachment links.
    pub fn origin(&self) -> String {
        let mut base = self.api_base.clone();
        let path = base.path().trim_end_matches("/api/v1").to_owned();
        base.set_path(&path);
        base.as_str().trim_end_matches('/').to_owned()
    }

    pub(crate) fn send_mutation(
        &self,
        method: Method,
        resource: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<()> {
        self.send_mutation_json(method, resource, query, body)
            .map(|_| ())
    }

    // Returns the response body, or Null when it is empty or not JSON.
    pub(crate) fn send_mutation_json(
        &self,
        method: Method,
        resource: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        self.send_mutation_with_timeout(method, resource, query, body, REQUEST_TIMEOUT)
    }

    fn send_mutation_with_timeout(
        &self,
        method: Method,
        resource: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut request = self
            .client
            .request(method, self.endpoint(resource)?)
            .bearer_auth(&self.token)
            .timeout(timeout)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .context(tr("api.memos.error.server_unreachable"))?;
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .ok()
                .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                .and_then(|body| body.get("message")?.as_str().map(str::to_owned))
                .filter(|message| !message.is_empty());
            let error = crate::tr_format!("api.memos.error.mutation_http_status", status = status);
            bail!(match message {
                Some(message) =>
                    format!("{error}: {}", message.chars().take(300).collect::<String>()),
                None => error,
            });
        }
        Ok(response
            .text()
            .ok()
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or(Value::Null))
    }

    fn endpoint(&self, resource: &str) -> Result<Url> {
        let mut endpoint = self.api_base.clone();
        endpoint.set_path(&format!("{}/{resource}", self.api_base.path()));
        Ok(endpoint)
    }

    fn get_json(&self, endpoint: Url) -> Result<Value> {
        let response = self
            .client
            .get(endpoint)
            .bearer_auth(&self.token)
            .send()
            .context(tr("api.memos.error.server_unreachable"))?;
        let status = response.status();
        if !status.is_success() {
            bail!(crate::tr_format!(
                "api.memos.error.request_http_status",
                status = status
            ));
        }
        response
            .json()
            .context(tr("api.memos.error.invalid_json_response"))
    }

    fn list_pages<T: for<'de> Deserialize<'de>>(
        &self,
        resource: &str,
        collection: &str,
    ) -> Result<Vec<T>> {
        self.list_pages_with_query(resource, collection, &[])
    }

    fn list_pages_with_query<T: for<'de> Deserialize<'de>>(
        &self,
        resource: &str,
        collection: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();

        loop {
            let mut endpoint = self.endpoint(resource)?;
            {
                let mut pairs = endpoint.query_pairs_mut();
                pairs.extend_pairs(query.iter().map(|(name, value)| (*name, value.as_str())));
                pairs.append_pair("pageSize", "1000");
                if let Some(token) = &page_token {
                    pairs.append_pair("pageToken", token);
                }
            }

            let page = self.get_json(endpoint)?;
            let values = page
                .get(collection)
                .and_then(Value::as_array)
                .with_context(|| {
                    crate::tr_format!(
                        "api.memos.error.collection_missing",
                        collection = collection
                    )
                })?;
            for value in values {
                items.push(serde_json::from_value(value.clone()).with_context(|| {
                    crate::tr_format!(
                        "api.memos.error.invalid_collection_entry",
                        collection = collection
                    )
                })?);
            }

            page_token = page
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned);
            let Some(token) = &page_token else {
                break;
            };
            ensure!(
                seen_tokens.insert(token.clone()),
                tr("api.memos.error.repeated_page_token")
            );
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn connection_errors_are_distinguished_from_api_errors() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let client = MemosClient::new(&format!("http://{address}"), "probe-token")?;
        let error = client.check_connection().unwrap_err();
        assert!(is_connection_error(
            &error.context("Achtergrondophalen mislukt")
        ));
        assert!(!is_connection_error(&anyhow::anyhow!(
            "Memos API gaf HTTP 401"
        )));
        Ok(())
    }

    #[test]
    fn connection_check_calls_auth_me_with_bearer_token() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<String> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut request = String::new();
            reader.read_line(&mut request)?;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header)?;
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if header.to_ascii_lowercase().starts_with("authorization:") {
                    request.push_str(&header);
                }
            }
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )?;
            Ok(request)
        });

        let client = MemosClient::new(&format!("http://{address}"), "probe-token")?;
        assert!(client.uses_plain_http());
        client.check_connection()?;
        let request = server.join().expect("mock server thread panicked")?;
        assert!(request.starts_with("GET /api/v1/auth/me HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer probe-token")
        );
        Ok(())
    }

    #[test]
    fn get_memo_requests_only_the_selected_resource() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<String> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut request = String::new();
            reader.read_line(&mut request)?;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header)?;
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if header.to_ascii_lowercase().starts_with("authorization:") {
                    request.push_str(&header);
                }
            }
            let body = r#"{"name":"memos/note-id","content":"Remote","state":"NORMAL"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )?;
            Ok(request)
        });

        let client = MemosClient::new(&format!("http://{address}"), "probe-token")?;
        let memo = client.get_memo("note-id")?;
        let request = server.join().expect("mock server thread panicked")?;
        assert!(request.starts_with("GET /api/v1/memos/note-id HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer probe-token")
        );
        assert_eq!(memo.content, "Remote");
        Ok(())
    }

    #[test]
    fn opening_folder_gets_space_and_its_paginated_memos() -> Result<()> {
        use crate::adapters::MemosAdapter;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<Vec<String>> {
            let responses = [
                r#"{"name":"spaces/work","title":"Nieuwe mapnaam","memberCount":1}"#,
                r#"{"name":"spaces/work","title":"Nieuwe mapnaam","memberCount":1}"#,
                r#"{"memos":[{"name":"memos/one","content":"Nieuwste inhoud","state":"NORMAL","space":"spaces/work"}],"nextPageToken":"next-page"}"#,
                r#"{"memos":[{"name":"memos/two","content":"Tweede notitie","state":"NORMAL","space":"spaces/work"}]}"#,
            ];
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut request = String::new();
                reader.read_line(&mut request)?;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header)?;
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if header.to_ascii_lowercase().starts_with("authorization:") {
                        request.push_str(&header);
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )?;
                requests.push(request);
            }
            Ok(requests)
        });
        let adapter = MemosAdapter::new(&format!("http://{address}/server"), "probe-token")?;
        let folder = adapter.get_folder("spaces/work")?;
        let memos = adapter.list_folder_memos("spaces/work")?;
        let requests = server.join().expect("mock server thread panicked")?;
        assert!(requests[0].starts_with("GET /server/api/v1/spaces/work HTTP/1.1"));
        assert!(requests[1].starts_with("GET /server/api/v1/spaces/work HTTP/1.1"));
        assert_eq!(folder.id, "spaces/work");
        assert_eq!(folder.name, "Nieuwe mapnaam");
        assert_eq!(memos.len(), 2);
        assert_eq!(memos[0].content, "Nieuwste inhoud");
        for (index, request) in requests.iter().enumerate() {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer probe-token")
            );
            if index < 2 {
                continue;
            }
            let path = request.split_whitespace().nth(1).unwrap();
            let endpoint = Url::parse(&format!("http://{address}{path}"))?;
            assert_eq!(endpoint.path(), "/server/api/v1/memos");
            let query: std::collections::HashMap<_, _> = endpoint.query_pairs().collect();
            assert_eq!(
                query.get("filter").map(|value| value.as_ref()),
                Some("space == \"spaces/work\"")
            );
            assert_eq!(
                query.get("state").map(|value| value.as_ref()),
                Some("NORMAL")
            );
            assert_eq!(
                query.get("pageSize").map(|value| value.as_ref()),
                Some("1000")
            );
            assert_eq!(
                query.get("pageToken").map(|value| value.as_ref()),
                if index == 3 { Some("next-page") } else { None }
            );
        }
        Ok(())
    }

    #[test]
    fn get_memo_if_exists_only_treats_not_found_as_absence() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<Vec<String>> {
            let mut requests = Vec::new();
            for status in [404, 503] {
                let (mut stream, _) = listener.accept()?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut request = String::new();
                reader.read_line(&mut request)?;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header)?;
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                }
                requests.push(request);
                stream.write_all(
                    format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )?;
            }
            Ok(requests)
        });
        let client = MemosClient::new(&format!("http://{address}"), "probe-token")?;

        assert!(client.get_memo_if_exists("note-id")?.is_none());
        assert!(client.get_memo_if_exists("note-id").is_err());

        let requests = server.join().expect("mock server thread panicked")?;
        assert!(
            requests
                .iter()
                .all(|request| { request.starts_with("GET /api/v1/memos/note-id HTTP/1.1") })
        );
        Ok(())
    }

    #[test]
    fn list_memos_and_spaces_follow_pagination_and_map_fields() -> Result<()> {
        use crate::adapters::{BackendAdapter, MemosAdapter};

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<Vec<String>> {
            let responses = [
                r#"{"spaces":[{"name":"spaces/work","title":"Work","currentUserRole":"ADMIN","memberCount":1},{"name":"spaces/team","title":"Team","currentUserRole":"USER","memberCount":3}]}"#,
                r##"{"memos":[{"name":"memos/one","content":"# Title","state":"NORMAL","visibility":"PRIVATE","space":"spaces/work","property":{"title":"Title"}},{"name":"memos/shared-space","content":"Shared space note","state":"NORMAL","visibility":"PRIVATE","space":"spaces/team"},{"name":"memos/protected","content":"Protected note","state":"NORMAL","visibility":"PROTECTED"}],"nextPageToken":"second-page"}"##,
                r#"{"memos":[{"name":"memos/two","content":"Another","state":"ARCHIVED","visibility":"PRIVATE","pinned":true,"updateTime":"2026-10-10T12:00:00Z"}]}"#,
                r#"{"spaces":[{"name":"spaces/work","title":"Work","currentUserRole":"ADMIN","memberCount":1},{"name":"spaces/team","title":"Team","currentUserRole":"USER","memberCount":3}]}"#,
            ];
            let mut requests = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept()?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut request = String::new();
                reader.read_line(&mut request)?;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header)?;
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if header.to_ascii_lowercase().starts_with("authorization:") {
                        request.push_str(&header);
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes())?;
                requests.push(request);
            }
            Ok(requests)
        });

        let adapter = MemosAdapter::new(&format!("http://{address}"), "probe-token")?;
        let backend: &dyn BackendAdapter = &adapter;
        let memos = backend.list_memos()?;
        let folders = backend.list_folders()?;
        let requests = server.join().expect("mock server thread panicked")?;

        assert_eq!(memos.len(), 4);
        assert_eq!(memos[0].id, "memos/one");
        assert_eq!(memos[0].content, "# Title");
        assert_eq!(memos[0].title.as_deref(), Some("Title"));
        assert_eq!(memos[0].folder_id.as_deref(), Some("spaces/work"));
        assert!(!memos[0].archived);
        assert!(!memos[0].is_shared);
        assert!(memos[1].is_shared);
        assert!(memos[2].is_shared);
        assert!(!memos[1].archived);
        assert!(memos[3].archived);
        assert!(memos[3].pinned);
        assert_eq!(memos[3].title.as_deref(), Some("Another"));
        assert_eq!(memos[3].folder_id, None);
        assert_eq!(memos[3].updated_at, Some(1791633600000));
        assert_eq!(folders[0].id, "spaces/work");
        assert_eq!(folders[0].name, "Work");
        assert!(!folders[0].is_shared);
        assert!(folders[1].is_shared);
        let store = crate::Store::open(":memory:")?;
        store.add_backend_account(
            "mock-account",
            "Memos",
            "memos",
            &format!("http://{address}"),
        )?;
        store.import_backend_snapshot("mock-account", &folders, &memos)?;
        let active = store.notes(&crate::NoteFilter {
            account_id: Some("mock-account".to_owned()),
            ..Default::default()
        })?;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "# Title");
        assert_eq!(active[0].title, "Title");
        assert_eq!(
            active[0].folder_id.as_deref(),
            Some(store.folders()?[0].id.as_str())
        );
        let archived = store.notes(&crate::NoteFilter {
            account_id: Some("mock-account".to_owned()),
            archived: true,
            ..Default::default()
        })?;
        assert_eq!(archived.len(), 1);
        assert!(store.pending_sync_events(100)?.is_empty());
        assert!(requests[0].starts_with("GET /api/v1/spaces?pageSize=1000 HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/memos?pageSize=1000 HTTP/1.1"));
        assert!(
            requests[2]
                .starts_with("GET /api/v1/memos?pageSize=1000&pageToken=second-page HTTP/1.1")
        );
        assert!(requests[3].starts_with("GET /api/v1/spaces?pageSize=1000 HTTP/1.1"));
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer probe-token")
        }));
        Ok(())
    }

    #[test]
    fn base64_encodes_with_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_credentials_embedded_in_server_url() {
        assert!(MemosClient::new("https://user:secret@example.com", "token").is_err());
    }
}

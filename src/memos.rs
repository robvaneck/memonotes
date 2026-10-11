use anyhow::{bail, ensure, Context, Result};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::time::Duration;

pub fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>()
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
#[serde(rename_all = "camelCase")]
pub struct RemoteSpace {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub current_user_role: String,
}

impl MemosClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let mut api_base =
            Url::parse(base_url.trim_end_matches('/')).context("Ongeldige Memos-server-URL")?;
        ensure!(
            matches!(api_base.scheme(), "https" | "http"),
            "De Memos-server-URL moet https:// of http:// gebruiken"
        );
        ensure!(
            api_base.host_str().is_some(),
            "De Memos-server-URL mist een host"
        );
        ensure!(
            api_base.username().is_empty()
                && api_base.password().is_none()
                && api_base.query().is_none()
                && api_base.fragment().is_none(),
            "Gebruik geen inloggegevens, query of fragment in de Memos-server-URL"
        );
        ensure!(!token.trim().is_empty(), "De Memos PAT mag niet leeg zijn");

        let base_path = api_base.path().trim_end_matches('/');
        api_base.set_path(&format!("{base_path}/api/v1"));

        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(Policy::none())
            .build()
            .context("HTTP-client voor Memos kon niet worden gestart")?;

        Ok(Self {
            client,
            api_base,
            token: token.to_owned(),
        })
    }

    pub fn from_env() -> Result<Self> {
        let base_url = env::var("MEMONOTES_MEMOS_URL")
            .context("Stel MEMONOTES_MEMOS_URL in, bijvoorbeeld https://memos.example.com")?;
        let token = env::var("MEMONOTES_MEMOS_TOKEN")
            .context("Stel MEMONOTES_MEMOS_TOKEN in met een Memos Personal Access Token")?;
        let client = Self::new(&base_url, &token)?;
        ensure!(
            !client.uses_plain_http()
                || env::var("MEMONOTES_ALLOW_INSECURE_HTTP").ok().as_deref() == Some("1"),
            "HTTP is onversleuteld; stel MEMONOTES_ALLOW_INSECURE_HTTP=1 expliciet in om door te gaan"
        );
        Ok(client)
    }

    pub fn check_connection(&self) -> Result<()> {
        let user = self.get_json(self.endpoint("auth/me")?)?;
        ensure!(
            user.is_object(),
            "Memos API gaf geen gebruikersobject terug"
        );
        Ok(())
    }

    pub fn list_memos(&self) -> Result<Vec<RemoteMemo>> {
        self.list_pages("memos", "memos")
    }

    pub fn get_memo(&self, id: &str) -> Result<RemoteMemo> {
        serde_json::from_value(self.get_json(self.endpoint(&format!("memos/{id}"))?)?)
            .context("Memos API gaf een ongeldige notitie terug")
    }

    pub fn list_spaces(&self) -> Result<Vec<RemoteSpace>> {
        self.list_pages("spaces", "spaces")
    }

    pub fn get_space(&self, id: &str) -> Result<RemoteSpace> {
        serde_json::from_value(self.get_json(self.endpoint(&format!("spaces/{id}"))?)?)
            .context("Memos API gaf een ongeldige space terug")
    }

    pub fn list_space_memos(&self, id: &str) -> Result<Vec<RemoteMemo>> {
        let filter = format!("space == {}", serde_json::to_string(&format!("spaces/{id}"))?);
        self.list_pages_with_query(
            "memos",
            "memos",
            &[("filter", filter), ("state", "NORMAL".to_owned())],
        )
    }

    pub fn uses_plain_http(&self) -> bool {
        self.api_base.scheme() == "http"
    }

    pub(crate) fn send_mutation(
        &self,
        method: Method,
        resource: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<()> {
        let mut request = self
            .client
            .request(method, self.endpoint(resource)?)
            .bearer_auth(&self.token)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().context("Memos-server niet bereikbaar")?;
        let status = response.status();
        ensure!(status.is_success(), "Memos API gaf HTTP {status}; event niet bevestigd");
        Ok(())
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
            .context("Memos-server niet bereikbaar")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Memos API gaf HTTP {status}; controleer serveradres en PAT");
        }
        response
            .json()
            .context("Memos API gaf geen geldige JSON terug")
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
                .with_context(|| format!("Memos API-response mist de lijst '{collection}'"))?;
            for value in values {
                items.push(serde_json::from_value(value.clone()).with_context(|| {
                    format!("Memos API gaf een ongeldige entry in '{collection}' terug")
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
                "Memos API herhaalde een paginatoken; paginering afgebroken"
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
        assert!(is_connection_error(&error.context("Achtergrondophalen mislukt")));
        assert!(!is_connection_error(&anyhow::anyhow!("Memos API gaf HTTP 401")));
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
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer probe-token"));
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
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer probe-token"));
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
                r#"{"name":"spaces/work","title":"Nieuwe mapnaam"}"#,
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
                write!(stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body)?;
                requests.push(request);
            }
            Ok(requests)
        });
        let adapter = MemosAdapter::new(&format!("http://{address}/server"), "probe-token")?;
        let folder = adapter.get_folder("spaces/work")?;
        let memos = adapter.list_folder_memos("spaces/work")?;
        let requests = server.join().expect("mock server thread panicked")?;
        assert!(requests[0].starts_with("GET /server/api/v1/spaces/work HTTP/1.1"));
        assert_eq!(folder.id, "spaces/work");
        assert_eq!(folder.name, "Nieuwe mapnaam");
        assert_eq!(memos.len(), 2);
        assert_eq!(memos[0].content, "Nieuwste inhoud");
        for (index, request) in requests.iter().enumerate() {
            assert!(request.to_ascii_lowercase().contains("authorization: bearer probe-token"));
            if index == 0 {
                continue;
            }
            let path = request.split_whitespace().nth(1).unwrap();
            let endpoint = Url::parse(&format!("http://{address}{path}"))?;
            assert_eq!(endpoint.path(), "/server/api/v1/memos");
            let query: std::collections::HashMap<_, _> = endpoint.query_pairs().collect();
            assert_eq!(query.get("filter").map(|value| value.as_ref()), Some("space == \"spaces/work\""));
            assert_eq!(query.get("state").map(|value| value.as_ref()), Some("NORMAL"));
            assert_eq!(query.get("pageSize").map(|value| value.as_ref()), Some("1000"));
            assert_eq!(query.get("pageToken").map(|value| value.as_ref()),
                if index == 2 { Some("next-page") } else { None });
        }
        Ok(())
    }

    #[test]
    fn list_memos_and_spaces_follow_pagination_and_map_fields() -> Result<()> {
        use crate::adapters::{BackendAdapter, MemosAdapter};

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<Vec<String>> {
            let responses = [
                r##"{"memos":[{"name":"memos/one","content":"# Title","state":"NORMAL","visibility":"PRIVATE","space":"spaces/work","property":{"title":"Title"}}],"nextPageToken":"second-page"}"##,
                r#"{"memos":[{"name":"memos/two","content":"Another","state":"ARCHIVED","visibility":"PRIVATE","pinned":true,"updateTime":"2026-10-10T12:00:00Z"}]}"#,
                r#"{"spaces":[{"name":"spaces/work","title":"Work","currentUserRole":"ADMIN"}]}"#,
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

        assert_eq!(memos.len(), 2);
        assert_eq!(memos[0].id, "memos/one");
        assert_eq!(memos[0].content, "# Title");
        assert_eq!(memos[0].title.as_deref(), Some("Title"));
        assert_eq!(memos[0].folder_id.as_deref(), Some("spaces/work"));
        assert!(!memos[0].archived);
        assert!(memos[1].archived);
        assert!(memos[1].pinned);
        assert_eq!(memos[1].title.as_deref(), Some("Another"));
        assert_eq!(memos[1].folder_id, None);
        assert_eq!(memos[1].updated_at, Some(1791633600000));
        assert_eq!(folders[0].id, "spaces/work");
        assert_eq!(folders[0].name, "Work");
        let store = crate::Store::open(":memory:")?;
        store.add_backend_account("mock-account", "Memos", "memos", &format!("http://{address}"))?;
        store.import_backend_snapshot("mock-account", &folders, &memos)?;
        let active = store.notes(&crate::NoteFilter {
            account_id: Some("mock-account".to_owned()), ..Default::default()
        })?;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "# Title");
        assert_eq!(active[0].title, "Title");
        assert_eq!(active[0].folder_id.as_deref(), Some(store.folders()?[0].id.as_str()));
        let archived = store.notes(&crate::NoteFilter {
            account_id: Some("mock-account".to_owned()), archived: true, ..Default::default()
        })?;
        assert_eq!(archived.len(), 1);
        assert!(store.pending_sync_events(100)?.is_empty());
        assert!(requests[0].starts_with("GET /api/v1/memos?pageSize=1000 HTTP/1.1"));
        assert!(requests[1]
            .starts_with("GET /api/v1/memos?pageSize=1000&pageToken=second-page HTTP/1.1"));
        assert!(requests[2].starts_with("GET /api/v1/spaces?pageSize=1000 HTTP/1.1"));
        assert!(requests.iter().all(|request| request
            .to_ascii_lowercase()
            .contains("authorization: bearer probe-token")));
        Ok(())
    }

    #[test]
    fn rejects_credentials_embedded_in_server_url() {
        assert!(MemosClient::new("https://user:secret@example.com", "token").is_err());
    }
}

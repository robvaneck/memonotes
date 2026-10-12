# Memonotes

Memonotes is an early Linux/GNOME desktop app for local and Memos-backed notes.
Status updated: 2026-10-12. The current GTK
window has a three-pane layout, folder creation/navigation, note search, pinning,
archive/restore, moving notes between folders, and pastel note colors backed by
SQLite. Note colors adapt to light and dark appearance. Memos accounts can be
added in Settings; account details are stored in SQLite and PATs in GNOME
Keyring. Multiple accounts are supported. Enabled Memos accounts fetch memos
and spaces in the background at startup, after saving an account, when an
account is enabled, and every 60 seconds while the main window is visible.
Selecting a backend note also fetches that memo when no local edits are pending.
Opening a Memos folder fetches its space with `GET /api/v1/spaces/{id}` and
all active memo pages with `GET /api/v1/memos`, filtered by that space.
The adapter refreshes the cached folder name and note contents without
overwriting pending local changes or deletions. Local-only folders make no
network requests. Results are cached in SQLite and displayed under the account; offline starts retain
cached notes. Background fetching and autosaves are silent. API failures appear
as libadwaita toasts; identical failures are not repeated until recovery.
Local edits are uploaded automatically in ordered batches through a durable
outbox. The scheduler runs once per second; text edits delay uploads for the
account until one second after the last edit. GNOME/GIO network detection
pauses automatic API calls while offline. Temporary failures retry for that
account with capped exponential backoff up to 15 minutes; authentication and
other API errors remain visible. When the network returns, synchronization resumes
automatically: full server state is fetched and compared before safe pending
outbox events are uploaded. A **Sync now** control also starts a refresh and
clears scheduled retry delays. Transient failures use capped exponential
backoff. There is no manual online/offline switch. Offline changes survive app
restarts.
The editor shows the complete note without a separate title field. The first
line uses H1 styling without displaying its Markdown heading marker.
Saving normalizes the first line as a Markdown H1 (`# Title`), so the UI and
Memos share one document.
The note list derives its virtual title from the first content line, not the
API's computed title property; the list preview omits that title line.
Imported content stays unchanged until edited. Existing
local notes with separate titles are combined on their next edit without
discarding the body.
The editor is currently plain text;
WYSIWYG formatting and images are not implemented yet. Basic two-way text
synchronization detects divergent local/server edits and keeps both versions
until the user chooses which to keep. Stable-ID creates are reconciled after
errors or HTTP 409 by fetching that ID and acknowledging only an exact match.
Automatic merging and live multi-client race validation remain open.

Notes can be opened in separate editable windows. Keyboard shortcuts include
new note/folder, search, close, quit, and undo for archive operations.
Archived notes can be permanently deleted using the trash button or Delete key
after confirmation. Permanent deletion cannot be undone. Memos-backed notes
queue a `NoteDeleted` adapter event that deletes the remote memo; offline or
failed requests remain queued until synchronization succeeds.
Deleting a folder offers three choices: cancel, delete the folder while keeping
its notes, or delete the folder and all its notes. Kept notes are moved out of
the Memos space before its delete event is queued; when deleting notes too,
their delete events are queued first. This prevents the remote space deletion
from unexpectedly deleting notes the user chose to keep.
The note list supports multiple selection and dragging notes into folders.
It uses GTK's native virtualized `ListView`: only visible rows and an offscreen
buffer have widgets, and sidebar resizing keeps text at its normal font size.
See [PLAN.md](PLAN.md) for the current implementation checklist and release gaps.

## Translations

All application labels, tooltips, dialogs and application-generated errors are
translated into English, German, French, Dutch and Spanish. The external
`translations.json` catalog uses stable, descriptive English keys, with `en`,
`de`, `fr`, `nl` and `es` values. Colors use `color_mint`, `color_iris`,
`color_teal`, etc. Other keys use dotted namespaces with snake_case names,
such as `note.move_to_folder`, `account.field.server_url` and
`api.memos.error.invalid_server_url`. Use `tr("color_mint")` for fixed text
and `tr_format!("api.memos.error.request_http_status", status = status)` for
messages with named placeholders. Keep placeholders identical to the English
reference translation. Stored color IDs such as `mint` do not change.
Translations are embedded at build time; rebuild after changes.
The development watcher also watches the catalog.

Choose **Settings > General > Language** to select English, German, French,
Dutch or Spanish, or **System default**. The choice is stored locally and takes
effect after restarting the app; open notes and ongoing synchronization are
not interrupted. Language names are displayed in their own language.

With **System default**, the app follows the system message locale (`LC_ALL`, `LC_MESSAGES`, then `LANG`),
including `LANGUAGE` preferences unless the message locale is `C` or `POSIX`.
Unsupported languages fall back to English. With **System default** selected,
override the application language at startup, for example:

```sh
MEMONOTES_LANGUAGE=nl npm start
MEMONOTES_LANGUAGE=de npm run dev
```

Use `en`, `de`, `fr`, `nl` or `es`. Existing note content, account names and
folder names are never translated. Messages supplied by operating-system or
third-party libraries retain their own localization.

## Backend architecture

Backend calls go through `BackendAdapter`, implemented by `MemosAdapter`.
The adapter returns backend-independent `Memo` and `MemoFolder` models;
Memos spaces map to folders. HTTP and API response types stay in `MemosClient`.
Background fetching, outbox uploads, and the API probe use this adapter. Cache import
is transactional and does not enqueue outgoing events. Partial imports never
infer deletion from a missing item. Full account refreshes directly GET missing
previously synced memo IDs and reconcile only confirmed 404s; pending changes
become conflicts instead of being discarded. Schema version 11 stores
per-account remote-resource links, sync baselines and durable conflicts so the
same remote name on two servers cannot mix notes or folders. The adapter also
accepts existing `SyncEvent` values through `apply_event`:

| Event | Memos 0.31.0 API |
| --- | --- |
| `FolderCreated` | `POST /api/v1/spaces?spaceId={id}` |
| `FolderUpdated` | `PATCH /api/v1/spaces/{id}`, mask `title` |
| `FolderDeleted` | `DELETE /api/v1/spaces/{id}`, after queued note moves/deletions |
| `NoteCreated`, `NoteRestored` | `POST /api/v1/memos?memoId={id}`, visibility `PRIVATE` |
| `NoteUpdated` | `PATCH /api/v1/memos/{id}`, mask `content,pinned,state,space` |
| `NoteArchived`, `NoteUnarchived` | `PATCH /api/v1/memos/{id}`, mask `state` |
| `NoteDeleted` | `DELETE /api/v1/memos/{id}` |

Local UUIDs are stable create IDs; resource names such as `memos/{id}` and
`spaces/{id}` are also accepted. Updates do not change visibility. Moving a
note out of a folder sends `space: null` with the `space` field mask. The
saved Markdown content is sent unchanged; the display title and timestamps
are not sent as computed Memos properties. The editor stores its title as
the first H1 in that content, from which Memos derives its title.

Local-only snapshots are rejected by the adapter and are not uploaded by the
worker. The worker routes events by account, translates local IDs through the
stored resource links, coalesces consecutive updates for the same note, and
acknowledges only successfully sent batches. Failed requests leave events in
the outbox. After a create error, the adapter fetches the stable ID and only
acknowledges it when the existing note matches the requested content and state.
`NoteRestored` recreates the snapshot, not deleted attachments, relations, or
shares. The Memos integration has been tested successfully against the target
server. Attachment behavior and concurrent-update handling still need separate
live validation.

### Remaining API and sync gaps

- Attachment upload/download/linking/deletion and offline image caching are missing.
- Visibility is not retained in the local note model or editable; updates leave
	server visibility unchanged. Space roles are not mapped into the folder model.
- Space membership/invitations and API-based PAT management are not implemented
	and are outside the current version 1 scope.
- Explicit server list filters/sorting and live multi-client conflict testing remain open.

## Fedora build prerequisites

Install the GTK development packages:

```sh
sudo dnf install gtk4-devel libadwaita-devel libsecret-devel pkgconf-pkg-config
```

Make sure Cargo and npm are on `PATH`. Install the file watcher once:

```sh
cargo install cargo-watch
```

The root `package.json` is a shortcut list for Cargo commands; it does not add a
JavaScript runtime to the app. Run `npm run` to see all available commands.
Start commands from the project directory:

```sh
cd ~/repos/memonotes
npm run dev
```

`npm run dev` opens the GTK app and rebuilds/restarts it when Rust source changes.
Changes are debounced for one second. The app only starts after a successful build.
After a compile error or app exit (including exit status 0), the watcher stays
active and retries on the next file change. A compile error still needs to be
fixed; it does not stop the watcher. Dev instances skip the shared D-Bus
application registration to avoid registration races during restarts.
Use `Ctrl+C` in the terminal to stop the watcher.

| Command | Action |
| --- | --- |
| `npm run dev` | Watch Rust files, rebuild, and restart the GTK app on changes |
| `npm run watch:check` | Re-run the GTK compile check on changes without launching the app |
| `npm start` | Launch the app once |
| `npm run build` | Build an optimized release binary |
| `npm run check` | Compile-check the GTK app |
| `npm test` | Run library tests, including storage, sync, and mock-server API tests |
| `npm run format` | Format Rust sources |

## Memos API probe

The API probe checks a Memos Personal Access Token against
`GET /api/v1/auth/me`, then reads the memos returned by the server and accessible spaces,
following pagination. It prints counts only; note contents are not printed or
saved by the probe. Provide the server URL and token through the environment:

```sh
export MEMONOTES_MEMOS_URL='https://memos.example.com'
read -rsp 'Memos PAT: ' MEMONOTES_MEMOS_TOKEN
printf '\n'
export MEMONOTES_MEMOS_TOKEN
cargo run --bin memos_api_probe
unset MEMONOTES_MEMOS_TOKEN
```

For a local HTTP server, also set `MEMONOTES_ALLOW_INSECURE_HTTP=1` explicitly.
HTTP does not encrypt the token in transit.

The watcher restarts the app after source changes. Notes are stored separately,
so they remain on disk across restarts. The database lives under
`$XDG_DATA_HOME/memonotes`, or `~/.local/share/memonotes` when `XDG_DATA_HOME`
is unset.
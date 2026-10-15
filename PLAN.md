# Memonotes - product and implementation plan

Original plan: 2026-10-05. Status updated: 2026-10-15.
Status: GTK desktop app with SQLite, multiple Memos accounts, background fetching
and automatic outbox uploads. Sync keeps base versions, detects conflicts,
reconciles confirmed remote deletions and recovers uncertain creates. The editor
supports WYSIWYG formatting and inline images that sync as Memos attachments;
concurrent edits from other clients surface as a version conflict that the user resolves.
Automatic merging is not built.
Project directory: /home/fedora/repos/memonotes.
The product agreements below describe the goal; the checklist shows what is
actually implemented in the current code. See also [README.md](README.md) and
[FEATURES.md](FEATURES.md).

## Current status and todos

### Done

- [x] Rust/GTK4/libadwaita app with three resizable panes and a collapsible sidebar.
- [x] SQLite storage, local autosave, folders, search, pinning and archive/restore.
- [x] Permanently delete archived notes with a confirmation dialog and `NoteDeleted` through the adapter to the Memos DELETE API; no undo for permanent deletion.
- [x] Undo for archive actions; undo is not persistent.
- [x] Multiple note selection and dragging to a folder.
- [x] Separate editable note windows and keyboard navigation/shortcuts.
- [x] 32 note colors with light/dark variants and an About window.
- [x] Manage multiple accounts, enable/disable them, test the connection and store the PAT in GNOME Keyring.
- [x] Memos adapter with backend-independent note and folder models; the store, sync code and UI no longer know Memos names: provider checks, adapter construction, resource naming, content limits and error classification go through `adapters.rs` (schema v13 stores a `kind` per remote link). Translation keys stay per adapter (`api.memos.*`, `api.imap.*`).
- [x] IMAP adapter (`src/imap_notes.rs`): notes are mail messages in the `Notes` mailbox in the Apple Notes layout (Gmail-compatible), folders are subfolders, images are attachments. Account format `[imaps://|imap://]user@host[:port]` plus password/app password. Tested against an in-memory fake and live against a real IMAP server (listing, single GET, uploads, edits and deletes confirmed on the remote). Gmail/Fastmail are not tried yet. Joplin is still only an option.
- [x] IMAP performance and compatibility: every mail in `Notes` is a note (mail without the Apple ID header gets a stable ID from UIDVALIDITY, UID and a subject/date hash); lookups no longer need server-side header search; parsed messages are cached per mailbox by UID so refreshes only fetch new mail; idle connections are reused with one retry when a reused one has timed out.
- [x] Editor and sync performance with large collections (2,600+ notes, notes up to 100 KB): linear Markdown serialization on long lines, change handlers skipped while a note loads, debounced note list refresh while typing, unchanged notes skipped in snapshot imports, cached pending-event check and no refresh loop while typing.
- [x] Paginated fetching of Memos notes/spaces and transactional caching per account (schema v12: remote-ID links, sync baselines and conflicts).
- [x] Fetch at startup, on saving/enabling an account and every 60 seconds while the main window is visible.
- [x] Fetch the selected backend note separately when no local note events are pending.
- [x] When opening a Memos folder, fetch the space via GET and its active notes with a space filter, paginated; pending local changes stay protected.
- [x] Durable outbox, account routing, remote-ID links and acknowledgement after successful upload.
- [x] Automatic uploads, a one-second debounce after text edits, merging of consecutive updates and capped exponential retry (30 seconds up to 15 minutes).
- [x] Automatic network detection without a manual mode: API calls pause offline; after reconnect, fetch and compare first, then upload safe outbox events.
- [x] API mapping for note CRUD/restore, pinning, archiving, moving and folder create/rename/delete.
- [x] Core Memos integration tested live against the target server and working.
- [x] Safe folder/space deletion with three dialog choices: cancel, delete the folder but keep its notes, or delete the folder and all its notes. Note moves/deletions are queued before `FolderDeleted`.
- [x] Title and body form one synchronized Markdown document. Legacy notes with a separate title are converted to a first H1 on edit without losing the body; unchanged content creates no new sync event.
- [x] The editor shows inline bold/italic/underline as formatting, H1-H6 headings and paragraph choice via the toolbar, clickable checklists, non-selectable bullet markers, GTK undo/redo and locally inserted inline images that are selectable, draggable and resizable from the corners (aspect ratio is kept); saving translates this back to Markdown. Image files live in app-private storage and use a local reference.
- [x] Three-way conflict detection keeps the base, local and remote versions; the user chooses local or server. Local deletes that collide with server changes can also be resolved.
- [x] Complete account refreshes confirm missing memos with a direct GET; only a 404 counts as a remote delete. Pending local changes are kept as a conflict.
- [x] Uncertain creates and HTTP 409 are reconciled through the stable memo ID; only an exactly matching remote resource is acknowledged.
- [x] Manual sync and capped exponential retry for temporary failures (30 seconds up to 15 minutes).
- [x] API error toasts with deduplication; background fetching/autosave without progress toasts.
- [x] Storage/sync tests and HTTP mock tests; API probe available.

### Still to do

- [x] Fetch all 2,700 notes paginated and sort them correctly; confirmed live.
- [x] Archive/restore works remotely; tested end to end on the real server.
- [x] Filter shared Memos spaces and non-private notes out of the UI; keep shared data only in the sync cache.
- [x] GTK editor undo/redo with focus-aware Ctrl+Z routing and updated button state.
- [x] Finish the WYSIWYG editor: inline formatting, headings/paragraphs, checklist, bullet and numbered lists (toolbar button and Ctrl+Shift+O), link, local image, table, horizontal rule (typing `---` and Enter) and undo/redo actions are wired up with roundtrip tests.
- [x] Memos attachments for inline images: upload linked to the memo in the same mutation (Memos 0.31 otherwise rejects with 400), download and cache locally, map images from the Memos web client (also without a file name) to local images, and unbind/delete attachments when the user removes the image from the note. Attachment ID = local UUID; content references `/file/attachments/<id>/<file>`; the width is stored in the alt text because Memos rejects URL fragments. Verified manually against the target server; bare anchor characters without an image are no longer saved to Markdown.
- [x] Insert an image offline and upload it after reconnect: confirmed manually, one attachment on the server.
- [x] Validate image sync further: upload retries after an interrupted connection without duplicate/orphaned attachments, server file size limits and non-image attachment types.
- [x] Import one or many Markdown files (including large batches) into a selected account and folder/space. Every import creates new notes; existing notes are never overwritten.
- [x] Export a selected account to a ZIP with one directory per folder/space and its Markdown files; include metadata and images.
- [x] Safe unlinking of an account: removing an account with unsent outbox changes shows a warning with the number of changes. Export is not offered in that dialog; use the account export first. A separate onboarding flow is not needed; the app starts locally and accounts are added in Settings.
- [ ] Narrow-window navigation, accessibility and visible sync/conflict status. Remembered pane widths are not needed.
- [x] Fedora/Flatpak packaging (`packaging/`, app ID `io.github.robvaneck.Memonotes`); network and GNOME Keyring tested in the sandbox. Screenshots and Flathub submission are still to do.
- [x] Apply Markdown formatting when opening MD files. Blockquotes and code blocks stay visible as raw Markdown characters (`>`, backticks); that is accepted for now.
- [x] Automatically continue lists (bullets, `-`/`*`/`+` and numbered) when pressing Enter.
- [x] Table support: Markdown pipe tables render as editable grid widgets (top-level tables only; tables inside lists or quotes stay text) at full editor width, with an insert-table toolbar button, row/column add and delete, drag selection of cells with copy, cut, clear and paste, pasting spreadsheet ranges (Excel, Calc, web tables) as a new table, and undo/redo for structure changes.
- [x] Bulk delete in the note list: Ctrl+A selects all notes, Delete archives the selection (or permanently deletes it, after confirmation, in the archive).
- [x] Export: write the note title into the Markdown content (as the first H1), not only into the file name.

Not built and outside the current version 1 scope: space members/invitations,
publishing, PAT management through the API, mobile and React Native. File
attachments other than images are not built.

## 1. Confirmed conclusions

- The app is called Memonotes and targets the Linux/GNOME experience, with Fedora as the first target platform.
- Desktop stack: Rust, GTK4 and libadwaita.
- Apple Notes is the visual reference: sidebar, note list and note detail/editor.
- No sub-folders: in Memos mode a space is exactly one folder containing notes.
- Personal organization is the goal; collaboration is not a goal of version 1.
- The main editor is WYSIWYG like Apple Notes, not a Markdown code editor.
- Checklists/todos with clickable checkboxes, bold, italic and images belong in version 1.
- Underline is written as a limited `<u>...</u>` extension; compatibility and rendering on the target server must be tested live.
- An optional Markdown source tab is allowed but not needed for normal use.
- Without a backend the app works fully locally.
- With a backend the app uses the existing Memos API on Umbrel.
- The user creates an app key in Memos and enters it in Memonotes.
- Compatibility baseline: Memos 0.31.0.
- In Memos mode, offline reading and editing must also work, with later synchronization.
- Mobile and React Native are outside this project plan; they do not drive architecture or planning now.

## 2. Design and navigation

Goal: the same recognizable layout and calm interaction as Apple Notes.
GTK/libadwaita does provide Fedora-native window controls, widgets and accessibility.
Pixel-identical macOS controls are not promised; use own branding and available
Linux fonts, and ship no Apple assets or Apple fonts.

Proposed wide desktop layout:

```text
| Sidebar            | Note list             | Detail / editor             |
|--------------------|-----------------------|-----------------------------|
| On this device     | Search                | Action bar                  |
| or Memos account   | Pinned notes          | Title                       |
| All notes          | Title, date, snippet  | Date and save status        |
| No folder          | Selected note         | Note content                |
| Folders / spaces   | Other notes           |                             |
| Archive            |                       |                             |
| Settings           |                       |                             |
```

- Resizable pane widths and remembered selection.
- On narrow windows, consecutive screens with back navigation instead of squashed columns.
- Actions: new note, new folder, move, pin, archive and search.
- Local autosave with a recognizable status: saved, waiting for sync, offline, conflict or error.
- Light/dark following the system setting; keyboard and screen reader support.
- WYSIWYG as the default: edit formatted content directly, without visible Markdown syntax.
- Checklists/todos with checkboxes that can be ticked directly; the state is stored and synchronized.
- Formatting actions for bold and italic, with recognizable controls and active selection state.
- Insert and display images inline; support file picking and pasting.
- Undo/redo, selection and normal keyboard operation belong to the editor prototype.
- New notes store the title as the first H1 for Memos compatibility; existing notes keep their content unchanged until the user edits them.
- Sample-less or empty notes get a local display title; do not rewrite existing content.

Markdown remains the exchange format with Memos, not the normal user interface.
The editor uses a proven rich-text engine; the concrete GTK integration is chosen in a
small prototype. GTK/libadwaita alone does not automatically provide this complete editor.
First demonstrate that checklists, formatting and images survive saving and reopening.
Never silently strip formatting or keep it only locally if the server cannot preserve it.

A Markdown source tab is optional and not yet decided. If added, both tabs edit the
same note; switching tabs preserves content and never shows a stale version.
Do not silently lose unknown Markdown/HTML from Memos in a WYSIWYG roundtrip.
Keep unsupported content intact and warn specifically if visual editing is not safe.

## 3. Storage choice

There is no onboarding flow: the app starts immediately in local mode, without an
account, key or network requests. Memos accounts are added in Settings, by entering the
server address and personal app key and testing the connection.

In Settings > Storage it stays possible to connect or disconnect Memos later.
Current implementation: multiple Memos accounts with separate collections; local notes
stay separate. The original restriction to one account was not applied.
Connecting never silently uploads existing local notes. An explicit copy/import action
shows the selection and destination and keeps the local originals until success is confirmed.
Disconnecting warns about unsynchronized changes and offers export.
An unreachable server never silently switches a Memos collection to local mode.

Local storage proposal: SQLite for notes, folders, search index and sync administration.
Keep image files in app-private local storage, with references in SQLite.
Local images remain available without a backend; export includes the files.
Storage follows XDG locations; in Flatpak the corresponding app-private locations.
Local data is not automatically encrypted: disk encryption and optional additional
database encryption must be assessed separately.
Markdown import and export belong in the first release. Import lets the user select
one or many `.md` files and choose both the destination account (including On this
device) and destination folder/space. Export first asks which account or local
collection to export, then creates a ZIP containing one directory per folder/space
and the corresponding Markdown files. Folder names and Markdown content must be
preserved safely in archive paths. Image files and note metadata should be included
in export. Imports create new notes and queue Memos destinations through the normal
sync flow. Relative image paths and app-specific metadata in incoming Markdown are
not imported yet; image and metadata roundtrip support remains a follow-up.

## 4. Memos 0.31.0: verified API agreements

The upstream API definitions of tag v0.31.0 were consulted, not the real Umbrel server.
An integration test against the user's installation remains necessary for release.

| Area | API / meaning |
| --- | --- |
| Verify user | `GET /api/v1/auth/me` |
| Read / create notes | `GET /api/v1/memos`, `POST /api/v1/memos` |
| Read / update / delete a note | `GET`, `PATCH`, `DELETE /api/v1/memos/{id}` |
| Folders via spaces | `GET`, `POST /api/v1/spaces`; `GET`, `PATCH`, `DELETE /api/v1/spaces/{id}` |
| App key | Personal Access Token (PAT), not the short-lived session token |
| Content | Markdown; `property.title` is derived from the first H1 |
| Grouping | Optional `space` field, for example `spaces/{id}` |
| Tags | Derived from content and output-only; not an independently writable folder field |
| Updates | Field masks: change only the intended fields |
| Lists | Handle pagination completely; choose server filters and sorting explicitly |
| Archive | State `ARCHIVED`; not the same as a recoverable trash |

PAT management exists under `/api/v1/users/{user}/personalAccessTokens`.
Memonotes does not create this key itself; the user does that in Memos.
The proposed authentication is `Authorization: Bearer <PAT>`; test this explicitly
against the target installation, including wrong, expired and revoked tokens.

Security:

- Fedora: store the PAT through the Secret Service/keyring, not in SQLite, configuration or logs.
- If the keyring is unavailable: a clear error or session-only use, no plaintext fallback.
- HTTPS by default. HTTP for a local Umbrel network only after an explicit warning;
  the key and notes are not transport-encrypted there.
- Never silently disable TLS certificate verification; do not forward the token to other hosts.
- The user must be able to reach the server themselves, locally or through their own secured access.
  Memonotes does not automatically make Umbrel reachable from the internet.
- Process rich-text content and images safely; allow no executable HTML or scripts from notes.

## 5. Decision: a space is a folder

Product decision: a Memos space is a folder containing notes, without sub-folders.
To verify technically: a space with only the owner as member works as a personal folder.
Local folders get a local ID; Memos folders refer to a real space resource.
Notes without `space` appear under No folder. In version 1 at most one folder per note
and no nested folders: the consulted Space definition offers no parent field.

- New Memonotes notes are `PRIVATE` by default, also when they are in a space.
- Moving to a folder does not implicitly change visibility.
- Shared spaces and non-private notes are out of scope and are not shown in Memonotes; the cache keeps them only for sync administration.
- Invitations, member management and publishing are outside version 1.
- Deleting a space can, according to the API, also delete all placed notes.
  The confirmation dialog therefore offers three choices: cancel, delete the folder
  and keep the notes, or delete the folder with all its notes. When keeping, notes are
  first moved to No folder; when deleting, notes are deleted first. Only then is
  `FolderDeleted` processed. The core space flow has been tested live.
- Discriminating integration test: use a PAT to create an own space, attach a PRIVATE
  note, rename it and read/move it from a second client.
- If the space semantics turn out to be unsuitable, propose an alternative and agree again;
  do not introduce a hidden metadata or tag convention without a decision.

## 6. Offline and synchronization

The UI always reads and writes locally. In Memos mode, SQLite and a durable
outbox are the local cache and queue; Memos remains the shared server for multiple devices.
Saving a note and the corresponding queue action form one database transaction.

Proposed minimal model:

- Account: mode, server address, remote user ID and reference to the keyring item.
- Folder: local ID, account ID, name, optional remote space ID and sync status.
- Note: local ID, account ID, folder ID, Markdown, pin/state/visibility and remote memo ID.
- ImageAsset: local ID, file path, media type, optional remote attachment ID and upload/cache status.
- SyncMetadata: last synchronized server content, remote update_time, local revision.
- Outbox: durable mutations, target ID, payload, retry status and dependencies.
- Conflict: base version, local version and remote version for recovery and comparison.

Sync runs on connecting, return online, manual refresh and periodically while
the app runs. Network work never blocks the GTK main loop.
Pagination, backoff, authentication errors and folder creation before note creation belong to it.
Inserting images offline stores the file locally and creates a durable upload action.
On sync, first create the required memo/attachment resources according to the tested Memos API,
then publish the final image reference; never send local file paths to the server.
Upload retries and interrupted uploads must not produce duplicate or orphaned attachments.
Cache linked images locally for offline reading; report missing files visibly.
The attachment API and URL/authentication are tested for images.
Retries must not create duplicate notes: use stable API-compatible IDs and
query the server state first after an uncertain create outcome. This is implemented
for note creates and attachment uploads.
A change during an upload is not marked as synchronized by the upload acknowledgement.

Do not resolve conflicts with blind last-write-wins: compare base, local and remote.
The app keeps those three versions and lets the user keep local or server.
Automatic merging does not exist.
The API definitions consulted prove no atomic compare-and-swap, so a read-before-write
is not complete race protection. A change made by another client in that window is
detected on the next fetch and reported as a version conflict.
Missing items in a partial list never automatically mean deletion.
Remote deletions are processed only after direct GET confirmation. Deletions with
pending local edits or a concurrent server edit lead to recovery/conflict,
not silent data loss.
The current app offers archiving and hard deletion from the archive, with an
in-memory undo stack. `NoteRestored` recreates a deleted snapshot; attachments,
relations and shares are not restored. A robust design for deletion/restore
and a possible trash remains necessary.

## 7. Linux/GNOME architecture

Desktop layers: GTK/libadwaita UI -> Rust domain logic -> SQLite repository and Memos API adapter.
Evaluate suitable building blocks: gtk-rs, libadwaita-rs, rusqlite, reqwest, serde,
Secret Service integration, a proven rich-text engine and Markdown conversion.
Do not write an own Markdown parser; do not lose unknown Memos content on edit.

The editor prototype determines whether a suitable native editor integration suffices or whether an
embedded editor is needed. An embedded solution does not change the Rust/GTK app shell,
but needs explicit validation of GNOME integration, focus, accessibility, safe
content handling and Flatpak packaging before it is chosen.
No cross-platform UI framework, mobile bindings or shared React Native core in scope.
Focus on GNOME window controls, system file chooser, keyring, keyboard and Wayland.
Memos collections can sync with other clients through the server; local-only collections cannot.

## 8. Phased execution and acceptance

1. API test: check PAT, current user, CRUD, space mapping, PRIVATE behavior, update masks,
   pagination, archive, attachments and concurrent updates on Memos 0.31.0.
2. UI/editor prototype: three panes, selection, search, WYSIWYG and adaptive navigation.
   Show checkboxes, bold/italic and inline images working; validate roundtrip and
   GNOME integration before the editor engine is finally chosen.
3. Local MVP: SQLite, autosave, flat folders, moving, pinning, archive, batch Markdown import and ZIP export.
   Starting without a backend makes no network requests; restart and crash lose no confirmed saves.
4. Memos integration: server/PAT setup, keyring, fetching and durable outbox.
   Without network, read/edit text and images; after reconnect, sync without duplicates or silent loss.
5. Robustness and Fedora release: sync conflicts, token errors, migrations, packaging.
   Distribution proposal: Flatpak; RPM only when needed. Test network and keyring access in the sandbox.

Test coverage focuses on storage transactions, restart with pending outbox, lost
responses, retries, concurrent edits, remote deletions and account switching.
Editor checks include formatting/checklist roundtrips, images
after restart, offline image uploads, undo/redo and preservation of unknown Memos content.
UI checks include empty/error/offline states, pane resizing, keyboard operation and accessibility.

## 9. Still to decide together

- How should Markdown imports resolve relative image files and app-specific metadata?
- Add an optional Markdown source tab or offer WYSIWYG only?
- How close should the design be to Apple Notes versus standard libadwaita?
- Besides confirmed images, also other file attachments in version 1?
- Definitively include multiple linked accounts, as currently built, in version 1?

Technical experiments, not open product choices: editor engine,
personal space semantics and safe offline image sync. Mobile is out of scope.

## 10. Sources and limitations

- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/memo_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/auth_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/space_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/user_service.proto

The app and local storage/API mock tests now exist in this workspace.
The current code is authoritative for the implemented checklist; the other sections
remain product goals and acceptance criteria. Core live server compatibility is
confirmed; release/sandbox tests are not yet validated.

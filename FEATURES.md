# Memonotes features

Feature overview as of 2026-10-14. Items marked *(open)* are not finished yet;
see [PLAN.md](PLAN.md) for the roadmap and [README.md](README.md) for technical details.

## Native Linux and pluggable backends

- Native GTK4/libadwaita interface written in Rust; no Electron or embedded browser.
- Backend-independent note and folder models behind a `BackendAdapter`; Memos is the first backend (IMAP and Joplin are possible future options, not built).
- Multiple accounts side by side, each with its own folders, sync state and outbox, plus local-only notes.
- The same remote name on two servers never mixes notes or folders.

## Notes and organization

- Three-pane layout (sidebar, note list, editor) with resizable panes and a collapsible sidebar.
- Folders (flat, no sub-folders), creation, renaming and navigation.
- Full-text note search.
- Pinning, archive and restore, with undo for archive actions.
- Permanent deletion of archived notes after confirmation.
- Multi-select and drag-and-drop of notes into folders.
- Move notes between folders or out of a folder.
- 32 pastel note colors with light and dark variants.
- Notes can be opened in separate editable windows.
- Safe folder deletion: cancel, delete the folder but keep its notes, or delete the folder and its notes.
- Fast with large collections: GTK's virtualized `ListView` keeps a library of 2,700+ notes responsive.

## Editor

- WYSIWYG editing: no visible Markdown syntax and no separate title field.
- Bold, italic and underline; headings H1-H6; bulleted lists with nesting; clickable checklists.
- Links (`Ctrl+K`, `Ctrl+click` to open).
- Inline images that can be selected, dragged and resized from their corners while keeping their aspect ratio.
- Undo and redo, including for image changes.
- The first line is the title (H1) and is saved as Markdown so Memos derives the same title.
- Existing content stays unchanged until you edit it; unknown content is not silently stripped.
- Markdown export with folders, metadata and images *(open)*.

## Local-first storage

- Works completely offline and without an account: SQLite storage, autosave, no network requests for local-only folders.
- Notes and images live in your own data directory (`$XDG_DATA_HOME/memonotes`).
- Offline edits are stored durably and survive app restarts.

## Memos integration

- Multiple Memos accounts, each enabled or disabled separately, with a connection test.
- Personal Access Token stored in GNOME Keyring, never in SQLite, configuration or logs.
- Memos spaces appear as folders; only personal `PRIVATE` notes are shown.
- Background fetching at startup, after saving an account, on enabling an account and every 60 seconds; paginated fetching of large collections.
- Automatic upload through a durable outbox: batched, debounced one second after the last edit, with consecutive updates merged.
- Automatic network detection (GIO): API calls pause when offline and resume with a full compare when the network returns.
- Capped exponential retry (30 seconds up to 15 minutes) and a **Sync now** control.
- Image attachments are uploaded and linked to the memo, downloaded to a local cache, and mapped from images added by other clients. Removed images are unbound and deleted remotely.
- Memos 0.31.0 compatible; works with Memos on Umbrel; core flows tested live against a real server.

## Safe synchronization

- Three-way conflict detection keeps the base, local and server versions until you choose **Keep Local** or **Keep Server**.
- Local deletes that collide with server edits can also be resolved.
- Missing items in a partial list never imply deletion; remote deletions are confirmed with a direct request first.
- Uncertain creates and HTTP 409 are reconciled through the stable ID, so retries do not create duplicates.
- Pending local changes are never overwritten by a refresh.
- Automatic merging and live multi-client race validation *(open)*.

## Desktop integration

- Rust, GTK4 and libadwaita: native GNOME look with light and dark appearance following the system.
- Keyboard shortcuts for new note/folder, search, close, quit and undo.
- Interface translated into English, German, French, Dutch and Spanish, with a language picker (**Settings > General > Language**) or system default.
- API failures shown as deduplicated toasts; background fetching and autosave are silent.
- Fedora/Flatpak packaging, onboarding, narrow-window navigation and accessibility work *(open)*.

## Not planned for version 1

Shared spaces and invitations, publishing, API-based PAT management, non-image attachments, mobile apps.

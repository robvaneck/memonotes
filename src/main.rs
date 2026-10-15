use adw::gtk;
use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, ToolbarView};
use anyhow::{Context, Result};
use libadwaita as adw;
use memonotes::adapters::BackendAdapter;
use memonotes::editor_document::{
    ImageSpan, LinkSpan, Mark, MarkupSpan, is_openable_link, markdown_inline_text, markdown_link,
    markdown_preview_text, normalize_link_url, render_markdown_source,
};
use memonotes::i18n::tr;
use memonotes::markdown_io;
use memonotes::{
    BackendAccount, FolderDeleteMode, Note, NoteFilter, Store, SyncConflictResolution,
    SyncedNoteState,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod editor_table;
mod note_list;

use note_list::{NoteList, NoteListEntry};

const NOTE_COLOR_PALETTE: [(&str, &str, &str, &str); 32] = [
    ("default", "color_default", "#ffffff", "#242424"),
    ("gray", "color_gray", "#f1f1f0", "#2a2a2a"),
    ("mauve", "color_mauve", "#f0eff1", "#2e2b31"),
    ("slate", "color_slate", "#eff0f3", "#292d32"),
    ("sage", "color_sage", "#eff1f0", "#292d2b"),
    ("olive", "color_olive", "#f1f1ed", "#2d2e26"),
    ("sand", "color_sand", "#f1f0ed", "#2e2d2b"),
    ("tomato", "color_tomato", "#feebe7", "#3b1813"),
    ("red", "color_red", "#feebec", "#3b1219"),
    ("ruby", "color_ruby", "#ffeaef", "#3a141e"),
    ("crimson", "color_crimson", "#ffe9f0", "#381525"),
    ("pink", "color_pink", "#fee9f5", "#3b152f"),
    ("plum", "color_plum", "#fce9f8", "#351a36"),
    ("purple", "color_purple", "#f2eafe", "#301c3b"),
    ("violet", "color_violet", "#f4f0fe", "#2a1c40"),
    ("iris", "color_iris", "#efedfe", "#24203f"),
    ("indigo", "color_indigo", "#e8f0fe", "#1c2340"),
    ("blue", "color_blue", "#e6f4fe", "#172b46"),
    ("cyan", "color_cyan", "#e1f8fa", "#12343b"),
    ("teal", "color_teal", "#e1f8f4", "#12372f"),
    ("jade", "color_jade", "#e6f7ed", "#123a31"),
    ("green", "color_green", "#e6f6eb", "#17392c"),
    ("grass", "color_grass", "#e9f6e9", "#25391d"),
    ("bronze", "color_bronze", "#f5f0e7", "#352c25"),
    ("gold", "color_gold", "#f9f1dd", "#382f1e"),
    ("brown", "color_brown", "#f6eee7", "#38291f"),
    ("orange", "color_orange", "#feefe7", "#3c271b"),
    ("amber", "color_amber", "#fff1c6", "#3c3015"),
    ("yellow", "color_yellow", "#fff7c2", "#3a3214"),
    ("lime", "color_lime", "#f2f9c7", "#303716"),
    ("mint", "color_mint", "#e1f9f2", "#15382e"),
    ("sky", "color_sky", "#e1f6fd", "#183747"),
];

const LEGACY_NOTE_COLORS: [&str; 4] = ["rose", "peach", "lemon", "lilac"];
const SAVE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// The account id is `None` for notes that only live on this device.
#[derive(Clone, PartialEq, Eq)]
enum ActiveNoteFilter {
    All(Option<String>),
    Unfiled(Option<String>),
    Folder(Option<String>, String),
    Archive(Option<String>),
}

#[derive(Clone)]
enum UndoOperation {
    Archive(Note),
}

#[derive(Clone)]
struct EditorSnapshot {
    markdown: String,
    cursor: i32,
}

#[derive(Clone)]
struct EditorImageChange {
    before: EditorSnapshot,
    after: EditorSnapshot,
    // Table edits are undone in place; the snapshots are the fallback once the table was rebuilt.
    table_patch: Option<editor_table::TablePatch>,
}

#[derive(Default)]
struct EditorImageHistory {
    undo: Vec<EditorImageChange>,
    redo: Vec<EditorImageChange>,
    internal_change: bool,
    handles: Vec<gtk::Widget>,
    handle_positions: Vec<(i32, i32)>,
    resize_target: Option<gtk::glib::WeakRef<gtk::Widget>>,
    resize_gesture: Option<gtk::GestureDrag>,
    pending_delete: Option<EditorSnapshot>,
    native_undo_wipe_pending: bool,
}

type SharedEditorImageHistory = Rc<RefCell<EditorImageHistory>>;

fn new_editor_image_history() -> SharedEditorImageHistory {
    Rc::new(RefCell::new(EditorImageHistory::default()))
}

thread_local! {
    static EDITOR_HISTORIES: RefCell<Vec<(gtk::glib::WeakRef<gtk::TextView>, SharedEditorImageHistory)>> =
        const { RefCell::new(Vec::new()) };
}

fn register_editor_image_history(editor: &gtk::TextView, history: &SharedEditorImageHistory) {
    EDITOR_HISTORIES.with_borrow_mut(|entries| {
        entries.retain(|(weak, _)| weak.upgrade().is_some());
        entries.push((editor.downgrade(), Rc::clone(history)));
    });
}

fn editor_image_history(editor: &gtk::TextView) -> Option<SharedEditorImageHistory> {
    EDITOR_HISTORIES.with_borrow(|entries| {
        entries
            .iter()
            .find(|(weak, _)| weak.upgrade().as_ref() == Some(editor))
            .map(|(_, history)| Rc::clone(history))
    })
}

impl ActiveNoteFilter {
    fn is_archived(&self) -> bool {
        matches!(self, Self::Archive(_))
    }

    fn account_id(&self) -> Option<String> {
        match self {
            Self::All(account_id)
            | Self::Unfiled(account_id)
            | Self::Folder(account_id, _)
            | Self::Archive(account_id) => account_id.clone(),
        }
    }

    fn folder_id(&self) -> Option<String> {
        match self {
            Self::Folder(_, folder_id) => Some(folder_id.clone()),
            Self::All(_) | Self::Unfiled(_) | Self::Archive(_) => None,
        }
    }

    fn for_folder_picker(account_id: Option<String>, folder_id: Option<String>) -> Self {
        match folder_id {
            Some(folder_id) => Self::Folder(account_id, folder_id),
            None => Self::Unfiled(account_id),
        }
    }

    fn to_note_filter(&self, search: String) -> NoteFilter {
        let (folder_id, unfiled, archived) = match self {
            Self::All(_) => (None, false, false),
            Self::Unfiled(_) => (None, true, false),
            Self::Folder(_, folder_id) => (Some(folder_id.clone()), false, false),
            Self::Archive(_) => (None, false, true),
        };
        NoteFilter {
            account_id: self.account_id(),
            folder_id,
            unfiled,
            archived,
            search,
        }
    }
}

struct SidebarEntry {
    row: gtk::ListBoxRow,
    filter: ActiveNoteFilter,
    count_label: gtk::Label,
}

struct WindowState {
    store: RefCell<Store>,
    signal_handlers: RefCell<
        Vec<(
            gtk::glib::WeakRef<gtk::glib::Object>,
            gtk::glib::SignalHandlerId,
        )>,
    >,
    api_sources: RefCell<HashMap<u32, gtk::glib::SourceId>>,
    toast_overlay: adw::ToastOverlay,
    api_toast: RefCell<Option<adw::Toast>>,
    api_errors: RefCell<HashMap<String, String>>,
    account_statuses: RefCell<HashMap<String, bool>>,
    account_indicators: RefCell<HashMap<String, gtk::Box>>,
    account_alerts: RefCell<HashMap<String, gtk::Button>>,
    loading_accounts: RefCell<HashSet<String>>,
    loading_memos: RefCell<HashSet<String>>,
    loading_folders: RefCell<HashSet<(String, String)>>,
    uploading_accounts: RefCell<HashSet<String>>,
    upload_retry_at: RefCell<HashMap<String, Instant>>,
    upload_retry_attempts: RefCell<HashMap<String, u32>>,
    refresh_after_upload: RefCell<HashSet<String>>,
    upload_after: RefCell<HashMap<String, Instant>>,
    note_list_refresh_generation: Cell<u64>,
    checked_outbox_revision: RefCell<HashMap<String, i64>>,
    shown_sync_conflicts: RefCell<HashSet<String>>,
    note_color: RefCell<String>,
    undo_stack: RefCell<Vec<UndoOperation>>,
    active_filter: RefCell<ActiveNoteFilter>,
    note_id: RefCell<Option<String>>,
    note_windows: RefCell<HashMap<String, ApplicationWindow>>,
    note_window_editors: RefCell<HashMap<String, gtk::glib::WeakRef<gtk::TextView>>>,
    suppress_detached_editor_events: Cell<bool>,
    note_list: NoteList,
    note_list_focus: gtk::ScrolledWindow,
    search: gtk::SearchEntry,
    content: gtk::TextView,
    pin_button: gtk::Button,
    archive_button: gtk::Button,
    delete_button: gtk::Button,
    folder_button: gtk::Button,
    folder_popover: gtk::Popover,
    folder_menu: gtk::ListBox,
    folder_picker: gtk::DropDown,
    folder_options: RefCell<Vec<Option<String>>>,
    sidebar: gtk::ListBox,
    sidebar_entries: RefCell<Vec<SidebarEntry>>,
    note_surfaces: RefCell<Vec<gtk::Widget>>,
    selection_anchor: RefCell<Option<String>>,
    suppress_folder_picker_events: Cell<bool>,
    suppress_editor_events: Cell<bool>,
}

#[derive(Clone)]
struct AccountRows {
    group: adw::glib::WeakRef<adw::PreferencesGroup>,
    parent: adw::glib::WeakRef<ApplicationWindow>,
    rows: Rc<RefCell<Vec<adw::glib::WeakRef<gtk::Widget>>>>,
    add_row: adw::ButtonRow,
    add_row_attached: Rc<Cell<bool>>,
}

impl AccountRows {
    fn new(
        group: &adw::PreferencesGroup,
        parent: &ApplicationWindow,
        add_row: &adw::ButtonRow,
    ) -> Self {
        Self {
            group: group.downgrade(),
            parent: parent.downgrade(),
            rows: Rc::new(RefCell::new(Vec::new())),
            add_row: add_row.clone(),
            add_row_attached: Rc::new(Cell::new(false)),
        }
    }
}

fn main() -> Result<()> {
    let flags = if std::env::var_os("MEMONOTES_DEV").is_some() {
        adw::gio::ApplicationFlags::NON_UNIQUE
    } else {
        adw::gio::ApplicationFlags::FLAGS_NONE
    };
    let application = Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();

    application.connect_activate(|application| {
        if let Err(error) = build_window(application) {
            eprintln!(
                "{}",
                memonotes::tr_format!("app.error.startup_failed", error = format!("{error:#}"))
            );
        }
    });

    application.run();
    Ok(())
}

fn build_window(application: &Application) -> Result<()> {
    let display = gtk::gdk::Display::default().context(tr("app.error.open_display"))?;
    gtk::IconTheme::for_display(&display).add_search_path(env!("CARGO_MANIFEST_DIR"));
    let store = Store::open(database_path()?).context(tr("storage.error.open_local_notes"))?;
    memonotes::i18n::initialize_language(store.language_preference()?.as_deref());
    let initial_filter = ActiveNoteFilter::All(
        sidebar_sections(&store)?
            .into_iter()
            .next()
            .and_then(|(account_id, _)| account_id),
    );
    let initial_notes = store.notes(&initial_filter.to_note_filter(String::new()))?;
    let initial_note = initial_notes.first().cloned();
    let note_color = store.note_color()?;
    let folder_model = gtk::StringList::new(&[tr("folder.unfiled")]);
    let folder_options = vec![None];

    let note_list = NoteList::new();
    note_list.view.set_margin_bottom(12);
    note_list.view.set_margin_start(12);
    note_list.view.set_margin_end(12);
    let list_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&note_list.view)
        .build();
    list_scroll.set_focusable(true);
    let search = gtk::SearchEntry::builder()
        .placeholder_text(tr("note.search.placeholder"))
        .build();
    search.set_margin_top(12);
    search.set_margin_start(12);
    search.set_margin_end(12);
    search.set_width_chars(12);
    search.set_hexpand(true);
    let content = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(48)
        .left_margin(44)
        .right_margin(44)
        .build();
    content.add_css_class("note-detail-surface");
    prepare_empty_heading(&content);
    let pin_button = icon_button("view-pin-symbolic", tr("note.pin"));
    let archive_button = icon_button("user-trash-symbolic", tr("note.archive"));
    let delete_button = icon_button("user-trash-symbolic", tr("note.delete_permanently"));
    delete_button.add_css_class("destructive-action");
    delete_button.set_visible(false);
    let folder_picker = gtk::DropDown::new(Some(folder_model.clone()), None::<&gtk::Expression>);
    folder_picker.set_tooltip_text(Some(tr("note.move_to_folder")));
    let folder_button = icon_button("folder-symbolic", tr("note.move_to_folder"));
    let folder_menu = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    folder_menu.set_activate_on_single_click(true);
    folder_menu.add_css_class("folder-context-menu");
    let folder_popover = gtk::Popover::new();
    folder_popover.set_has_arrow(false);
    folder_popover.set_child(Some(&folder_menu));
    folder_popover.set_parent(&folder_button);
    let folder_popover_for_button = folder_popover.clone();
    folder_button.connect_clicked(move |_| {
        if folder_popover_for_button.is_visible() {
            folder_popover_for_button.popdown();
        } else {
            folder_popover_for_button.popup();
        }
    });
    pin_button.set_sensitive(false);
    archive_button.set_sensitive(false);
    folder_button.set_sensitive(false);
    folder_picker.set_sensitive(false);

    let state = Rc::new(WindowState {
        store: RefCell::new(store),
        signal_handlers: RefCell::new(Vec::new()),
        api_sources: RefCell::new(HashMap::new()),
        toast_overlay: adw::ToastOverlay::new(),
        api_toast: RefCell::new(None),
        api_errors: RefCell::new(HashMap::new()),
        account_statuses: RefCell::new(HashMap::new()),
        account_indicators: RefCell::new(HashMap::new()),
        account_alerts: RefCell::new(HashMap::new()),
        loading_accounts: RefCell::new(HashSet::new()),
        loading_memos: RefCell::new(HashSet::new()),
        loading_folders: RefCell::new(HashSet::new()),
        uploading_accounts: RefCell::new(HashSet::new()),
        upload_retry_at: RefCell::new(HashMap::new()),
        upload_retry_attempts: RefCell::new(HashMap::new()),
        refresh_after_upload: RefCell::new(HashSet::new()),
        upload_after: RefCell::new(HashMap::new()),
        note_list_refresh_generation: Cell::new(0),
        checked_outbox_revision: RefCell::new(HashMap::new()),
        shown_sync_conflicts: RefCell::new(HashSet::new()),
        note_color: RefCell::new(note_color),
        undo_stack: RefCell::new(Vec::new()),
        active_filter: RefCell::new(initial_filter),
        note_id: RefCell::new(initial_note.as_ref().map(|note| note.id.clone())),
        note_windows: RefCell::new(HashMap::new()),
        note_window_editors: RefCell::new(HashMap::new()),
        suppress_detached_editor_events: Cell::new(false),
        note_list,
        note_list_focus: list_scroll.clone(),
        search: search.clone(),
        content: content.clone(),
        pin_button: pin_button.clone(),
        archive_button: archive_button.clone(),
        delete_button: delete_button.clone(),
        folder_button: folder_button.clone(),
        folder_popover,
        folder_menu,
        folder_picker: folder_picker.clone(),
        folder_options: RefCell::new(folder_options),
        sidebar: gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build(),
        sidebar_entries: RefCell::new(Vec::new()),
        note_surfaces: RefCell::new(Vec::new()),
        selection_anchor: RefCell::new(initial_note.as_ref().map(|note| note.id.clone())),
        suppress_folder_picker_events: Cell::new(false),
        suppress_editor_events: Cell::new(false),
    });

    let window = ApplicationWindow::builder()
        .application(application)
        .title("Memonotes")
        .default_width(1180)
        .default_height(780)
        .build();
    window.set_icon_name(Some("icon"));
    install_pinned_separator_style(&window);
    install_account_error_style(&window);
    install_note_detail_style(&window);
    let weak_state = Rc::downgrade(&state);
    window.connect_destroy(move |window| {
        if let Some(state) = weak_state.upgrade() {
            for (_, source) in state.api_sources.borrow_mut().drain() {
                source.remove();
            }
            let handlers = state.signal_handlers.take();
            for (object, handler) in handlers {
                if let Some(object) = object.upgrade() {
                    object.disconnect(handler);
                }
            }
            for action in window.list_actions() {
                window.remove_action(&action);
            }
            if let Some(toast) = state.api_toast.borrow_mut().take() {
                toast.dismiss();
            }
            let note_windows = state.note_windows.take();
            for (_, note_window) in note_windows {
                note_window.destroy();
            }
            state.sidebar_entries.borrow_mut().clear();
            state.note_list.replace(Vec::new());
            while let Some(child) = state.sidebar.first_child() {
                state.sidebar.remove(&child);
            }
            state.folder_popover.unparent();
        }
    });
    let add_folder = icon_button("folder-new-symbolic", tr("folder.add"));
    let sidebar = build_sidebar(&state, &window, &add_folder)?;
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let new_note = icon_button("document-new-symbolic", tr("note.new"));
    list_column.append(&search);
    list_column.append(&list_scroll);

    let editor = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor.add_css_class("note-detail-surface");
    let image_history = new_editor_image_history();
    let format_toolbar = editor_format_toolbar(&content, Rc::clone(&image_history));
    editor.append(&format_toolbar);
    install_editor_format_shortcuts(&content, image_history);
    let editor_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    editor_actions.append(&pin_button);
    editor_actions.append(&archive_button);
    editor_actions.append(&delete_button);
    editor_actions.append(&folder_button);
    editor_actions.set_halign(gtk::Align::End);
    editor_actions.set_valign(gtk::Align::Center);
    let editor_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&content)
        .build();
    editor_scroll.add_css_class("note-detail-surface");
    editor.append(&editor_scroll);

    let navigation_layout = gtk::Paned::new(gtk::Orientation::Horizontal);
    navigation_layout.add_css_class("sidebar-layout");
    navigation_layout.set_wide_handle(false);
    navigation_layout.set_hexpand(true);
    navigation_layout.set_start_child(Some(&sidebar));
    navigation_layout.set_end_child(Some(&list_column));
    navigation_layout.set_position(225);

    let navigation_toolbar = ToolbarView::new();
    let navigation_header = HeaderBar::new();
    navigation_toolbar.add_top_bar(&navigation_header);
    navigation_toolbar.set_content(Some(&navigation_layout));

    let editor_toolbar = ToolbarView::new();
    editor_toolbar.add_css_class("note-detail-surface");
    editor_toolbar.add_css_class("note-detail-pane");
    let editor_header = HeaderBar::new();
    editor_header.add_css_class("note-detail-surface");
    editor_header.add_css_class("note-detail-pane");
    editor_header.set_title_widget(Some(&gtk::Label::new(None)));
    editor_header.set_show_start_title_buttons(false);
    editor_header.set_show_end_title_buttons(false);
    editor_header.pack_end(&editor_actions);
    editor_toolbar.add_top_bar(&editor_header);
    editor_toolbar.set_content(Some(&editor));
    let note_surfaces = collect_note_surfaces(&editor_toolbar.clone().upcast());
    *state.note_surfaces.borrow_mut() = note_surfaces.clone();

    let layout = gtk::Paned::new(gtk::Orientation::Horizontal);
    layout.set_wide_handle(false);
    layout.set_hexpand(true);
    layout.set_start_child(Some(&navigation_toolbar));
    layout.set_end_child(Some(&editor_toolbar));
    layout.set_position(535);
    let sidebar_width = Rc::new(Cell::new(225));

    let sidebar_icon = gtk::Image::from_icon_name("sidebar-show-symbolic");
    let sidebar_toggle = gtk::ToggleButton::new();
    sidebar_toggle.set_child(Some(&sidebar_icon));
    sidebar_toggle.set_active(true);
    sidebar_toggle.set_tooltip_text(Some(tr("sidebar.toggle")));
    let layout_for_toggle = navigation_layout.clone();
    let sidebar_width_for_toggle = Rc::clone(&sidebar_width);
    let animation_generation = Rc::new(Cell::new(0_u64));
    let animation_running = Rc::new(Cell::new(false));
    sidebar_toggle.connect_toggled(move |button| {
        let was_animating = animation_running.get();
        let generation = animation_generation.get().wrapping_add(1);
        animation_generation.set(generation);
        let target = if button.is_active() {
            sidebar_width_for_toggle.get()
        } else {
            let current_width = layout_for_toggle.position();
            if current_width > 0 && !was_animating {
                sidebar_width_for_toggle.set(current_width);
            }
            0
        };
        animate_sidebar_position(
            &layout_for_toggle,
            target,
            generation,
            Rc::clone(&animation_generation),
            Rc::clone(&animation_running),
        );
    });

    navigation_header.pack_start(&sidebar_toggle);
    navigation_header.pack_start(&add_folder);
    let more_button = gtk::MenuButton::new();
    more_button.set_icon_name("view-more-symbolic");
    more_button.set_tooltip_text(Some(tr("menu.more")));
    let titlebar_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    titlebar_actions.append(&new_note);
    let sync_button = icon_button("view-refresh-symbolic", tr("sync.refresh_now"));
    sync_button.set_size_request(36, 36);
    let sync_icon = gtk::Image::from_icon_name("view-refresh-symbolic");
    sync_icon.set_pixel_size(16);
    sync_icon.set_halign(gtk::Align::Center);
    sync_icon.set_valign(gtk::Align::Center);
    let sync_spinner = gtk::Spinner::new();
    sync_spinner.set_size_request(16, 16);
    sync_spinner.set_halign(gtk::Align::Center);
    sync_spinner.set_valign(gtk::Align::Center);
    sync_spinner.set_visible(false);
    let sync_visual = gtk::Overlay::new();
    sync_visual.set_size_request(16, 16);
    sync_visual.set_halign(gtk::Align::Center);
    sync_visual.set_valign(gtk::Align::Center);
    sync_visual.set_child(Some(&sync_icon));
    sync_visual.add_overlay(&sync_spinner);
    sync_button.set_child(Some(&sync_visual));
    let state_for_sync = Rc::clone(&state);
    let window_for_sync = window.clone();
    let button_for_sync = sync_button.clone();
    let icon_for_sync = sync_icon.clone();
    let spinner_for_sync = sync_spinner.clone();
    sync_button.connect_clicked(move |_| {
        request_manual_sync(
            &state_for_sync,
            &window_for_sync,
            &button_for_sync,
            &icon_for_sync,
            &spinner_for_sync,
        );
    });
    titlebar_actions.append(&sync_button);
    titlebar_actions.append(&more_button);
    navigation_header.pack_end(&titlebar_actions);
    install_app_menu(&window, &more_button, &state);
    let toolbar = ToolbarView::new();
    toolbar.set_content(Some(&layout));
    state.toast_overlay.set_child(Some(&toolbar));
    window.set_content(Some(&state.toast_overlay));
    let layout_for_map = navigation_layout.clone();
    window.connect_map(move |_| {
        layout_for_map.set_position(225);
    });

    connect_events(&state, &new_note);
    install_shortcuts(application, &window, &state, &new_note, &add_folder);
    refresh_notes(&state)?;
    if let Some(note) = initial_note {
        show_note(&state, &note);
    }
    window.present();
    watch_backend_network(&state, &window);
    load_backend_accounts(&state, &window);
    start_backend_refresh(&state, &window);
    start_backend_uploads(&state, &window);
    Ok(())
}

fn track_signal(
    state: &WindowState,
    object: &impl IsA<gtk::glib::Object>,
    handler: gtk::glib::SignalHandlerId,
) {
    state
        .signal_handlers
        .borrow_mut()
        .push((object.as_ref().downgrade(), handler));
}

fn api_timeout(
    state: &Rc<WindowState>,
    interval: Duration,
    mut callback: impl FnMut() -> gtk::glib::ControlFlow + 'static,
) {
    let weak_state = Rc::downgrade(state);
    let source_number = Rc::new(Cell::new(0));
    let number_for_callback = Rc::clone(&source_number);
    let source = gtk::glib::timeout_add_local(interval, move || {
        let result = callback();
        if result == gtk::glib::ControlFlow::Break {
            if let Some(state) = weak_state.upgrade() {
                state
                    .api_sources
                    .borrow_mut()
                    .remove(&number_for_callback.get());
            }
        }
        result
    });
    source_number.set(source.as_raw());
    state
        .api_sources
        .borrow_mut()
        .insert(source.as_raw(), source);
}

fn show_api_error(state: &WindowState, key: &str, message: &str) {
    show_api_error_with_action(state, key, message, None);
}

fn show_api_error_with_action(
    state: &WindowState,
    key: &str,
    message: &str,
    action: Option<Box<dyn Fn()>>,
) {
    if state
        .api_errors
        .borrow()
        .get(key)
        .is_some_and(|previous| previous == message)
    {
        return;
    }
    state
        .api_errors
        .borrow_mut()
        .insert(key.to_owned(), message.to_owned());
    if let Some(previous) = state.api_toast.borrow_mut().take() {
        previous.dismiss();
    }
    let toast = adw::Toast::new(message);
    toast.set_use_markup(false);
    toast.set_timeout(8);
    if let Some(action) = action {
        toast.set_button_label(Some(tr("sync.problems.open_note")));
        toast.connect_button_clicked(move |_| action());
    }
    state.toast_overlay.add_toast(toast.clone());
    *state.api_toast.borrow_mut() = Some(toast);
}

fn backend_available(state: &WindowState, account_id: &str) -> bool {
    adw::gio::NetworkMonitor::default().is_network_available()
        && !state
            .upload_retry_at
            .borrow()
            .get(account_id)
            .is_some_and(|deadline| *deadline > Instant::now())
}

fn sync_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(30_u64.saturating_mul(1_u64 << attempt.min(5)).min(900))
}

fn outbox_revision_needs_refresh(checked: Option<i64>, current: i64) -> bool {
    checked.is_none_or(|checked| checked < current)
}

fn next_sync_retry_delay(state: &WindowState, account_id: &str) -> Duration {
    let mut attempts = state.upload_retry_attempts.borrow_mut();
    let attempt = attempts.entry(account_id.to_owned()).or_default();
    let delay = sync_retry_delay(*attempt);
    *attempt = attempt.saturating_add(1);
    delay
}

fn request_manual_sync(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    button: &gtk::Button,
    icon: &gtk::Image,
    spinner: &gtk::Spinner,
) {
    state.upload_retry_at.borrow_mut().clear();
    state.upload_retry_attempts.borrow_mut().clear();
    state.upload_after.borrow_mut().clear();
    state.shown_sync_conflicts.borrow_mut().clear();
    state.api_errors.borrow_mut().retain(|key, _| {
        !key.starts_with("fetch:") && !key.starts_with("upload:") && key != "uploads"
    });
    spinner.start();
    icon.set_visible(false);
    spinner.set_visible(true);
    button.set_sensitive(false);
    load_backend_accounts(state, window);

    let weak_state = Rc::downgrade(state);
    let weak_button = button.downgrade();
    let icon = icon.clone();
    let spinner = spinner.clone();
    api_timeout(state, Duration::from_millis(100), move || {
        let (Some(state), Some(button)) = (weak_state.upgrade(), weak_button.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        let accounts = state.store.borrow().backend_accounts().unwrap_or_default();
        let mut pending_events = false;
        let mut status_error = false;
        for account in accounts
            .iter()
            .filter(|account| account.enabled && memonotes::adapters::is_supported(&account.provider))
        {
            match state.store.borrow().has_backend_events(&account.id) {
                Ok(has_events) => pending_events |= has_events,
                Err(_) => status_error = true,
            }
        }
        let busy = !state.loading_accounts.borrow().is_empty()
            || !state.loading_folders.borrow().is_empty()
            || !state.uploading_accounts.borrow().is_empty();
        let retry_waiting = state
            .upload_retry_at
            .borrow()
            .values()
            .chain(state.upload_after.borrow().values())
            .any(|deadline| *deadline > Instant::now());
        let api_error =
            state.api_errors.borrow().keys().any(|key| {
                key.starts_with("fetch:") || key.starts_with("upload:") || key == "uploads"
            });
        let online = adw::gio::NetworkMonitor::default().is_network_available();
        if busy || (pending_events && online && !retry_waiting && !api_error && !status_error) {
            return gtk::glib::ControlFlow::Continue;
        }
        spinner.stop();
        spinner.set_visible(false);
        icon.set_visible(true);
        button.set_sensitive(true);
        gtk::glib::ControlFlow::Break
    });
}

fn handle_backend_error(
    state: &WindowState,
    account_id: &str,
    key: &str,
    message: &str,
    error: &anyhow::Error,
) {
    handle_backend_error_with_action(state, account_id, key, message, error, None);
}

fn handle_backend_error_with_action(
    state: &WindowState,
    account_id: &str,
    key: &str,
    message: &str,
    error: &anyhow::Error,
    action: Option<Box<dyn Fn()>>,
) {
    set_account_online(state, account_id, false);
    if !adw::gio::NetworkMonitor::default().is_network_available()
        || memonotes::adapters::is_connection_error(error)
    {
        state.upload_retry_at.borrow_mut().insert(
            account_id.to_owned(),
            Instant::now() + next_sync_retry_delay(state, account_id),
        );
        state
            .refresh_after_upload
            .borrow_mut()
            .insert(account_id.to_owned());
        return;
    }
    show_api_error_with_action(state, key, message, action);
}

fn open_rejected_note_action(state: &Rc<WindowState>, account_id: &str) -> Option<Box<dyn Fn()>> {
    let rejection = state
        .store
        .borrow()
        .sync_rejections(account_id)
        .ok()?
        .into_iter()
        .find(|rejection| rejection.is_note)?;
    let weak_state = Rc::downgrade(state);
    Some(Box::new(move || {
        let Some(state) = weak_state.upgrade() else {
            return;
        };
        if let Some(window) = state
            .note_list
            .view
            .root()
            .and_downcast::<ApplicationWindow>()
        {
            navigate_to_note(&state, &window, &rejection.item_id);
        }
    }))
}

fn watch_backend_network(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let monitor = adw::gio::NetworkMonitor::default();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let handler = monitor.connect_network_changed(move |_, available| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        if !available {
            for (account_id, indicator) in state.account_indicators.borrow().iter() {
                state
                    .account_statuses
                    .borrow_mut()
                    .insert(account_id.clone(), false);
                update_account_indicator(indicator, Some(false));
            }
            return;
        }
        state.upload_retry_at.borrow_mut().clear();
        state.upload_retry_attempts.borrow_mut().clear();
        state.checked_outbox_revision.borrow_mut().clear();
        state.upload_after.borrow_mut().clear();
        if let Err(error) = upload_backend_events(&state) {
            show_api_error(
                &state,
                "uploads",
                &memonotes::tr_format!("api.memos.error.sync_failed", error = format!("{error:#}")),
            );
        }
        load_backend_accounts(&state, &window);
    });
    track_signal(state, &monitor, handler);
}

fn start_backend_uploads(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    api_timeout(state, Duration::from_secs(1), move || {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        if !window.is_visible() {
            return gtk::glib::ControlFlow::Break;
        }
        if let Err(error) = upload_backend_events(&state) {
            show_api_error(
                &state,
                "uploads",
                &memonotes::tr_format!("api.memos.error.save_failed", error = format!("{error:#}")),
            );
        } else {
            state.api_errors.borrow_mut().remove("uploads");
        }
        let retry_due = state
            .upload_retry_at
            .borrow()
            .values()
            .any(|deadline| *deadline <= Instant::now());
        if retry_due {
            load_backend_accounts(&state, &window);
        }
        gtk::glib::ControlFlow::Continue
    });
}

fn start_backend_refresh(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    api_timeout(state, Duration::from_secs(60), move || {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        if window.is_visible() {
            load_backend_accounts(&state, &window);
        }
        gtk::glib::ControlFlow::Continue
    });
}

fn upload_backend_events(state: &Rc<WindowState>) -> Result<()> {
    let accounts = state.store.borrow().backend_accounts()?;
    for account in accounts
        .into_iter()
        .filter(|account| account.enabled && memonotes::adapters::is_supported(&account.provider))
    {
        if !backend_available(state, &account.id)
            || state.loading_accounts.borrow().contains(&account.id)
            || !state.loading_memos.borrow().is_empty()
            || state
                .loading_folders
                .borrow()
                .iter()
                .any(|(account_id, _)| account_id == &account.id)
            || state.uploading_accounts.borrow().contains(&account.id)
            || state
                .upload_retry_at
                .borrow()
                .get(&account.id)
                .is_some_and(|retry_at| *retry_at > Instant::now())
            || state
                .upload_after
                .borrow()
                .get(&account.id)
                .is_some_and(|deadline| *deadline > Instant::now())
        {
            continue;
        }
        if !state.store.borrow().has_backend_events(&account.id)? {
            continue;
        }
        let current_revision = state.store.borrow().outbox_revision()?;
        let checked_revision = state
            .checked_outbox_revision
            .borrow()
            .get(&account.id)
            .copied();
        if outbox_revision_needs_refresh(checked_revision, current_revision) {
            precheck_pending_notes(state, account, current_revision);
            continue;
        }
        let path = database_path()?;
        state.upload_after.borrow_mut().remove(&account.id);
        state
            .uploading_accounts
            .borrow_mut()
            .insert(account.id.clone());
        let (sender, receiver) = mpsc::channel();
        let worker_account = account.clone();
        thread::spawn(move || {
            let result = (|| -> Result<usize> {
                let adapter = connect_account_adapter(&worker_account)?;
                let store = Store::open(path)?;
                store.sync_backend_events(&worker_account, adapter.as_ref())
            })();
            let _ = sender.send(result);
        });
        let weak_state = Rc::downgrade(state);
        api_timeout(state, Duration::from_millis(50), move || {
            let Some(state) = weak_state.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!(tr("api.memos.error.save_interrupted")))
                }
            };
            state.uploading_accounts.borrow_mut().remove(&account.id);
            refresh_account_alert(&state, &account.id);
            match result {
                Ok(count) => {
                    if count > 0 {
                        set_account_online(&state, &account.id, true);
                    }
                    state.upload_retry_at.borrow_mut().remove(&account.id);
                    state.upload_retry_attempts.borrow_mut().remove(&account.id);
                    state
                        .api_errors
                        .borrow_mut()
                        .remove(&format!("upload:{}", account.id));
                    if state.refresh_after_upload.borrow_mut().remove(&account.id) {
                        if let Some(window) = state
                            .note_list
                            .view
                            .root()
                            .and_downcast::<ApplicationWindow>()
                        {
                            load_backend_accounts(&state, &window);
                        }
                    }
                }
                Err(error) => {
                    state
                        .checked_outbox_revision
                        .borrow_mut()
                        .remove(&account.id);
                    let connection_error = !adw::gio::NetworkMonitor::default()
                        .is_network_available()
                        || memonotes::adapters::is_connection_error(&error);
                    if !connection_error {
                        let retry_delay = next_sync_retry_delay(&state, &account.id);
                        state
                            .upload_after
                            .borrow_mut()
                            .insert(account.id.clone(), Instant::now() + retry_delay);
                    }
                    handle_backend_error_with_action(
                        &state,
                        &account.id,
                        &format!("upload:{}", account.id),
                        &memonotes::tr_format!(
                            "api.memos.error.account_upload_failed",
                            account = account.label,
                            error = format!("{error:#}")
                        ),
                        &error,
                        memonotes::adapters::is_rejected_request(&error)
                            .then(|| open_rejected_note_action(&state, &account.id))
                            .flatten(),
                    );
                    if !connection_error && !memonotes::adapters::is_rejected_request(&error) {
                        if let Some(window) = state
                            .note_list
                            .view
                            .root()
                            .and_downcast::<ApplicationWindow>()
                        {
                            load_backend_accounts(&state, &window);
                        }
                    }
                }
            }
            gtk::glib::ControlFlow::Break
        });
    }
    Ok(())
}

// Compares only the notes about to be uploaded with the server, instead of a full refresh.
fn precheck_pending_notes(state: &Rc<WindowState>, account: BackendAccount, revision: i64) {
    let notes = match state
        .store
        .borrow()
        .pending_existing_note_resources(&account.id)
    {
        Ok(notes) => notes,
        Err(error) => {
            show_api_error(
                state,
                &format!("upload:{}", account.id),
                &memonotes::tr_format!("api.memos.error.save_failed", error = format!("{error:#}")),
            );
            return;
        }
    };
    if notes.is_empty() {
        state
            .checked_outbox_revision
            .borrow_mut()
            .insert(account.id.clone(), revision);
        return;
    }
    if !state
        .loading_accounts
        .borrow_mut()
        .insert(account.id.clone())
    {
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let worker_account = account.clone();
    thread::spawn(move || {
        let result = (|| -> Result<Vec<(String, Option<memonotes::memo::Memo>)>> {
            let adapter = connect_account_adapter(&worker_account)?;
            notes
                .into_iter()
                .map(|(note_id, name)| Ok((note_id, adapter.get_memo_if_exists(&name)?)))
                .collect()
        })();
        let _ = sender.send(result);
    });
    let weak_state = Rc::downgrade(state);
    api_timeout(state, Duration::from_millis(50), move || {
        let Some(state) = weak_state.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!(tr("api.memos.error.save_interrupted")))
            }
        };
        state.loading_accounts.borrow_mut().remove(&account.id);
        let result = result.and_then(|checked| {
            let current = state.store.borrow().backend_accounts()?;
            if !current.iter().any(|current| current == &account) {
                return Ok(());
            }
            set_account_online(&state, &account.id, true);
            let mut memos = Vec::new();
            let mut deletions = Vec::new();
            for (note_id, memo) in checked {
                match memo {
                    Some(memo) => memos.push(memo),
                    None => deletions.push(note_id),
                }
            }
            state.store.borrow().import_complete_backend_snapshot(
                &account.id,
                &[],
                &memos,
                &deletions,
            )?;
            state
                .checked_outbox_revision
                .borrow_mut()
                .insert(account.id.clone(), revision);
            refresh_notes(&state)?;
            if let Some(window) = state
                .note_list
                .view
                .root()
                .and_downcast::<ApplicationWindow>()
            {
                show_next_sync_conflict(&state, &window, &account.id);
            }
            Ok(())
        });
        if let Err(error) = result {
            let connection_error = !adw::gio::NetworkMonitor::default().is_network_available()
                || memonotes::adapters::is_connection_error(&error);
            if !connection_error {
                let retry_delay = next_sync_retry_delay(&state, &account.id);
                state
                    .upload_after
                    .borrow_mut()
                    .insert(account.id.clone(), Instant::now() + retry_delay);
            }
            handle_backend_error(
                &state,
                &account.id,
                &format!("upload:{}", account.id),
                &memonotes::tr_format!(
                    "api.memos.error.account_upload_failed",
                    account = account.label,
                    error = format!("{error:#}")
                ),
                &error,
            );
        } else if let Err(error) = upload_backend_events(&state) {
            eprintln!("{error:#}");
        }
        gtk::glib::ControlFlow::Break
    });
}

fn load_backend_accounts(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let accounts = match state.store.borrow().backend_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            show_api_error(
                state,
                "accounts",
                &memonotes::tr_format!("account.error.fetch_failed", error = format!("{error:#}")),
            );
            return;
        }
    };
    state.api_errors.borrow_mut().remove("accounts");
    for account in accounts
        .into_iter()
        .filter(|account| account.enabled && memonotes::adapters::is_supported(&account.provider))
    {
        if !backend_available(state, &account.id) {
            continue;
        }
        if state.uploading_accounts.borrow().contains(&account.id) {
            state
                .refresh_after_upload
                .borrow_mut()
                .insert(account.id.clone());
            continue;
        }
        if !state
            .loading_accounts
            .borrow_mut()
            .insert(account.id.clone())
        {
            continue;
        }
        let refresh_outbox_revision = match state.store.borrow().outbox_revision() {
            Ok(revision) => revision,
            Err(error) => {
                state.loading_accounts.borrow_mut().remove(&account.id);
                show_api_error(
                    state,
                    "accounts",
                    &memonotes::tr_format!(
                        "sync.error.read_status_failed",
                        error = format!("{error:#}")
                    ),
                );
                continue;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let worker_account = account.clone();
        thread::spawn(move || {
            let result = (|| -> Result<_> {
                let adapter = connect_account_adapter(&worker_account)?;
                let folders = adapter.list_folders()?;
                let memos = adapter.list_memos()?;
                let store = Store::open(database_path()?)?;
                let listed_names: Vec<_> = memos.iter().map(|memo| memo.id.clone()).collect();
                let missing = store
                    .note_resources_missing_from_snapshot(&worker_account.id, &listed_names)?;
                let mut confirmed_deletions = Vec::new();
                for (note_id, remote_name) in missing {
                    if adapter.get_memo_if_exists(&remote_name)?.is_none() {
                        confirmed_deletions.push(note_id);
                    }
                }
                Ok((folders, memos, confirmed_deletions))
            })();
            let _ = sender.send(result);
        });
        let weak_state = Rc::downgrade(state);
        let weak_window = window.downgrade();
        api_timeout(state, Duration::from_millis(50), move || {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return gtk::glib::ControlFlow::Break;
            };
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!(tr("api.memos.error.fetch_interrupted")))
                }
            };
            state.loading_accounts.borrow_mut().remove(&account.id);
            let result = (|| -> Result<()> {
                let current = state.store.borrow().backend_accounts()?;
                if !current.iter().any(|current| current == &account) {
                    return Ok(());
                }
                let (folders, memos, confirmed_deletions) = result?;
                set_account_online(&state, &account.id, true);
                state.store.borrow().import_complete_backend_snapshot(
                    &account.id,
                    &folders,
                    &memos,
                    &confirmed_deletions,
                )?;
                let current_revision = state.store.borrow().outbox_revision()?;
                if current_revision == refresh_outbox_revision {
                    state
                        .checked_outbox_revision
                        .borrow_mut()
                        .insert(account.id.clone(), current_revision);
                } else {
                    // Edits made meanwhile are compared against the server by the upload precheck.
                    state
                        .checked_outbox_revision
                        .borrow_mut()
                        .remove(&account.id);
                }
                populate_sidebar(&state, &window)?;
                refresh_notes(&state)?;
                let selected_id = state.note_id.borrow().clone();
                if let Some(id) = selected_id {
                    let note = state.store.borrow().note(&id)?;
                    if let Some(note) = note {
                        show_note(&state, &note);
                    }
                } else {
                    let filter = state
                        .active_filter
                        .borrow()
                        .to_note_filter(state.search.text().to_string());
                    let notes = state.store.borrow().notes(&filter)?;
                    if let Some(note) = notes.first() {
                        show_note(&state, note);
                        refresh_notes(&state)?;
                    }
                }
                refresh_open_note_windows(&state);
                show_next_sync_conflict(&state, &window, &account.id);
                Ok(())
            })();
            if let Err(error) = result {
                let connection_error = !adw::gio::NetworkMonitor::default().is_network_available()
                    || memonotes::adapters::is_connection_error(&error);
                if !connection_error
                    && state
                        .store
                        .borrow()
                        .has_backend_events(&account.id)
                        .unwrap_or(false)
                {
                    let retry_delay = next_sync_retry_delay(&state, &account.id);
                    state
                        .upload_after
                        .borrow_mut()
                        .insert(account.id.clone(), Instant::now() + retry_delay);
                }
                handle_backend_error(
                    &state,
                    &account.id,
                    &format!("fetch:{}", account.id),
                    &memonotes::tr_format!(
                        "api.memos.error.account_fetch_failed",
                        account = account.label,
                        error = format!("{error:#}")
                    ),
                    &error,
                );
            } else {
                state.upload_retry_at.borrow_mut().remove(&account.id);
                if !state.upload_after.borrow().contains_key(&account.id) {
                    state.upload_retry_attempts.borrow_mut().remove(&account.id);
                }
                state.refresh_after_upload.borrow_mut().remove(&account.id);
                state
                    .api_errors
                    .borrow_mut()
                    .remove(&format!("fetch:{}", account.id));
            }
            gtk::glib::ControlFlow::Break
        });
    }
}

fn refresh_backend_folder(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    account_id: &str,
    folder_id: &str,
) {
    if !backend_available(state, account_id) {
        return;
    }
    let key = (account_id.to_owned(), folder_id.to_owned());
    let preparation = (|| -> Result<_> {
        let account = state
            .store
            .borrow()
            .backend_accounts()?
            .into_iter()
            .find(|account| {
                account.id == account_id && account.enabled && memonotes::adapters::is_supported(&account.provider)
            });
        let Some(account) = account else {
            return Ok(None);
        };
        let remote_name = state
            .store
            .borrow()
            .backend_folder_name(account_id, folder_id)?;
        Ok(Some((account, remote_name)))
    })();
    let (account, remote_name) = match preparation {
        Ok(Some(prepared)) => prepared,
        Ok(None) => return,
        Err(error) => {
            show_api_error(
                state,
                &format!("fetch:folder:{folder_id}"),
                &memonotes::tr_format!(
                    "api.memos.error.folder_fetch_failed",
                    error = format!("{error:#}")
                ),
            );
            return;
        }
    };
    if !state.loading_folders.borrow_mut().insert(key.clone()) {
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let worker_account = account.clone();
    thread::spawn(move || {
        let result = (|| -> Result<_> {
            let adapter = connect_account_adapter(&worker_account)?;
            let folder = adapter.get_folder(&remote_name)?;
            let memos = adapter.list_folder_memos(&remote_name)?;
            Ok((folder, memos))
        })();
        let _ = sender.send(result);
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let folder_id = folder_id.to_owned();
    api_timeout(state, Duration::from_millis(50), move || {
        let Some(state) = weak_state.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        let Some(window) = weak_window.upgrade() else {
            state.loading_folders.borrow_mut().remove(&key);
            return gtk::glib::ControlFlow::Break;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!(tr(
                "api.memos.error.folder_fetch_interrupted"
            ))),
        };
        state.loading_folders.borrow_mut().remove(&key);
        let result = result.and_then(|(folder, memos)| {
            let accounts = state.store.borrow().backend_accounts()?;
            if !accounts.iter().any(|current| current == &account)
                || !state
                    .store
                    .borrow()
                    .folders()?
                    .iter()
                    .any(|current| current.id == folder_id)
            {
                return Ok(());
            }
            set_account_online(&state, &account.id, true);
            state
                .store
                .borrow()
                .import_backend_snapshot(&account.id, &[folder], &memos)?;
            populate_sidebar(&state, &window)?;
            refresh_notes(&state)?;
            let selected_id = state.note_id.borrow().clone();
            if let Some(note_id) = selected_id {
                let note = state.store.borrow().note(&note_id)?;
                if let Some(note) = note {
                    show_note(&state, &note);
                }
            }
            refresh_open_note_windows(&state);
            show_next_sync_conflict(&state, &window, &account.id);
            Ok(())
        });
        let error_key = format!("fetch:folder:{folder_id}");
        if let Err(error) = result {
            handle_backend_error(
                &state,
                &account.id,
                &error_key,
                &memonotes::tr_format!(
                    "api.memos.error.folder_fetch_failed",
                    error = format!("{error:#}")
                ),
                &error,
            );
        } else {
            state.api_errors.borrow_mut().remove(&error_key);
        }
        gtk::glib::ControlFlow::Break
    });
}

fn refresh_backend_note(state: &Rc<WindowState>, window: &ApplicationWindow, note: &Note) {
    let Some(account_id) = note.account_id.as_deref() else {
        return;
    };
    if !backend_available(state, account_id) {
        return;
    }
    let account = match state.store.borrow().backend_accounts() {
        Ok(accounts) => accounts.into_iter().find(|account| {
            account.id == account_id && account.enabled && memonotes::adapters::is_supported(&account.provider)
        }),
        Err(error) => {
            show_api_error(
                state,
                "accounts",
                &memonotes::tr_format!("account.error.fetch_failed", error = format!("{error:#}")),
            );
            return;
        }
    };
    let Some(account) = account else {
        return;
    };
    match state.store.borrow().has_pending_note_events(&note.id) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            show_api_error(
                state,
                &format!("fetch:{}", note.id),
                &memonotes::tr_format!(
                    "sync.error.read_local_status_failed",
                    error = format!("{error:#}")
                ),
            );
            return;
        }
    }
    if state.loading_accounts.borrow().contains(account_id)
        || !state.loading_memos.borrow_mut().insert(note.id.clone())
    {
        return;
    }
    let remote_name = match state.store.borrow().backend_memo_name(account_id, &note.id) {
        Ok(name) => name,
        Err(error) => {
            state.loading_memos.borrow_mut().remove(&note.id);
            show_api_error(
                state,
                &format!("fetch:{}", note.id),
                &memonotes::tr_format!(
                    "api.memos.error.note_fetch_failed",
                    error = format!("{error:#}")
                ),
            );
            return;
        }
    };
    let (sender, receiver) = mpsc::channel();
    let worker_account = account.clone();
    thread::spawn(move || {
        let result = (|| -> Result<_> {
            let adapter = connect_account_adapter(&worker_account)?;
            adapter.get_memo(&remote_name)
        })();
        let _ = sender.send(result);
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let note_id = note.id.clone();
    api_timeout(state, Duration::from_millis(50), move || {
        let Some(state) = weak_state.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if weak_window.upgrade().is_none() {
            state.loading_memos.borrow_mut().remove(&note_id);
            return gtk::glib::ControlFlow::Break;
        }
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!(tr("api.memos.error.fetch_interrupted")))
            }
        };
        state.loading_memos.borrow_mut().remove(&note_id);
        let result = result.and_then(|memo| {
            let current_accounts = state.store.borrow().backend_accounts()?;
            if !current_accounts.iter().any(|current| current == &account) {
                return Ok(());
            }
            set_account_online(&state, &account.id, true);
            state
                .store
                .borrow()
                .import_backend_memo(&account.id, &memo)?;
            refresh_notes(&state)?;
            if state.note_id.borrow().as_deref() == Some(note_id.as_str()) {
                if let Some(note) = state.store.borrow().note(&note_id)? {
                    show_note(&state, &note);
                }
            }
            refresh_open_note_windows(&state);
            if let Some(window) = weak_window.upgrade() {
                show_next_sync_conflict(&state, &window, &account.id);
            }
            Ok(())
        });
        if let Err(error) = result {
            handle_backend_error(
                &state,
                &account.id,
                &format!("fetch:{note_id}"),
                &memonotes::tr_format!(
                    "api.memos.error.note_fetch_failed",
                    error = format!("{error:#}")
                ),
                &error,
            );
        } else {
            state
                .api_errors
                .borrow_mut()
                .remove(&format!("fetch:{note_id}"));
        }
        gtk::glib::ControlFlow::Break
    });
}

fn show_next_sync_conflict(state: &Rc<WindowState>, window: &ApplicationWindow, account_id: &str) {
    let conflicts = match state.store.borrow().sync_conflicts(account_id) {
        Ok(conflicts) => conflicts,
        Err(error) => {
            show_api_error(
                state,
                &format!("conflicts:{account_id}"),
                &memonotes::tr_format!(
                    "sync.error.read_local_status_failed",
                    error = format!("{error:#}")
                ),
            );
            return;
        }
    };
    let Some(conflict) = conflicts.into_iter().find(|conflict| {
        !state
            .shown_sync_conflicts
            .borrow()
            .contains(&format!("{}:{}", conflict.account_id, conflict.note_id))
    }) else {
        return;
    };
    let conflict_key = format!("{}:{}", conflict.account_id, conflict.note_id);
    state
        .shown_sync_conflicts
        .borrow_mut()
        .insert(conflict_key.clone());

    let local = if conflict.local_deleted {
        tr("sync.conflict.local_deleted").to_owned()
    } else {
        conflict_version_text(state, &SyncedNoteState::from(&conflict.local))
    };
    let remote = match conflict.remote.as_ref() {
        Some(version) => conflict_version_text(state, version),
        None => tr("sync.conflict.remote_deleted").to_owned(),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(memonotes::tr_format!(
            "sync.conflict.title",
            note = conflict.local.title
        ))
        .body(memonotes::tr_format!(
            "sync.conflict.body",
            local = local,
            remote = remote
        ))
        .build();
    dialog.add_responses(&[
        ("later", tr("action.cancel")),
        ("keep_local", tr("sync.conflict.keep_local")),
        ("keep_remote", tr("sync.conflict.keep_remote")),
    ]);
    dialog.set_default_response(Some("later"));
    dialog.set_close_response("later");
    dialog.set_response_appearance("keep_remote", adw::ResponseAppearance::Destructive);

    let state = Rc::clone(state);
    let dialog_window = window.clone();
    let next_dialog_window = window.clone();
    let account_id = conflict.account_id;
    let note_id = conflict.note_id;
    dialog.choose(
        Some(&dialog_window),
        None::<&adw::gio::Cancellable>,
        move |response| {
            let resolution = match response.as_str() {
                "keep_local" => Some(SyncConflictResolution::KeepLocal),
                "keep_remote" => Some(SyncConflictResolution::KeepRemote),
                _ => None,
            };
            if let Some(resolution) = resolution {
                let result =
                    state
                        .store
                        .borrow()
                        .resolve_sync_conflict(&account_id, &note_id, resolution);
                if let Err(error) = result {
                    show_api_error(
                        &state,
                        &format!("resolve-conflict:{conflict_key}"),
                        &memonotes::tr_format!(
                            "sync.error.resolve_conflict_failed",
                            error = format!("{error:#}")
                        ),
                    );
                } else {
                    state
                        .shown_sync_conflicts
                        .borrow_mut()
                        .remove(&conflict_key);
                    let _ = refresh_notes(&state);
                    refresh_open_note_windows(&state);
                    if state.note_id.borrow().as_deref() == Some(note_id.as_str()) {
                        if let Ok(Some(note)) = state.store.borrow().note(&note_id) {
                            show_note(&state, &note);
                        } else {
                            *state.note_id.borrow_mut() = None;
                            set_editor_text(&state, "");
                        }
                    }
                }
            }
            show_next_sync_conflict(&state, &next_dialog_window, &account_id);
        },
    );
}

fn conflict_version_text(state: &WindowState, version: &SyncedNoteState) -> String {
    let folders = state.store.borrow().folders().unwrap_or_default();
    let folder = version
        .folder_id
        .as_deref()
        .and_then(|folder_id| folders.iter().find(|folder| folder.id == folder_id))
        .map_or_else(
            || tr("folder.unfiled").to_owned(),
            |folder| folder.name.clone(),
        );
    memonotes::tr_format!(
        "sync.conflict.version",
        content = version.content.clone(),
        folder = folder,
        pinned = tr(if version.pinned {
            "sync.conflict.enabled"
        } else {
            "sync.conflict.disabled"
        }),
        archived = tr(if version.archived {
            "sync.conflict.enabled"
        } else {
            "sync.conflict.disabled"
        })
    )
}

fn install_app_menu(
    window: &ApplicationWindow,
    more_button: &gtk::MenuButton,
    state: &Rc<WindowState>,
) {
    let menu = adw::gio::Menu::new();
    menu.append(
        Some(tr("markdown.menu.import")),
        Some("win.markdown-import"),
    );
    menu.append(
        Some(tr("markdown.menu.export")),
        Some("win.markdown-export"),
    );
    menu.append(Some(tr("settings.title")), Some("win.settings"));
    menu.append(Some(tr("about.title")), Some("win.about"));
    more_button.set_menu_model(Some(&menu));

    let settings_action = adw::gio::SimpleAction::new("settings", None);
    let window_for_settings = window.clone();
    let state_for_settings = Rc::clone(state);
    settings_action.connect_activate(move |_, _| {
        let settings_dialog = adw::PreferencesDialog::builder()
            .title(tr("settings.title"))
            .content_width(940)
            .content_height(680)
            .build();

        let palette = gtk::Grid::builder()
            .column_spacing(6)
            .row_spacing(6)
            .build();
        palette.set_column_homogeneous(true);
        palette.set_row_homogeneous(true);
        let current_color = state_for_settings.note_color.borrow().clone();
        let mut first_option: Option<gtk::ToggleButton> = None;
        for (index, &(color, translation_key, _, _)) in NOTE_COLOR_PALETTE.iter().enumerate() {
            let button = gtk::ToggleButton::new();
            button.set_size_request(68, 64);
            button.set_hexpand(true);
            button.set_vexpand(true);
            button.add_css_class("note-color-option");
            button.set_tooltip_text(Some(tr(translation_key)));
            if let Some(first_option) = &first_option {
                button.set_group(Some(first_option));
            } else {
                first_option = Some(button.clone());
            }
            button.set_active(canonical_note_color(color) == canonical_note_color(&current_color));

            let option_content = gtk::Box::new(gtk::Orientation::Vertical, 5);
            option_content.set_halign(gtk::Align::Center);
            let swatch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            swatch.set_size_request(30, 30);
            swatch.set_halign(gtk::Align::Center);
            swatch.set_valign(gtk::Align::Center);
            swatch.add_css_class("note-color-swatch");
            swatch.add_css_class(&format!("swatch-{color}"));
            option_content.append(&swatch);
            let color_label = gtk::Label::new(Some(tr(translation_key)));
            color_label.set_width_chars(9);
            color_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            option_content.append(&color_label);
            button.set_child(Some(&option_content));

            let state_for_color = Rc::clone(&state_for_settings);
            button.connect_toggled(move |button| {
                if button.is_active() {
                    set_note_color(&state_for_color, color);
                }
            });
            palette.attach(&button, (index % 6) as i32, (index / 6) as i32, 1, 1);
        }

        let accounts_page = build_accounts_page(&window_for_settings, &state_for_settings);
        settings_dialog.add(&accounts_page);

        let appearance_page = adw::PreferencesPage::builder()
            .title(tr("settings.appearance.title"))
            .icon_name("applications-graphics-symbolic")
            .build();
        let appearance_group = adw::PreferencesGroup::builder()
            .title(tr("settings.appearance.note_color"))
            .build();
        appearance_group.add(&palette);
        appearance_page.add(&appearance_group);
        settings_dialog.add(&appearance_page);

        let general_page = adw::PreferencesPage::builder()
            .title(tr("settings.general.title"))
            .icon_name("preferences-system-symbolic")
            .build();
        general_page.add(&build_language_group(&state_for_settings));
        settings_dialog.add(&general_page);

        let shortcuts_page = build_shortcuts_page();
        settings_dialog.add(&shortcuts_page);
        settings_dialog.present(Some(&window_for_settings));
    });
    window.add_action(&settings_action);

    let about_action = adw::gio::SimpleAction::new("about", None);
    let window_for_about = window.clone();
    about_action.connect_activate(move |_, _| {
        show_about_window(&window_for_about);
    });
    window.add_action(&about_action);

    let import_action = adw::gio::SimpleAction::new("markdown-import", None);
    let window_for_import = window.clone();
    let state_for_import = Rc::clone(state);
    import_action.connect_activate(move |_, _| {
        choose_markdown_import(&state_for_import, &window_for_import);
    });
    window.add_action(&import_action);

    let export_action = adw::gio::SimpleAction::new("markdown-export", None);
    let window_for_export = window.clone();
    let state_for_export = Rc::clone(state);
    export_action.connect_activate(move |_, _| {
        choose_markdown_export(&state_for_export, &window_for_export);
    });
    window.add_action(&export_action);
}

fn markdown_account_choices(state: &WindowState) -> Result<Vec<(Option<String>, String)>> {
    let mut choices = vec![(None, tr("sidebar.on_this_device").to_owned())];
    choices.extend(
        state
            .store
            .borrow()
            .backend_accounts()?
            .into_iter()
            .filter(|account| memonotes::adapters::is_supported(&account.provider))
            .map(|account| (Some(account.id), account.label)),
    );
    Ok(choices)
}

fn choose_markdown_import(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let chooser = gtk::FileChooserNative::new(
        Some(tr("markdown.import.title")),
        Some(window),
        gtk::FileChooserAction::Open,
        Some(tr("markdown.import.choose")),
        Some(tr("action.cancel")),
    );
    chooser.set_select_multiple(true);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Markdown"));
    filter.add_mime_type("text/markdown");
    filter.add_pattern("*.md");
    filter.add_pattern("*.MD");
    chooser.add_filter(&filter);

    let state = Rc::clone(state);
    let window = window.clone();
    chooser.connect_response(move |chooser, response| {
        if response != gtk::ResponseType::Accept {
            chooser.hide();
            return;
        }
        let files = chooser.files();
        let mut paths = Vec::new();
        for index in 0..files.n_items() {
            if let Some(path) = files
                .item(index)
                .and_downcast::<gio::File>()
                .and_then(|file| file.path())
            {
                paths.push(path);
            }
        }
        chooser.hide();
        if paths.is_empty() {
            return;
        }
        show_markdown_import_destination(&state, &window, paths);
    });
    chooser.show();
}

fn update_markdown_folder_choices(
    state: &WindowState,
    account_id: Option<&str>,
    model: &gtk::StringList,
    options: &Rc<RefCell<Vec<Option<String>>>>,
) -> Result<()> {
    let folders: Vec<_> = state
        .store
        .borrow()
        .folders()?
        .into_iter()
        .filter(|folder| folder.account_id.as_deref() == account_id)
        .collect();
    let mut labels = vec![tr("folder.unfiled").to_owned()];
    let mut folder_ids = vec![None];
    for folder in folders {
        labels.push(folder.name);
        folder_ids.push(Some(folder.id));
    }
    let labels: Vec<_> = labels.iter().map(String::as_str).collect();
    model.splice(0, model.n_items(), &labels);
    options.replace(folder_ids);
    Ok(())
}

fn show_markdown_import_destination(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    paths: Vec<PathBuf>,
) {
    let accounts = match markdown_account_choices(state) {
        Ok(accounts) => accounts,
        Err(error) => {
            state.toast_overlay.add_toast(adw::Toast::new(&format!(
                "{}: {error:#}",
                tr("markdown.error.accounts")
            )));
            return;
        }
    };
    let dialog = gtk::Dialog::builder()
        .modal(true)
        .transient_for(window)
        .title(tr("markdown.import.destination"))
        .build();
    dialog.add_button(tr("action.cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(tr("markdown.import"), gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_spacing(12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let account_labels: Vec<_> = accounts.iter().map(|(_, label)| label.as_str()).collect();
    let account_model = gtk::StringList::new(&account_labels);
    let account_dropdown = gtk::DropDown::new(Some(account_model), None::<&gtk::Expression>);
    let folder_model = gtk::StringList::new(&[tr("folder.unfiled")]);
    let folder_dropdown = gtk::DropDown::new(Some(folder_model.clone()), None::<&gtk::Expression>);
    let folder_options = Rc::new(RefCell::new(vec![None]));
    if let Some((account_id, _)) = accounts.first()
        && let Err(error) = update_markdown_folder_choices(
            state,
            account_id.as_deref(),
            &folder_model,
            &folder_options,
        )
    {
        state
            .toast_overlay
            .add_toast(adw::Toast::new(&format!("{error:#}")));
    }
    content.append(&markdown_dialog_row(
        tr("markdown.destination.account"),
        &account_dropdown,
    ));
    content.append(&markdown_dialog_row(
        tr("markdown.destination.folder"),
        &folder_dropdown,
    ));

    let folders_for_account = Rc::clone(&folder_options);
    let state_for_account = Rc::clone(state);
    let folder_model_for_account = folder_model.clone();
    let folder_dropdown_for_account = folder_dropdown.clone();
    let accounts_for_account = accounts.clone();
    account_dropdown.connect_selected_notify(move |dropdown| {
        let Some((account_id, _)) = accounts_for_account.get(dropdown.selected() as usize) else {
            return;
        };
        if update_markdown_folder_choices(
            &state_for_account,
            account_id.as_deref(),
            &folder_model_for_account,
            &folders_for_account,
        )
        .is_ok()
        {
            folder_dropdown_for_account.set_selected(0);
        }
    });

    let state = Rc::clone(state);
    let window = window.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            let account_index = account_dropdown.selected() as usize;
            if let Some((account_id, account_label)) = accounts.get(account_index) {
                let folder_id = folder_options
                    .borrow()
                    .get(folder_dropdown.selected() as usize)
                    .cloned()
                    .flatten();
                start_markdown_import(
                    &state,
                    &window,
                    paths.clone(),
                    account_id.clone(),
                    account_label.clone(),
                    folder_id,
                );
            }
        }
        dialog.close();
    });
    dialog.present();
}

fn markdown_dialog_row(label: &str, dropdown: &gtk::DropDown) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    row.append(&gtk::Label::new(Some(label)));
    row.append(dropdown);
    row
}

fn start_markdown_import(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    paths: Vec<PathBuf>,
    account_id: Option<String>,
    account_label: String,
    folder_id: Option<String>,
) {
    state
        .toast_overlay
        .add_toast(adw::Toast::new(&memonotes::tr_format!(
            "markdown.import.started",
            count = paths.len().to_string(),
            account = account_label.clone()
        )));
    let database = match database_path() {
        Ok(path) => path,
        Err(error) => {
            state
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("{error:#}")));
            return;
        }
    };
    let (sender, receiver) = mpsc::channel();
    let worker_account_id = account_id.clone();
    thread::spawn(move || {
        let result = (|| -> Result<Vec<String>> {
            let markdown = markdown_io::read_markdown_files(&paths)?;
            Store::open(database)?.import_markdown_notes(
                worker_account_id.as_deref(),
                folder_id.as_deref(),
                &markdown,
            )
        })();
        let _ = sender.send(result);
    });

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        match receiver.try_recv() {
            Ok(Ok(ids)) => {
                state
                    .active_filter
                    .replace(ActiveNoteFilter::All(account_id.clone()));
                state.search.set_text("");
                if let Err(error) =
                    populate_sidebar(&state, &window).and_then(|()| refresh_notes(&state))
                {
                    state
                        .toast_overlay
                        .add_toast(adw::Toast::new(&format!("{error:#}")));
                }
                if let Some(id) = ids.first()
                    && let Ok(Some(note)) = state.store.borrow().note(id)
                {
                    show_note(&state, &note);
                }
                if let Some(account_id) = &account_id {
                    state
                        .upload_after
                        .borrow_mut()
                        .insert(account_id.clone(), Instant::now());
                }
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(&memonotes::tr_format!(
                        "markdown.import.complete",
                        count = ids.len().to_string(),
                        account = account_label.clone()
                    )));
                gtk::glib::ControlFlow::Break
            }
            Ok(Err(error)) => {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(&memonotes::tr_format!(
                        "markdown.import.failed",
                        error = format!("{error:#}")
                    )));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(tr("markdown.import.interrupted")));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
        }
    });
}

fn choose_markdown_export(state: &Rc<WindowState>, window: &ApplicationWindow) {
    let accounts = match markdown_account_choices(state) {
        Ok(accounts) => accounts,
        Err(error) => {
            state
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("{error:#}")));
            return;
        }
    };
    let dialog = gtk::Dialog::builder()
        .modal(true)
        .transient_for(window)
        .title(tr("markdown.export.account"))
        .build();
    dialog.add_button(tr("action.cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(tr("action.continue"), gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);
    let labels: Vec<_> = accounts.iter().map(|(_, label)| label.as_str()).collect();
    let model = gtk::StringList::new(&labels);
    let dropdown = gtk::DropDown::new(Some(model), None::<&gtk::Expression>);
    let content = dialog.content_area();
    content.set_spacing(12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&markdown_dialog_row(
        tr("markdown.destination.account"),
        &dropdown,
    ));
    let state = Rc::clone(state);
    let window = window.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept
            && let Some((account_id, account_label)) = accounts.get(dropdown.selected() as usize)
        {
            choose_markdown_export_path(&state, &window, account_id.clone(), account_label.clone());
        }
        dialog.close();
    });
    dialog.present();
}

fn choose_markdown_export_path(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    account_id: Option<String>,
    account_label: String,
) {
    let chooser = gtk::FileChooserNative::new(
        Some(tr("markdown.export.title")),
        Some(window),
        gtk::FileChooserAction::Save,
        Some(tr("markdown.export.save")),
        Some(tr("action.cancel")),
    );
    chooser.set_current_name("memonotes-export.zip");
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("ZIP archive"));
    filter.add_pattern("*.zip");
    chooser.add_filter(&filter);
    let state = Rc::clone(state);
    let window = window.clone();
    chooser.connect_response(move |chooser, response| {
        if response == gtk::ResponseType::Accept
            && let Some(destination) = chooser.file().and_then(|file| file.path())
        {
            start_markdown_export(
                &state,
                &window,
                account_id.clone(),
                account_label.clone(),
                destination,
            );
        }
        chooser.hide();
    });
    chooser.show();
}

fn start_markdown_export(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    account_id: Option<String>,
    account_label: String,
    destination: PathBuf,
) {
    state
        .toast_overlay
        .add_toast(adw::Toast::new(&memonotes::tr_format!(
            "markdown.export.started",
            account = account_label.clone()
        )));
    let database = match database_path() {
        Ok(path) => path,
        Err(error) => {
            state
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("{error:#}")));
            return;
        }
    };
    let (sender, receiver) = mpsc::channel();
    let worker_destination = destination.clone();
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            let store = Store::open(database)?;
            let image_directory = memonotes::images::image_directory()?;
            markdown_io::export_account_zip(
                &store,
                account_id.as_deref(),
                &account_label,
                &image_directory,
                &worker_destination,
            )
        })();
        let _ = sender.send(result);
    });

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        let (Some(state), Some(_window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(&memonotes::tr_format!(
                        "markdown.export.complete",
                        path = destination.display().to_string()
                    )));
                gtk::glib::ControlFlow::Break
            }
            Ok(Err(error)) => {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(&memonotes::tr_format!(
                        "markdown.export.failed",
                        error = format!("{error:#}")
                    )));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(tr("markdown.export.interrupted")));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
        }
    });
}

fn language_selection(preference: Option<&str>) -> u32 {
    preference
        .and_then(|language| {
            memonotes::i18n::LANGUAGES
                .iter()
                .position(|candidate| *candidate == language)
        })
        .map_or(0, |index| index as u32 + 1)
}

fn build_language_group(state: &Rc<WindowState>) -> adw::PreferencesGroup {
    let preference = state
        .store
        .borrow()
        .language_preference()
        .unwrap_or_default();
    let startup_preference = memonotes::i18n::startup_language_preference();
    let option_keys = [
        "settings.language.system_default",
        "language.en",
        "language.de",
        "language.fr",
        "language.nl",
        "language.es",
    ];
    let labels: Vec<_> = option_keys.iter().map(|key| tr(key)).collect();
    let model = gtk::StringList::new(&labels);
    let row = adw::ComboRow::builder()
        .title(tr("settings.language.title"))
        .model(&model)
        .selected(language_selection(preference.as_deref()))
        .build();
    let restart_notice = adw::ActionRow::builder()
        .title(tr("settings.language.restart_required"))
        .visible(preference.as_deref() != startup_preference)
        .build();
    restart_notice.add_css_class("dim-label");
    let group = adw::PreferencesGroup::new();
    group.add(&row);
    group.add(&restart_notice);
    let previous_selection = Cell::new(row.selected());
    let state = Rc::clone(state);
    row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected == previous_selection.get() {
            return;
        }
        let preference = selected
            .checked_sub(1)
            .and_then(|index| memonotes::i18n::LANGUAGES.get(index as usize))
            .copied();
        if selected > memonotes::i18n::LANGUAGES.len() as u32 {
            return;
        }
        let result = state.store.borrow().set_language_preference(preference);
        if let Err(error) = result {
            row.set_selected(previous_selection.get());
            state
                .toast_overlay
                .add_toast(adw::Toast::new(&memonotes::tr_format!(
                    "settings.language.error.save_failed",
                    error = format!("{error:#}")
                )));
            return;
        }
        previous_selection.set(selected);
        restart_notice.set_visible(preference != startup_preference);
    });
    group
}

fn build_shortcuts_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title(tr("settings.shortcuts.title"))
        .icon_name("preferences-desktop-keyboard-shortcuts-symbolic")
        .build();

    let notes_group = adw::PreferencesGroup::builder()
        .title(tr("settings.shortcuts.group.notes"))
        .build();
    notes_group.add(&shortcut_row("settings.shortcuts.new_note", "<Control>n"));
    notes_group.add(&shortcut_row(
        "settings.shortcuts.new_folder",
        "<Control><Shift>n",
    ));
    notes_group.add(&shortcut_row("settings.shortcuts.undo", "<Control>z"));
    notes_group.add(&shortcut_row("settings.shortcuts.delete_note", "Delete"));
    page.add(&notes_group);

    let navigation_group = adw::PreferencesGroup::builder()
        .title(tr("settings.shortcuts.group.navigation"))
        .build();
    navigation_group.add(&shortcut_row("settings.shortcuts.search", "<Control>f"));
    navigation_group.add(&shortcut_row("settings.shortcuts.previous_note", "Up"));
    navigation_group.add(&shortcut_row("settings.shortcuts.next_note", "Down"));
    navigation_group.add(&shortcut_row("settings.shortcuts.focus_editor", "Tab"));
    navigation_group.add(&shortcut_row("settings.shortcuts.clear_search", "Escape"));
    page.add(&navigation_group);

    let application_group = adw::PreferencesGroup::builder()
        .title(tr("settings.shortcuts.group.application"))
        .build();
    application_group.add(&shortcut_row(
        "settings.shortcuts.close_window",
        "<Control>w",
    ));
    application_group.add(&shortcut_row("settings.shortcuts.quit", "<Control>q"));
    page.add(&application_group);

    page
}

fn shortcut_row(title: &str, accelerator: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(tr(title)).build();
    let shortcut = gtk::ShortcutLabel::new(accelerator);
    shortcut.set_valign(gtk::Align::Center);
    row.add_suffix(&shortcut);
    row.set_activatable(false);
    row
}

#[cfg(test)]
mod language_settings_tests {
    use super::*;

    #[test]
    fn selection_matches_system_default_and_supported_languages() {
        assert_eq!(language_selection(None), 0);
        assert_eq!(language_selection(Some("unknown")), 0);
        for (index, language) in memonotes::i18n::LANGUAGES.iter().enumerate() {
            assert_eq!(language_selection(Some(language)), index as u32 + 1);
        }
    }

    #[test]
    fn retry_backoff_grows_and_caps_at_fifteen_minutes() {
        assert_eq!(sync_retry_delay(0), Duration::from_secs(30));
        assert_eq!(sync_retry_delay(1), Duration::from_secs(60));
        assert_eq!(sync_retry_delay(2), Duration::from_secs(120));
        assert_eq!(sync_retry_delay(5), Duration::from_secs(900));
        assert_eq!(sync_retry_delay(20), Duration::from_secs(900));
    }

    #[test]
    fn outbox_revision_must_be_compared_before_upload() {
        assert!(outbox_revision_needs_refresh(None, 0));
        assert!(outbox_revision_needs_refresh(Some(8), 9));
        assert!(!outbox_revision_needs_refresh(Some(9), 9));
        assert!(!outbox_revision_needs_refresh(Some(10), 9));
    }

    fn find_language_row(root: &gtk::Widget) -> Option<adw::ComboRow> {
        let mut pending = vec![root.clone()];
        while let Some(widget) = pending.pop() {
            if let Some(row) = widget.downcast_ref::<adw::ComboRow>() {
                if row.title() == tr("settings.language.title") {
                    return Some(row.clone());
                }
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                pending.push(widget);
            }
        }
        None
    }

    fn find_shortcuts_page(root: &gtk::Widget) -> Option<adw::PreferencesPage> {
        let mut pending = vec![root.clone()];
        while let Some(widget) = pending.pop() {
            if let Some(page) = widget.downcast_ref::<adw::PreferencesPage>() {
                if page.title() == tr("settings.shortcuts.title") {
                    return Some(page.clone());
                }
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                pending.push(widget);
            }
        }
        None
    }

    #[test]
    fn language_picker_saves_and_reopens_in_isolated_process() -> Result<()> {
        if std::env::var_os("MEMONOTES_LANGUAGE_SETTINGS_TEST").is_none() {
            let directory = tempfile::tempdir()?;
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child.args(["--exact", "language_settings_tests::language_picker_saves_and_reopens_in_isolated_process", "--test-threads=1"])
                .env("MEMONOTES_LANGUAGE_SETTINGS_TEST", "1")
                .env("XDG_DATA_HOME", directory.path())
                .env("MEMONOTES_LANGUAGE", "en");
            assert!(child.status()?.success());
            Store::open(directory.path().join("memonotes/notes.sqlite3"))?
                .set_language_preference(Some("nl"))?;
            assert!(
                child
                    .env("MEMONOTES_LANGUAGE_SETTINGS_TEST", "restart")
                    .status()?
                    .success()
            );
            return Ok(());
        }
        gtk::init()?;
        let application = Application::builder()
            .application_id("app.memonotes.LanguageSettingsTest")
            .flags(adw::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&adw::gio::Cancellable>)?;
        build_window(&application)?;
        let window = application
            .windows()
            .into_iter()
            .next()
            .unwrap()
            .downcast::<ApplicationWindow>()
            .unwrap();
        adw::gio::prelude::ActionGroupExt::activate_action(&window, "settings", None);
        let dialog = window.visible_dialog().unwrap();
        let row = find_language_row(dialog.upcast_ref()).unwrap();
        assert!(find_shortcuts_page(dialog.upcast_ref()).is_some());
        assert_eq!(row.model().unwrap().n_items(), 6);
        if std::env::var("MEMONOTES_LANGUAGE_SETTINGS_TEST").as_deref() == Ok("restart") {
            assert_eq!(row.selected(), 4);
            assert_eq!(memonotes::i18n::language(), "nl");
            row.set_selected(0);
            assert_eq!(Store::open(database_path()?)?.language_preference()?, None);
            assert_eq!(memonotes::i18n::language(), "nl");
            dialog.close();
            window.destroy();
            return Ok(());
        }
        assert_eq!(row.selected(), 0);
        for (index, language) in memonotes::i18n::LANGUAGES.iter().enumerate() {
            row.set_selected(index as u32 + 1);
            assert_eq!(
                Store::open(database_path()?)?
                    .language_preference()?
                    .as_deref(),
                Some(*language)
            );
            assert_eq!(memonotes::i18n::language(), "en");
        }
        dialog.close();
        adw::gio::prelude::ActionGroupExt::activate_action(&window, "settings", None);
        let dialog = window.visible_dialog().unwrap();
        let row = find_language_row(dialog.upcast_ref()).unwrap();
        assert_eq!(row.selected(), 5);
        row.set_selected(0);
        assert_eq!(Store::open(database_path()?)?.language_preference()?, None);
        dialog.close();
        window.destroy();
        Ok(())
    }
}

fn show_about_window(parent: &ApplicationWindow) {
    let about_window = gtk::Window::builder()
        .title("Memonotes")
        .transient_for(parent)
        .modal(true)
        .default_width(360)
        .default_height(640)
        .build();
    about_window.set_size_request(360, -1);
    about_window.add_css_class("about");

    let header = HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.add_css_class("flat");
    header.set_title_widget(Some(&gtk::Label::new(None)));
    let start_controls = gtk::WindowControls::new(gtk::PackType::Start);
    header.pack_start(&start_controls);
    let close_controls = gtk::WindowControls::new(gtk::PackType::End);
    header.pack_end(&close_controls);

    let about_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let app_icon = about_app_icon();
    app_icon.set_pixel_size(128);
    app_icon.add_css_class("icon-dropshadow");
    app_icon.set_halign(gtk::Align::Center);
    about_content.append(&app_icon);

    let identity = gtk::Grid::builder().row_spacing(0).hexpand(true).build();
    identity.set_margin_bottom(12);
    let app_name = gtk::Label::builder()
        .label("Memonotes")
        .css_classes(["title-1"])
        .hexpand(true)
        .xalign(0.5)
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    identity.attach(&app_name, 0, 0, 1, 1);
    let developer_name = gtk::Label::builder()
        .label("Rob van Eck")
        .hexpand(true)
        .xalign(0.5)
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    developer_name.set_margin_bottom(8);
    identity.attach(&developer_name, 0, 1, 1, 1);
    let version = gtk::Button::with_label(env!("CARGO_PKG_VERSION"));
    version.set_halign(gtk::Align::Center);
    version.add_css_class("app-version");
    identity.attach(&version, 0, 2, 1, 1);
    about_content.append(&identity);

    let links_group = adw::PreferencesGroup::new();
    links_group.add(&about_link_row(
        "GitHub",
        "https://github.com/robvaneck/memonotes",
    ));
    about_content.append(&links_group);

    let credits_group = adw::PreferencesGroup::new();
    credits_group.add(
        &adw::ActionRow::builder()
            .title(tr("about.developer_credit"))
            .build(),
    );
    let bitcoin_address = "bc1qakjs0c48rm2860v9v3xnk8n6q3ry3u8vhmfwpg";
    let bitcoin_row = adw::ActionRow::builder()
        .title(tr("about.bitcoin_wallet"))
        .subtitle(bitcoin_address)
        .subtitle_selectable(true)
        .build();
    let copy_button = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy_button.set_tooltip_text(Some(tr("about.copy_bitcoin_address")));
    copy_button.add_css_class("flat");
    copy_button.set_valign(gtk::Align::Center);
    let clipboard = gtk::prelude::WidgetExt::display(&about_window).clipboard();
    copy_button.connect_clicked(move |_| {
        clipboard.set_text(bitcoin_address);
    });
    bitcoin_row.add_suffix(&copy_button);
    credits_group.add(&bitcoin_row);
    about_content.append(&credits_group);

    let legal_group = adw::PreferencesGroup::new();
    legal_group.add(&about_link_row(
        "GNU GPL-3.0-or-later",
        "https://github.com/robvaneck/memonotes/blob/main/LICENSE",
    ));
    about_content.append(&legal_group);

    let about_clamp = adw::Clamp::builder().child(&about_content).build();
    let about_scroll = gtk::ScrolledWindow::builder().child(&about_clamp).build();
    about_scroll.add_css_class("main-page");

    about_window.set_titlebar(Some(&header));
    about_window.set_child(Some(&about_scroll));
    about_window.present();
}

const APP_ID: &str = "io.github.robvaneck.Memonotes";

// An installed build has the icon in the theme; a source checkout falls back to the file.
fn about_app_icon() -> gtk::Image {
    let installed = gtk::gdk::Display::default()
        .is_some_and(|display| gtk::IconTheme::for_display(&display).has_icon(APP_ID));
    if installed {
        gtk::Image::from_icon_name(APP_ID)
    } else {
        gtk::Image::from_file(concat!(env!("CARGO_MANIFEST_DIR"), "/icon.svg"))
    }
}

fn about_link_row(title: &str, uri: &str) -> adw::ActionRow {
    let button = gtk::Button::builder()
        .icon_name("adw-external-link-symbolic")
        .tooltip_text(memonotes::tr_format!("link.open", title = title))
        .valign(gtk::Align::Center)
        .build();
    button.add_css_class("flat");

    let uri = uri.to_owned();
    button.connect_clicked(move |_| {
        if let Err(error) =
            adw::gio::AppInfo::launch_default_for_uri(&uri, None::<&adw::gio::AppLaunchContext>)
        {
            eprintln!(
                "{}",
                memonotes::tr_format!("link.error.open_failed", error = error)
            );
        }
    });

    let row = adw::ActionRow::builder()
        .title(title)
        .activatable_widget(&button)
        .build();
    row.add_suffix(&button);
    row
}

fn build_accounts_page(
    parent: &ApplicationWindow,
    state: &Rc<WindowState>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title(tr("account.list.title"))
        .icon_name("network-server-symbolic")
        .build();
    let add_row = adw::ButtonRow::builder()
        .title(tr("account.add"))
        .start_icon_name("list-add-symbolic")
        .build();
    let accounts_group = adw::PreferencesGroup::builder()
        .title(tr("account.list.title"))
        .build();
    let account_rows = AccountRows::new(&accounts_group, parent, &add_row);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("dim-label");
    status.set_visible(false);

    accounts_group.add(&status);
    refresh_backend_accounts(&account_rows, state, &status);
    page.add(&accounts_group);

    let parent_for_add_row = parent.clone();
    let state_for_add_row = Rc::clone(state);
    let rows_for_add_row = account_rows.clone();
    let status_for_add_row = status.clone();
    add_row.connect_activated(move |_| {
        open_account_editor(
            &parent_for_add_row,
            &state_for_add_row,
            &rows_for_add_row,
            &status_for_add_row,
            None,
        );
    });

    let security_group = adw::PreferencesGroup::builder()
        .title(tr("settings.security.title"))
        .build();
    let security_info = gtk::Label::new(Some(tr("settings.security.token_storage_notice")));
    security_info.set_xalign(0.0);
    security_info.set_wrap(true);
    security_info.add_css_class("dim-label");
    security_group.add(&security_info);
    page.add(&security_group);
    page
}

fn refresh_backend_accounts(rows: &AccountRows, state: &Rc<WindowState>, status: &gtk::Label) {
    let Some(group) = rows.group.upgrade() else {
        return;
    };
    if rows.add_row_attached.replace(false) {
        group.remove(&rows.add_row);
    }
    for row in rows
        .rows
        .borrow_mut()
        .drain(..)
        .filter_map(|row| row.upgrade())
    {
        group.remove(&row);
    }
    match state.store.borrow().backend_accounts() {
        Ok(accounts) if accounts.is_empty() => {
            status.set_text("");
            status.set_visible(false);
        }
        Ok(accounts) => {
            status.set_text("");
            status.set_visible(false);
            for account in accounts {
                append_backend_account(rows, state, status, account);
            }
        }
        Err(error) => {
            status.set_text(&memonotes::tr_format!(
                "account.error.load_failed",
                error = format!("{error:#}")
            ));
            status.set_visible(true);
        }
    }
    group.add(&rows.add_row);
    rows.add_row_attached.set(true);
}

fn append_backend_account(
    rows: &AccountRows,
    state: &Rc<WindowState>,
    status: &gtk::Label,
    account: BackendAccount,
) {
    let provider_name = memonotes::adapters::provider_name(&account.provider);
    let row = adw::SwitchRow::builder()
        .title(format!("{} · {}", provider_name, account.label))
        .subtitle(&account.server_url)
        .subtitle_lines(1)
        .active(account.enabled)
        .build();

    let state_for_toggle = Rc::clone(state);
    let parent_for_toggle = rows.parent.clone();
    let account_id_for_toggle = account.id.clone();
    let status_for_toggle = status.clone();
    let restoring = Rc::new(Cell::new(false));
    let restoring_for_toggle = Rc::clone(&restoring);
    row.connect_active_notify(move |row| {
        if restoring_for_toggle.get() {
            return;
        }
        let enabled = row.is_active();
        if let Err(error) = state_for_toggle
            .store
            .borrow()
            .set_backend_account_enabled(&account_id_for_toggle, enabled)
        {
            status_for_toggle.set_text(&memonotes::tr_format!(
                "account.error.change_status_failed",
                error = format!("{error:#}")
            ));
            status_for_toggle.set_visible(true);
            restoring_for_toggle.set(true);
            row.set_active(!enabled);
            restoring_for_toggle.set(false);
        } else {
            status_for_toggle.set_visible(false);
            if enabled {
                if let Some(window) = parent_for_toggle.upgrade() {
                    load_backend_accounts(&state_for_toggle, &window);
                }
            }
        }
    });

    let details_button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text(tr("account.edit_details"))
        .valign(gtk::Align::Center)
        .build();
    details_button.add_css_class("flat");
    let state_for_details = Rc::clone(state);
    let rows_for_details = rows.clone();
    let status_for_details = status.clone();
    let account_for_details = account.clone();
    details_button.connect_clicked(move |_| {
        if let Some(parent) = rows_for_details.parent.upgrade() {
            open_account_editor(
                &parent,
                &state_for_details,
                &rows_for_details,
                &status_for_details,
                Some(account_for_details.clone()),
            );
        }
    });

    let remove_button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text(tr("account.remove"))
        .valign(gtk::Align::Center)
        .build();
    remove_button.add_css_class("flat");
    let state_for_remove = Rc::clone(state);
    let rows_for_remove = rows.clone();
    let status_for_remove = status.clone();
    remove_button.connect_clicked(move |button| {
        let Some(parent) = button
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok())
        else {
            return;
        };
        let pending = state_for_remove
            .store
            .borrow()
            .pending_event_count(&account.id)
            .unwrap_or(0);
        let alert = if pending > 0 {
            adw::AlertDialog::builder()
                .heading(tr("account.remove.pending_title"))
                .body(memonotes::tr_format!(
                    "account.remove.pending_message",
                    account = account.label,
                    count = pending.to_string()
                ))
                .build()
        } else {
            adw::AlertDialog::builder()
                .heading(tr("account.remove.confirm_title"))
                .body(memonotes::tr_format!(
                    "account.remove.confirm_message",
                    account = account.label
                ))
                .build()
        };
        alert.add_responses(&[
            ("cancel", tr("action.cancel")),
            (
                "delete",
                if pending > 0 {
                    tr("account.remove.anyway")
                } else {
                    tr("action.delete")
                },
            ),
        ]);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

        let state = Rc::clone(&state_for_remove);
        let rows = rows_for_remove.clone();
        let status = status_for_remove.clone();
        let account_for_confirmation = account.clone();
        alert.choose(
            Some(&parent),
            None::<&adw::gio::Cancellable>,
            move |response| {
                if response == "delete" {
                    remove_backend_account(&state, &rows, &status, account_for_confirmation);
                }
            },
        );
    });

    row.add_suffix(&details_button);
    row.add_suffix(&remove_button);
    if let Some(group) = rows.group.upgrade() {
        group.add(&row);
        let widget: gtk::Widget = row.clone().upcast();
        rows.rows.borrow_mut().push(widget.downgrade());
    }
}

fn open_account_editor(
    parent: &ApplicationWindow,
    state: &Rc<WindowState>,
    rows: &AccountRows,
    list_status: &gtk::Label,
    account: Option<BackendAccount>,
) {
    let creating = account.is_none();
    let heading = if creating {
        tr("account.add")
    } else {
        tr("account.details.title")
    };
    let subtitle = account
        .as_ref()
        .map_or(tr("account.connect_server"), |account| {
            account.server_url.as_str()
        });
    let dialog = gtk::Dialog::builder()
        .title(heading)
        .transient_for(parent)
        .modal(true)
        .default_width(620)
        .default_height(520)
        .build();
    dialog.add_css_class("account-editor");
    let window_title = adw::WindowTitle::new(heading, subtitle);
    let titlebar = adw::HeaderBar::builder()
        .title_widget(&window_title)
        .show_start_title_buttons(false)
        .show_end_title_buttons(true)
        .build();
    dialog.set_titlebar(Some(&titlebar));
    dialog.add_button(
        if creating {
            tr("action.cancel")
        } else {
            tr("action.close")
        },
        gtk::ResponseType::Cancel,
    );
    dialog.add_button(
        if creating {
            tr("account.connect_and_add")
        } else {
            tr("action.edit")
        },
        gtk::ResponseType::Accept,
    );
    dialog.set_default_response(gtk::ResponseType::Accept);

    let account_state = Rc::new(RefCell::new(account.clone()));
    let editing = Rc::new(Cell::new(creating));
    let providers = gtk::StringList::new(
        &memonotes::adapters::PROVIDERS
            .iter()
            .map(|info| info.name)
            .collect::<Vec<_>>(),
    );
    let provider_index = account.as_ref().map_or(0, |account| {
        memonotes::adapters::PROVIDERS
            .iter()
            .position(|info| info.id == account.provider)
            .unwrap_or(0) as u32
    });
    let provider_row = adw::ComboRow::builder()
        .title(tr("account.field.provider"))
        .model(&providers)
        .selected(provider_index)
        .sensitive(creating)
        .build();
    let account_name_row = adw::EntryRow::builder()
        .title(tr("account.field.name"))
        .text(
            account
                .as_ref()
                .map_or("", |account| account.label.as_str()),
        )
        .build();
    let imap_account = account
        .as_ref()
        .filter(|account| account.provider == "imap")
        .and_then(|account| memonotes::imap_notes::ImapAccount::parse(&account.server_url).ok());
    let server_row = adw::EntryRow::builder()
        .title(tr("account.field.server_url"))
        .text(imap_account.as_ref().map_or_else(
            || {
                account
                    .as_ref()
                    .map_or("", |account| account.server_url.as_str())
            },
            |imap| imap.host.as_str(),
        ))
        .build();
    let imap_port_row = adw::EntryRow::builder()
        .title(tr("account.field.imap_port"))
        .text(
            imap_account
                .as_ref()
                .map_or(String::new(), |imap| imap.port.to_string()),
        )
        .build();
    let imap_security_row = adw::ComboRow::builder()
        .title(tr("account.field.imap_security"))
        .model(&gtk::StringList::new(&[
            tr("account.imap.security.tls"),
            tr("account.imap.security.starttls"),
        ]))
        .selected(imap_account.as_ref().is_some_and(|imap| imap.starttls) as u32)
        .build();
    let imap_username_row = adw::EntryRow::builder()
        .title(tr("account.field.imap_username"))
        .text(imap_account.as_ref().map_or("", |imap| imap.username.as_str()))
        .build();
    let imap_rows = ImapFormRows {
        port: imap_port_row,
        security: imap_security_row,
        username: imap_username_row,
    };
    let token_row = adw::PasswordEntryRow::builder()
        .title(if creating {
            tr("account.field.personal_access_token")
        } else {
            tr("account.field.replace_token_optional")
        })
        .build();
    let token_info = adw::ActionRow::builder()
        .title(tr("account.field.access_token"))
        .subtitle(tr("account.token_stored_securely"))
        .build();
    token_info.set_visible(!creating);
    token_row.set_visible(creating);
    let advanced_row = adw::ExpanderRow::builder()
        .title(tr("account.connection.advanced"))
        .expanded(false)
        .build();
    let allow_http_row = adw::SwitchRow::builder()
        .title(tr("account.connection.allow_insecure_http"))
        .subtitle(tr("account.connection.trusted_network_notice"))
        .active(
            account
                .as_ref()
                .is_some_and(|account| account.server_url.starts_with("http://")),
        )
        .build();
    advanced_row.add_row(&allow_http_row);
    let error_banner = adw::Banner::builder().revealed(false).build();
    error_banner.add_css_class("account-error-banner");

    let form_group = adw::PreferencesGroup::builder()
        .title(tr("account.connection.title"))
        .build();
    form_group.set_margin_top(24);
    form_group.set_margin_bottom(24);
    form_group.set_margin_start(24);
    form_group.set_margin_end(24);
    form_group.add(&provider_row);
    form_group.add(&account_name_row);
    form_group.add(&server_row);
    form_group.add(&imap_rows.port);
    form_group.add(&imap_rows.security);
    form_group.add(&imap_rows.username);
    form_group.add(&token_info);
    form_group.add(&token_row);
    form_group.add(&advanced_row);
    let apply_provider_labels = {
        let server_row = server_row.clone();
        let token_row = token_row.clone();
        let advanced_row = advanced_row.clone();
        let imap_rows = imap_rows.clone();
        move |provider_id: &str| {
            let imap = provider_id == "imap";
            server_row.set_title(if imap {
                tr("account.field.imap_host")
            } else {
                tr("account.field.server_url")
            });
            imap_rows.port.set_visible(imap);
            imap_rows.security.set_visible(imap);
            imap_rows.username.set_visible(imap);
            if creating {
                token_row.set_title(if imap {
                    tr("account.field.imap_password")
                } else {
                    tr("account.field.personal_access_token")
                });
            }
            advanced_row.set_visible(!imap);
        }
    };
    apply_provider_labels(
        memonotes::adapters::PROVIDERS
            .get(provider_index as usize)
            .map_or("", |info| info.id),
    );
    provider_row.connect_selected_notify(move |row| {
        if let Some(info) = memonotes::adapters::PROVIDERS.get(row.selected() as usize) {
            apply_provider_labels(info.id);
        }
    });
    let form = gtk::Box::new(gtk::Orientation::Vertical, 0);
    form.append(&error_banner);
    form.append(&form_group);
    let account_toast_overlay = adw::ToastOverlay::new();
    account_toast_overlay.set_vexpand(true);
    account_toast_overlay.set_child(Some(&form));
    dialog.content_area().append(&account_toast_overlay);

    set_account_editor_editable(
        &dialog,
        &account_state.borrow(),
        editing.get(),
        &provider_row,
        &account_name_row,
        &server_row,
        &imap_rows,
        &token_row,
        &token_info,
        &allow_http_row,
    );

    let state = Rc::clone(state);
    let rows = rows.clone();
    let list_status = list_status.clone();
    let state_for_dialog = Rc::clone(&account_state);
    let editing_for_dialog = Rc::clone(&editing);
    let title_for_dialog = window_title.clone();
    dialog.connect_response(move |dialog, response| {
        if response != gtk::ResponseType::Accept {
            dialog.close();
            return;
        }

        if state_for_dialog.borrow().is_some() && !editing_for_dialog.get() {
            editing_for_dialog.set(true);
            set_account_editor_editable(
                dialog,
                &state_for_dialog.borrow(),
                true,
                &provider_row,
                &account_name_row,
                &server_row,
                &imap_rows,
                &token_row,
                &token_info,
                &allow_http_row,
            );
            set_dialog_response_label(dialog, tr("action.save"));
            dialog.set_title(Some(tr("account.edit.title")));
            title_for_dialog.set_title(tr("account.edit.title"));
            if let Some(account) = state_for_dialog.borrow().as_ref() {
                title_for_dialog.set_subtitle(&account.server_url);
            }
            return;
        }

        let Some(provider) = memonotes::adapters::PROVIDERS
            .get(provider_row.selected() as usize)
            .filter(|info| info.available)
            .map(|info| info.id)
        else {
            error_banner.set_title(tr("account.error.backend_unavailable"));
            error_banner.set_revealed(true);
            return;
        };
        let label = account_name_row.text().trim().to_owned();
        let server_url = if provider == "imap" {
            let port_text = imap_rows.port.text();
            let starttls = imap_rows.security.selected() == 1;
            let port = if port_text.trim().is_empty() {
                Some(if starttls { 143 } else { 993 })
            } else {
                port_text.trim().parse::<u16>().ok()
            };
            let imap = port.map(|port| memonotes::imap_notes::ImapAccount {
                username: imap_rows.username.text().trim().to_owned(),
                host: server_row.text().trim().to_owned(),
                port,
                starttls,
            });
            match imap {
                Some(imap) if imap.username.is_empty() || imap.host.is_empty() => String::new(),
                Some(imap) => match memonotes::imap_notes::ImapAccount::parse(&imap.spec()) {
                    Ok(_) => imap.spec(),
                    Err(error) => {
                        error_banner.set_title(&format!("{error:#}"));
                        error_banner.set_revealed(true);
                        return;
                    }
                },
                None => {
                    error_banner.set_title(tr("api.imap.error.invalid_server"));
                    error_banner.set_revealed(true);
                    return;
                }
            }
        } else {
            server_row.text().trim().to_owned()
        };
        let new_token = token_row.text().to_string();
        if label.is_empty()
            || server_url.is_empty()
            || (state_for_dialog.borrow().is_none() && new_token.trim().is_empty())
        {
            error_banner.set_title(tr("account.error.missing_connection_fields"));
            error_banner.set_revealed(true);
            return;
        }

        let current_account = state_for_dialog.borrow().clone();
        let account_id = current_account
            .as_ref()
            .map_or_else(|| Uuid::new_v4().to_string(), |account| account.id.clone());
        let enabled = current_account
            .as_ref()
            .is_none_or(|account| account.enabled);
        let allow_http = allow_http_row.is_active();
        error_banner.set_revealed(false);
        let connection_toast = adw::Toast::new(tr("account.status.checking_connection"));
        connection_toast.set_timeout(0);
        account_toast_overlay.add_toast(connection_toast.clone());
        if let Some(button) = dialog.widget_for_response(gtk::ResponseType::Accept) {
            button.set_sensitive(false);
        }
        if let Some(button) = dialog.widget_for_response(gtk::ResponseType::Cancel) {
            button.set_sensitive(false);
        }

        let account_id_for_worker = account_id.clone();
        let server_for_worker = server_url.clone();
        let new_token_for_worker = new_token.clone();
        let existing_for_worker = current_account.clone();
        let label_for_worker = label.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = (|| -> Result<Option<Option<String>>> {
                let previous_token =
                    if existing_for_worker.is_some() && !new_token_for_worker.is_empty() {
                        Some(memonotes::credentials::lookup_token(
                            &account_id_for_worker,
                        )?)
                    } else {
                        None
                    };
                let effective_token = if !new_token_for_worker.is_empty() {
                    Some(new_token_for_worker.clone())
                } else if enabled {
                    memonotes::credentials::lookup_token(&account_id_for_worker)?
                } else {
                    None
                };
                let adapter = memonotes::adapters::connect(
                    provider,
                    &server_for_worker,
                    effective_token
                        .as_deref()
                        .unwrap_or("offline-account-without-token"),
                )?;
                if adapter.uses_plain_http() && !allow_http {
                    anyhow::bail!(tr("account.error.insecure_http_confirmation_required"));
                }
                if enabled {
                    effective_token
                        .as_deref()
                        .context(tr("account.error.replacement_token_required"))?;
                    adapter.check_connection()?;
                }
                if !new_token_for_worker.is_empty() {
                    memonotes::credentials::store_token(
                        &account_id_for_worker,
                        &label_for_worker,
                        &new_token_for_worker,
                    )?;
                }
                Ok(previous_token)
            })();
            let _ = sender.send(result);
        });

        let state_for_save = Rc::clone(&state);
        let rows_for_save = rows.clone();
        let list_status_for_save = list_status.clone();
        let dialog_for_save = dialog.clone();
        let error_banner_for_save = error_banner.clone();
        let toast_overlay_for_save = account_toast_overlay.clone();
        let account_id_for_save = account_id.clone();
        api_timeout(&state, Duration::from_millis(50), move || {
            match receiver.try_recv() {
                Ok(worker_result) => {
                    connection_toast.dismiss();
                    if !dialog_for_save.is_visible() {
                        if let Ok(previous_token) = worker_result {
                            rollback_account_token(
                                &account_id_for_save,
                                current_account.is_none(),
                                previous_token,
                                &label,
                            );
                        }
                        return gtk::glib::ControlFlow::Break;
                    }
                    match worker_result {
                        Ok(previous_token) => {
                            let save_result = if current_account.is_some() {
                                state_for_save.store.borrow().update_backend_account(
                                    &account_id_for_save,
                                    &label,
                                    &server_url,
                                )
                            } else {
                                state_for_save
                                    .store
                                    .borrow()
                                    .add_backend_account_with_state(
                                        &account_id_for_save,
                                        &label,
                                        provider,
                                        &server_url,
                                        true,
                                    )
                            };
                            match save_result {
                                Ok(()) => {
                                    refresh_backend_accounts(
                                        &rows_for_save,
                                        &state_for_save,
                                        &list_status_for_save,
                                    );
                                    if let Some(window) = rows_for_save.parent.upgrade() {
                                        let _ = populate_sidebar(&state_for_save, &window);
                                        load_backend_accounts(&state_for_save, &window);
                                    }
                                    dialog_for_save.close();
                                }
                                Err(error) => {
                                    rollback_account_token(
                                        &account_id_for_save,
                                        current_account.is_none(),
                                        previous_token,
                                        &label,
                                    );
                                    error_banner_for_save.set_title(&memonotes::tr_format!(
                                        "account.error.save_failed",
                                        error = format!("{error:#}")
                                    ));
                                    error_banner_for_save.set_revealed(true);
                                }
                            }
                        }
                        Err(error) => {
                            toast_overlay_for_save.add_toast(
                                adw::Toast::builder()
                                    .title(memonotes::tr_format!(
                                        "account.error.connection_failed",
                                        error = format!("{error:#}")
                                    ))
                                    .use_markup(false)
                                    .timeout(8)
                                    .build(),
                            );
                        }
                    }
                    if let Some(button) =
                        dialog_for_save.widget_for_response(gtk::ResponseType::Accept)
                    {
                        button.set_sensitive(true);
                    }
                    if let Some(button) =
                        dialog_for_save.widget_for_response(gtk::ResponseType::Cancel)
                    {
                        button.set_sensitive(true);
                    }
                    gtk::glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    connection_toast.dismiss();
                    toast_overlay_for_save
                        .add_toast(adw::Toast::new(tr("account.error.save_interrupted")));
                    gtk::glib::ControlFlow::Break
                }
            }
        });
    });
    dialog.present();
}

#[derive(Clone)]
struct ImapFormRows {
    port: adw::EntryRow,
    security: adw::ComboRow,
    username: adw::EntryRow,
}

fn set_account_editor_editable(
    dialog: &gtk::Dialog,
    account: &Option<BackendAccount>,
    editable: bool,
    provider_row: &adw::ComboRow,
    account_name_row: &adw::EntryRow,
    server_row: &adw::EntryRow,
    imap_rows: &ImapFormRows,
    token_row: &adw::PasswordEntryRow,
    token_info: &adw::ActionRow,
    allow_http_row: &adw::SwitchRow,
) {
    provider_row.set_sensitive(editable && account.is_none());
    account_name_row.set_editable(editable);
    server_row.set_editable(editable);
    imap_rows.port.set_editable(editable);
    imap_rows.security.set_sensitive(editable);
    imap_rows.username.set_editable(editable);
    token_row.set_editable(editable);
    token_row.set_visible(editable);
    token_info.set_visible(account.is_some() && !editable);
    allow_http_row.set_sensitive(editable);
    if let Some(button) = dialog.widget_for_response(gtk::ResponseType::Accept) {
        button.set_sensitive(true);
    }
}

fn set_dialog_response_label(dialog: &gtk::Dialog, label: &str) {
    if let Some(button) = dialog
        .widget_for_response(gtk::ResponseType::Accept)
        .and_then(|widget| widget.downcast::<gtk::Button>().ok())
    {
        button.set_label(label);
    }
}

fn rollback_account_token(
    account_id: &str,
    is_new: bool,
    previous_token: Option<Option<String>>,
    label: &str,
) {
    let account_id = account_id.to_owned();
    let label = label.to_owned();
    thread::spawn(move || {
        if is_new {
            let _ = memonotes::credentials::delete_token(&account_id);
        } else if let Some(previous_token) = previous_token {
            if let Some(previous_token) = previous_token {
                let _ = memonotes::credentials::store_token(&account_id, &label, &previous_token);
            } else {
                let _ = memonotes::credentials::delete_token(&account_id);
            }
        }
    });
}

fn remove_backend_account(
    state: &Rc<WindowState>,
    rows: &AccountRows,
    status: &gtk::Label,
    account: BackendAccount,
) {
    status.set_text(tr("account.status.removing_account_and_token"));
    let (sender, receiver) = mpsc::channel();
    let account_id = account.id.clone();
    thread::spawn(move || {
        let _ = sender.send(memonotes::credentials::delete_token(&account_id));
    });

    let state = Rc::clone(state);
    let rows = rows.clone();
    let status = status.clone();
    let timeout_state = Rc::clone(&state);
    api_timeout(
        &timeout_state,
        Duration::from_millis(50),
        move || match receiver.try_recv() {
            Ok(Ok(())) => match state.store.borrow().remove_backend_account(&account.id) {
                Ok(()) => {
                    refresh_backend_accounts(&rows, &state, &status);
                    if let Some(window) = rows.parent.upgrade() {
                        let _ = populate_sidebar(&state, &window);
                    }
                    gtk::glib::ControlFlow::Break
                }
                Err(error) => {
                    status.set_text(&memonotes::tr_format!(
                        "account.error.remove_failed",
                        error = format!("{error:#}")
                    ));
                    gtk::glib::ControlFlow::Break
                }
            },
            Ok(Err(error)) => {
                status.set_text(&memonotes::tr_format!(
                    "credentials.error.remove_key_failed",
                    error = format!("{error:#}")
                ));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                status.set_text(tr("account.error.remove_interrupted"));
                gtk::glib::ControlFlow::Break
            }
        },
    );
}

fn animate_sidebar_position(
    layout: &gtk::Paned,
    target: i32,
    generation: u64,
    current_generation: Rc<Cell<u64>>,
    running: Rc<Cell<bool>>,
) {
    let start = layout.position();
    let animations_enabled = layout.settings().property::<bool>("gtk-enable-animations");
    if start == target || !animations_enabled {
        running.set(false);
        layout.set_position(target);
        return;
    }
    running.set(true);
    let started = Cell::new(None);
    layout.add_tick_callback(move |layout, clock| {
        if current_generation.get() != generation {
            return gtk::glib::ControlFlow::Break;
        }
        let frame_time = clock.frame_time();
        let start_time = started.get().unwrap_or(frame_time);
        started.set(Some(start_time));
        let progress = ((frame_time - start_time) as f64 / 240_000.0).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let position = (start as f64 + (target - start) as f64 * eased).round() as i32;
        if layout.position() != position {
            layout.set_position(position);
        }
        if progress >= 1.0 {
            running.set(false);
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
}

fn build_sidebar(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    add_folder: &gtk::Button,
) -> Result<gtk::ScrolledWindow> {
    install_folder_menu_style();
    let items = state.sidebar.clone();
    items.set_activate_on_single_click(true);
    populate_sidebar(state, window)?;

    let state_for_create = Rc::clone(state);
    let parent_window = window.clone();
    let handler = add_folder.connect_clicked(move |_| {
        let dialog = gtk::Dialog::builder()
            .title(tr("folder.new"))
            .modal(true)
            .transient_for(&parent_window)
            .build();
        let cancel_button = dialog.add_button(tr("action.cancel"), gtk::ResponseType::Cancel);
        dialog.add_button(tr("action.create"), gtk::ResponseType::Accept);
        if let Some(action_area) = cancel_button.parent().and_downcast::<gtk::Box>() {
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            action_area.insert_child_after(&spacer, Some(&cancel_button));
        }

        let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
        form.set_vexpand(true);
        form.set_margin_top(16);
        form.set_margin_bottom(16);
        form.set_margin_start(16);
        form.set_margin_end(16);
        let name = gtk::Entry::builder()
            .placeholder_text(tr("folder.name.placeholder"))
            .activates_default(true)
            .build();
        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_visible(false);
        error.set_xalign(0.0);
        error.set_wrap(true);
        form.append(&name);
        form.append(&error);
        dialog.content_area().append(&form);
        dialog.set_default_response(gtk::ResponseType::Accept);

        let error_for_input = error.clone();
        let dialog_for_input = dialog.clone();
        name.connect_changed(move |entry| {
            if !entry.text().trim().is_empty() {
                error_for_input.set_visible(false);
                dialog_for_input.set_default_size(-1, -1);
            }
        });

        let state_for_response = Rc::clone(&state_for_create);
        let parent_window_for_response = parent_window.clone();
        let name_for_response = name.clone();
        let error_for_response = error.clone();
        dialog.connect_response(move |dialog, response| {
            if response != gtk::ResponseType::Accept {
                dialog.close();
                return;
            }

            let folder_name = name_for_response.text();
            let account_id = state_for_response.active_filter.borrow().account_id();
            let create_result = state_for_response
                .store
                .borrow()
                .create_folder(account_id.as_deref(), folder_name.as_str());
            match create_result {
                Ok(folder_id) => {
                    *state_for_response.active_filter.borrow_mut() =
                        ActiveNoteFilter::Folder(account_id, folder_id);
                    *state_for_response.note_id.borrow_mut() = None;
                    set_editor_text(&state_for_response, "");
                    let _ = populate_sidebar(&state_for_response, &parent_window_for_response);
                    let _ = refresh_notes(&state_for_response);
                    dialog.close();
                }
                Err(_) => {
                    error_for_response.set_text(tr("folder.error.name_required"));
                    error_for_response.set_visible(true);
                }
            }
        });
        dialog.present();
    });
    track_signal(state, add_folder, handler);

    let state_for_selection = Rc::clone(state);
    let handler = items.connect_row_activated(move |_, row| {
        let filter = state_for_selection
            .sidebar_entries
            .borrow()
            .iter()
            .find(|entry| &entry.row == row)
            .map(|entry| entry.filter.clone());
        let Some(filter) = filter else {
            return;
        };
        *state_for_selection.active_filter.borrow_mut() = filter;
        *state_for_selection.note_id.borrow_mut() = None;
        state_for_selection.pin_button.set_sensitive(false);
        state_for_selection.archive_button.set_sensitive(false);
        state_for_selection.delete_button.set_visible(false);
        state_for_selection.folder_button.set_sensitive(false);
        state_for_selection.folder_picker.set_sensitive(false);
        update_archive_button(
            &state_for_selection.archive_button,
            state_for_selection.active_filter.borrow().is_archived(),
        );
        set_editor_text(&state_for_selection, "");
        let _ = refresh_notes(&state_for_selection);
        let active = state_for_selection.active_filter.borrow().clone();
        if let ActiveNoteFilter::Folder(Some(account_id), folder_id) = active {
            if let Some(window) = row.root().and_downcast::<ApplicationWindow>() {
                refresh_backend_folder(&state_for_selection, &window, &account_id, &folder_id);
            }
        }
    });
    track_signal(state, &items, handler);

    let sidebar = gtk::ScrolledWindow::builder().child(&items).build();
    sidebar.add_css_class("memonotes-sidebar");
    items.add_css_class("memonotes-sidebar");
    Ok(sidebar)
}

/// Accounts first (sorted by label), then notes kept only on this device.
fn sidebar_sections(store: &Store) -> Result<Vec<(Option<String>, String)>> {
    let accounts = store.backend_accounts()?;
    let show_local = accounts.is_empty() || store.has_local_data()?;
    let mut sections: Vec<_> = accounts
        .into_iter()
        .map(|account| (Some(account.id), account.label))
        .collect();
    if show_local {
        sections.push((None, tr("sidebar.on_this_device").to_owned()));
    }
    Ok(sections)
}

fn populate_sidebar(state: &Rc<WindowState>, window: &ApplicationWindow) -> Result<()> {
    let (sections, folders) = {
        let store = state.store.borrow();
        (sidebar_sections(&store)?, store.folders()?)
    };
    let items = &state.sidebar;
    while let Some(child) = items.first_child() {
        items.remove(&child);
    }
    state.sidebar_entries.borrow_mut().clear();
    state.account_indicators.borrow_mut().clear();
    state.account_alerts.borrow_mut().clear();

    let show_headings = sections.iter().any(|(account_id, _)| account_id.is_some());
    for (account_id, label) in sections {
        if show_headings {
            let heading = append_sidebar_heading(items, &label, account_id.is_some());
            if let (Some(account_id), Some((indicator, alert))) = (&account_id, heading) {
                update_account_indicator(
                    &indicator,
                    state.account_statuses.borrow().get(account_id).copied(),
                );
                state
                    .account_indicators
                    .borrow_mut()
                    .insert(account_id.clone(), indicator);
                let weak_state = Rc::downgrade(state);
                let weak_window = window.downgrade();
                let alert_account_id = account_id.clone();
                let alert_label = label.clone();
                alert.connect_clicked(move |_| {
                    if let (Some(state), Some(window)) =
                        (weak_state.upgrade(), weak_window.upgrade())
                    {
                        show_sync_problems_dialog(
                            &state,
                            &window,
                            &alert_account_id,
                            &alert_label,
                        );
                    }
                });
                state
                    .account_alerts
                    .borrow_mut()
                    .insert(account_id.clone(), alert);
                refresh_account_alert(state, account_id);
            }
        }
        append_sidebar_item(
            items,
            state,
            tr("note.all"),
            ActiveNoteFilter::All(account_id.clone()),
        );
        append_sidebar_item(
            items,
            state,
            tr("folder.unfiled"),
            ActiveNoteFilter::Unfiled(account_id.clone()),
        );
        append_sidebar_item(
            items,
            state,
            tr("note.archive.title"),
            ActiveNoteFilter::Archive(account_id.clone()),
        );
        for folder in folders
            .iter()
            .filter(|folder| folder.account_id == account_id)
        {
            append_folder_row(
                items,
                state,
                window,
                &account_id,
                &folder.id,
                &folder.name,
                folder.count,
            );
        }
    }

    let active = state.active_filter.borrow().clone();
    let (selected_row, active_missing) = {
        let entries = state.sidebar_entries.borrow();
        match entries.iter().find(|entry| entry.filter == active) {
            Some(entry) => (Some(entry.row.clone()), false),
            None => (entries.first().map(|entry| entry.row.clone()), true),
        }
    };
    if let Some(row) = &selected_row {
        items.select_row(Some(row));
    }
    if active_missing {
        let fallback = state
            .sidebar_entries
            .borrow()
            .first()
            .map(|entry| entry.filter.clone());
        if let Some(fallback) = fallback {
            *state.active_filter.borrow_mut() = fallback;
        }
        clear_note_selection(state);
        return refresh_notes(state);
    }
    refresh_folder_counts(state)
}

fn account_status_text(status: Option<bool>) -> &'static str {
    match status {
        Some(true) => tr("account.status.online"),
        Some(false) => tr("account.status.offline"),
        None => tr("account.status.checking_server"),
    }
}

fn update_account_indicator(indicator: &gtk::Box, status: Option<bool>) {
    let online = status == Some(true);
    indicator.remove_css_class(if online {
        "account-offline"
    } else {
        "account-online"
    });
    indicator.add_css_class(if online {
        "account-online"
    } else {
        "account-offline"
    });
    let text = account_status_text(status);
    indicator.set_tooltip_text(Some(text));
    indicator.update_property(&[gtk::accessible::Property::Label(text)]);
}

fn set_account_online(state: &WindowState, account_id: &str, online: bool) {
    let online = online && adw::gio::NetworkMonitor::default().is_network_available();
    state
        .account_statuses
        .borrow_mut()
        .insert(account_id.to_owned(), online);
    if let Some(indicator) = state.account_indicators.borrow().get(account_id) {
        update_account_indicator(indicator, Some(online));
    }
}

fn refresh_account_alert(state: &WindowState, account_id: &str) {
    let Some(alert) = state.account_alerts.borrow().get(account_id).cloned() else {
        return;
    };
    let has_problems = state
        .store
        .borrow()
        .sync_rejections(account_id)
        .is_ok_and(|rejections| !rejections.is_empty());
    alert.set_visible(has_problems);
}

fn navigate_to_note(state: &Rc<WindowState>, window: &ApplicationWindow, note_id: &str) {
    let note = state.store.borrow().note(note_id).ok().flatten();
    let Some(note) = note else {
        state
            .toast_overlay
            .add_toast(adw::Toast::new(tr("sync.problems.note_missing")));
        return;
    };
    let filter = if note.archived {
        ActiveNoteFilter::Archive(note.account_id.clone())
    } else {
        ActiveNoteFilter::All(note.account_id.clone())
    };
    state.active_filter.replace(filter);
    state.search.set_text("");
    if let Err(error) = populate_sidebar(state, window).and_then(|()| refresh_notes(state)) {
        state
            .toast_overlay
            .add_toast(adw::Toast::new(&format!("{error:#}")));
        return;
    }
    state.note_list.select(note_id, true);
    *state.selection_anchor.borrow_mut() = Some(note_id.to_owned());
    state.note_list.scroll_to(note_id);
    show_note_by_id(state, note_id);
}

fn show_sync_problems_dialog(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    account_id: &str,
    account_label: &str,
) {
    let rejections = match state.store.borrow().sync_rejections(account_id) {
        Ok(rejections) => rejections,
        Err(error) => {
            show_api_error(
                state,
                &format!("rejections:{account_id}"),
                &memonotes::tr_format!(
                    "sync.error.read_local_status_failed",
                    error = format!("{error:#}")
                ),
            );
            return;
        }
    };
    refresh_account_alert(state, account_id);
    if rejections.is_empty() {
        return;
    }
    let dialog = gtk::Dialog::builder()
        .modal(true)
        .transient_for(window)
        .title(tr("sync.problems.title"))
        .default_width(520)
        .default_height(420)
        .build();
    dialog.add_button(tr("action.close"), gtk::ResponseType::Close);
    dialog.connect_response(|dialog, _| dialog.close());
    let content = dialog.content_area();
    content.set_spacing(12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(
        &gtk::Label::builder()
            .label(memonotes::tr_format!(
                "sync.problems.description",
                account = account_label
            ))
            .wrap(true)
            .xalign(0.0)
            .build(),
    );

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for rejection in rejections {
        let title_text = if rejection.title.trim().is_empty() {
            tr("note.new").to_owned()
        } else {
            rejection.title.clone()
        };
        let title = gtk::Label::builder()
            .label(&title_text)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let title_widget: gtk::Widget = if rejection.is_note {
            let open = gtk::Button::builder()
                .child(&title)
                .css_classes(["flat"])
                .hexpand(true)
                .halign(gtk::Align::Start)
                .tooltip_text(tr("sync.problems.open_note"))
                .build();
            let weak_dialog = dialog.downgrade();
            let state = Rc::clone(state);
            let window = window.clone();
            let note_id = rejection.item_id.clone();
            open.connect_clicked(move |_| {
                if let Some(dialog) = weak_dialog.upgrade() {
                    dialog.close();
                }
                let state = Rc::clone(&state);
                let window = window.clone();
                let note_id = note_id.clone();
                gtk::glib::idle_add_local_once(move || navigate_to_note(&state, &window, &note_id));
            });
            open.upcast()
        } else {
            title.set_hexpand(true);
            title.upcast()
        };
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text(tr("sync.problems.copy_error"))
            .build();
        let copied_text = format!("{title_text}: {}", rejection.message);
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&copied_text);
            button.set_icon_name("object-select-symbolic");
            let weak_button = button.downgrade();
            gtk::glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                if let Some(button) = weak_button.upgrade() {
                    button.set_icon_name("edit-copy-symbolic");
                }
            });
        });
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let discard = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text(tr("sync.problems.discard"))
            .build();
        header.append(&title_widget);
        header.append(&copy);
        header.append(&discard);
        let message = gtk::Label::builder()
            .label(&rejection.message)
            .selectable(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .css_classes(["caption"])
            .build();
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        body.append(&header);
        body.append(&message);
        let row = gtk::ListBoxRow::builder()
            .child(&body)
            .activatable(false)
            .build();
        list.append(&row);
        let weak_row = row.downgrade();
        let weak_list = list.downgrade();
        let weak_dialog = dialog.downgrade();
        let state = Rc::clone(state);
        let account_id = account_id.to_owned();
        let item_id = rejection.item_id.clone();
        discard.connect_clicked(move |_| {
            if let Err(error) = state
                .store
                .borrow()
                .discard_sync_rejection(&account_id, &item_id)
            {
                show_api_error(
                    &state,
                    &format!("rejections:{account_id}"),
                    &format!("{error:#}"),
                );
                return;
            }
            refresh_account_alert(&state, &account_id);
            if let (Some(list), Some(row)) = (weak_list.upgrade(), weak_row.upgrade()) {
                list.remove(&row);
                if list.first_child().is_none()
                    && let Some(dialog) = weak_dialog.upgrade()
                {
                    dialog.close();
                }
            }
        });
    }
    content.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    );
    dialog.present();
}

fn append_sidebar_heading(
    list: &gtk::ListBox,
    title: &str,
    remote: bool,
) -> Option<(gtk::Box, gtk::Button)> {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(0)
        .margin_top(12)
        .margin_bottom(2)
        .build();
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    content.append(&label);
    let indicator = remote.then(|| {
        let alert = gtk::Button::builder()
            .icon_name("dialog-warning-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .tooltip_text(tr("sync.problems.title"))
            .visible(false)
            .build();
        alert.update_property(&[gtk::accessible::Property::Label(tr("sync.problems.title"))]);
        content.append(&alert);
        let indicator = gtk::Box::builder()
            .width_request(8)
            .height_request(8)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Img)
            .css_classes(["account-status"])
            .build();
        update_account_indicator(&indicator, None);
        content.append(&indicator);
        (indicator, alert)
    });
    let row = gtk::ListBoxRow::builder()
        .child(&content)
        .selectable(false)
        .activatable(false)
        .focusable(false)
        .build();
    row.add_css_class("sidebar-account-heading");
    list.append(&row);
    indicator
}

#[cfg(test)]
mod sidebar_status_tests {
    use super::{account_status_text, tr};

    #[test]
    fn tooltip_describes_each_connection_status() {
        assert_eq!(account_status_text(Some(true)), tr("account.status.online"));
        assert_eq!(
            account_status_text(Some(false)),
            tr("account.status.offline")
        );
        assert_eq!(
            account_status_text(None),
            tr("account.status.checking_server")
        );
    }
}

fn append_sidebar_item(
    list: &gtk::ListBox,
    state: &Rc<WindowState>,
    label: &str,
    filter: ActiveNoteFilter,
) {
    let row = gtk::ListBoxRow::new();
    let count_label = folder_count_label(0);
    row.set_child(Some(&folder_row_content(label, &count_label)));
    list.append(&row);
    state.sidebar_entries.borrow_mut().push(SidebarEntry {
        row,
        filter,
        count_label,
    });
}

fn folder_count_label(count: i64) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(count.to_string())
        .xalign(1.0)
        .width_chars(4)
        .halign(gtk::Align::End)
        .hexpand(false)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label
}

fn append_folder_row(
    list: &gtk::ListBox,
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    account_id: &Option<String>,
    id: &str,
    name: &str,
    count: i64,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let count_label = folder_count_label(count);
    state.sidebar_entries.borrow_mut().push(SidebarEntry {
        row: row.clone(),
        filter: ActiveNoteFilter::Folder(account_id.clone(), id.to_owned()),
        count_label: count_label.clone(),
    });
    row.set_child(Some(&folder_row_content(name, &count_label)));
    let drop_target = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
    let state_for_drop = Rc::downgrade(state);
    let folder_id_for_drop = id.to_owned();
    drop_target.connect_drop(move |_, value, _, _| {
        let Some(state_for_drop) = state_for_drop.upgrade() else {
            return false;
        };
        let Ok(note_ids) = value.get::<String>() else {
            return false;
        };
        let note_ids: Vec<String> = note_ids.lines().map(str::to_owned).collect();
        move_notes_to_folder(&state_for_drop, &note_ids, &folder_id_for_drop)
    });
    row.add_controller(drop_target);
    list.append(&row);

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 3);
    menu.add_css_class("folder-context-menu");

    let rename_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    rename_list.set_activate_on_single_click(true);
    let rename_item = folder_menu_item(
        "document-edit-symbolic",
        tr("folder.rename"),
        "rename",
        false,
    );
    rename_list.append(&rename_item);
    menu.append(&rename_list);

    let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
    divider.set_margin_start(6);
    divider.set_margin_end(6);
    menu.append(&divider);

    let delete_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    delete_list.set_activate_on_single_click(true);
    let delete_item = folder_menu_item("user-trash-symbolic", tr("folder.delete"), "delete", true);
    delete_list.append(&delete_item);
    menu.append(&delete_list);

    popover.set_child(Some(&menu));
    popover.set_parent(&row);

    let state_for_rename = Rc::downgrade(state);
    let row_for_rename = row.downgrade();
    let window_for_menu = window.downgrade();
    let folder_id = id.to_owned();
    let old_name = name.to_owned();
    let count_label = count_label.clone();
    let state_for_delete = state_for_rename.clone();
    let window_for_delete = window_for_menu.clone();
    let folder_id_for_delete = folder_id.clone();
    let folder_name_for_delete = old_name.clone();
    let popover_for_rename = popover.downgrade();
    rename_list.connect_row_activated(move |_, _| {
        let (Some(state_for_rename), Some(row_for_rename), Some(popover_for_rename)) = (
            state_for_rename.upgrade(),
            row_for_rename.upgrade(),
            popover_for_rename.upgrade(),
        ) else {
            return;
        };
        let current_name = row_for_rename
            .child()
            .and_downcast::<gtk::Box>()
            .and_then(|box_widget| box_widget.first_child())
            .and_downcast::<gtk::Label>()
            .map(|label| label.text().to_string())
            .unwrap_or_else(|| old_name.clone());
        popover_for_rename.popdown();
        begin_folder_rename(
            &row_for_rename,
            &state_for_rename,
            &folder_id,
            &current_name,
            &count_label,
        );
    });

    let popover_for_delete = popover.downgrade();
    delete_list.connect_row_activated(move |_, _| {
        let (Some(state_for_delete), Some(window_for_delete), Some(popover_for_delete)) = (
            state_for_delete.upgrade(),
            window_for_delete.upgrade(),
            popover_for_delete.upgrade(),
        ) else {
            return;
        };
        popover_for_delete.popdown();
        confirm_delete_folder(
            &state_for_delete,
            &window_for_delete,
            &folder_id_for_delete,
            &folder_name_for_delete,
        );
    });

    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    let popover_for_click = popover.downgrade();
    right_click.connect_pressed(move |_, _, x, y| {
        let Some(popover_for_click) = popover_for_click.upgrade() else {
            return;
        };
        popover_for_click
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover_for_click.popup();
    });
    row.add_controller(right_click);
    let popover_for_destroy = popover.downgrade();
    row.connect_destroy(move |_| {
        if let Some(popover) = popover_for_destroy.upgrade() {
            popover.unparent();
        }
    });
    row
}

fn folder_menu_item(icon: &str, text: &str, id: &str, destructive: bool) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(id);
    row.add_css_class("folder-context-menu-row");
    if destructive {
        row.add_css_class("destructive");
    }
    let item = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    item.set_margin_top(5);
    item.set_margin_bottom(5);
    item.set_margin_start(8);
    item.set_margin_end(12);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    item.append(&image);
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .hexpand(true)
        .build();
    label.add_css_class("body");
    item.append(&label);
    row.set_child(Some(&item));
    row
}

fn install_folder_menu_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".folder-context-menu { padding: 0; } \
         .folder-context-menu list { padding: 0; margin: 0; } \
         .folder-context-menu-row { border-radius: 6px; margin: 1px 3px; } \
         .folder-context-menu-row:hover { background-color: alpha(@theme_fg_color, 0.08); } \
         .folder-context-menu-row.destructive, \
         .folder-context-menu-row.destructive label, \
         .folder-context-menu-row.destructive image { color: @error_color; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn install_account_error_style(window: &ApplicationWindow) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".account-status { \
             min-width: 8px; \
             min-height: 8px; \
             border-radius: 50%; \
         } \
         .account-status.account-online { background-color: #2ec27e; } \
         .account-status.account-offline { background-color: #e01b24; } \
         window.account-editor .dialog-action-box { \
             margin: 0; \
             padding: 0 24px 24px; \
         } \
         window.account-editor .dialog-action-area { \
             margin: 0; \
             padding: 0; \
         } \
         banner.account-error-banner, \
         banner.account-error-banner > revealer, \
         banner.account-error-banner > revealer > widget { \
             background-color: #f6cdd2; \
             background-image: none; \
             border-radius: 8px; \
         } \
         banner.account-error-banner > revealer > widget > label { \
             color: #8f1d32; \
             font-weight: 600; \
         }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    window.connect_destroy(move |_| {
        gtk::style_context_remove_provider_for_display(&display, &provider);
    });
}

fn install_note_detail_style(window: &ApplicationWindow) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    let style_manager = adw::StyleManager::default();
    provider.load_from_data(&note_detail_css(style_manager.is_dark()));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let provider_for_theme = provider.clone();
    let theme_handler = style_manager.connect_dark_notify(move |style_manager| {
        provider_for_theme.load_from_data(&note_detail_css(style_manager.is_dark()));
    });
    let theme_handler = RefCell::new(Some(theme_handler));
    window.connect_destroy(move |_| {
        if let Some(handler) = theme_handler.borrow_mut().take() {
            style_manager.disconnect(handler);
            gtk::style_context_remove_provider_for_display(&display, &provider);
        }
    });
}

fn note_detail_css(dark: bool) -> String {
    let default = note_color_value("default", dark);
    let mut css = format!(
        ".note-detail-surface {{ background-color: {default}; background-image: none; }} \
         .note-detail-surface label.editor-bullet {{ background-color: {default}; }} \
         textview.empty-note-heading {{ font-size: 1.65em; font-weight: 700; }} \
         .note-detail-pane {{ background-image: linear-gradient(to right, alpha(black, 0.05), transparent 8px); }} \
         .note-color-option {{ padding: 5px; }} \
         .note-color-swatch {{ border-radius: 50px; border: 1px solid alpha(@theme_fg_color, 0.18); }} \
         button.task-checkbox {{ min-width: 16px; min-height: 16px; padding: 0; border-radius: 50%; border: 1px solid #3584e4; background: transparent; box-shadow: none; transform: translate(0px, 5px); }} \
         button.task-checkbox:checked {{ background: #3584e4; color: white; }} \
         button.task-checkbox label {{ font-size: 14px; font-weight: 700; }} \
         picture.editor-image-selected {{ border: 2px solid @accent_color; }} \
         box.editor-image-drop-indicator {{ background-color: @accent_bg_color; border-radius: 1px; }} \
         box.editor-image-handle {{ background-color: @accent_bg_color; border: 2px solid white; border-radius: 50%; }} \
         entry.table-cell {{ border-radius: 0; box-shadow: none; background: transparent; border: 1px solid alpha(@theme_fg_color, 0.22); }} \
         entry.table-header {{ font-weight: 700; background-color: alpha(@theme_fg_color, 0.06); }} \
         entry.table-cell.table-cell-selected {{ background-color: alpha(@accent_bg_color, 0.3); }} \
         grid.table-selecting entry text selection {{ background: transparent; color: inherit; }} "
    );
    for &(color, _, _, _) in &NOTE_COLOR_PALETTE {
        let value = note_color_value(color, dark);
        if color != "default" {
            css.push_str(&format!(
                ".note-detail-surface.note-color-{color} {{ background-color: {value}; }} \
                 .note-detail-surface.note-color-{color} label.editor-bullet {{ background-color: {value}; }} "
            ));
        }
        css.push_str(&format!(
            ".note-color-swatch.swatch-{color} {{ background-color: {value}; }} "
        ));
    }
    for color in LEGACY_NOTE_COLORS {
        let value = note_color_value(color, dark);
        css.push_str(&format!(
            ".note-detail-surface.note-color-{color} {{ background-color: {value}; }} \
             .note-detail-surface.note-color-{color} label.editor-bullet {{ background-color: {value}; }} \
             .note-color-swatch.swatch-{color} {{ background-color: {value}; }} "
        ));
    }
    css
}

fn canonical_note_color(color: &str) -> &str {
    match color {
        "rose" => "pink",
        "peach" => "orange",
        "lemon" => "yellow",
        "lilac" => "plum",
        _ => color,
    }
}

fn note_color_value(color: &str, dark: bool) -> &'static str {
    let canonical = canonical_note_color(color);
    NOTE_COLOR_PALETTE
        .iter()
        .find(|entry| entry.0 == canonical)
        .map(|entry| if dark { entry.3 } else { entry.2 })
        .unwrap_or("#ffffff")
}

fn collect_note_surfaces(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut surfaces = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(widget) = pending.pop() {
        if widget.has_css_class("note-detail-surface") {
            surfaces.push(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            child = current.next_sibling();
            pending.push(current);
        }
    }
    surfaces
}

fn set_note_surfaces_color(surfaces: &[gtk::Widget], color: &str) {
    for surface in surfaces {
        for &(old_color, _, _, _) in &NOTE_COLOR_PALETTE {
            surface.remove_css_class(&format!("note-color-{old_color}"));
        }
        for old_color in LEGACY_NOTE_COLORS {
            surface.remove_css_class(&format!("note-color-{old_color}"));
        }
        if color != "default" {
            surface.add_css_class(&format!("note-color-{color}"));
        }
    }
}

fn set_note_color(state: &Rc<WindowState>, color: &str) {
    if state.store.borrow().set_note_color(color).is_err() {
        return;
    }
    *state.note_color.borrow_mut() = color.to_owned();
    set_note_surfaces_color(&state.note_surfaces.borrow(), color);
    for window in state.note_windows.borrow().values() {
        let surfaces = collect_note_surfaces(&window.clone().upcast());
        set_note_surfaces_color(&surfaces, color);
    }
}

fn confirm_delete_folder(
    state: &Rc<WindowState>,
    window: &ApplicationWindow,
    folder_id: &str,
    folder_name: &str,
) {
    let dialog = gtk::Dialog::builder()
        .title(tr("folder.delete"))
        .modal(true)
        .transient_for(window)
        .build();
    let cancel_button = dialog.add_button(tr("action.cancel"), gtk::ResponseType::Cancel);
    let keep_notes_button =
        dialog.add_button(tr("folder.delete.keep_notes"), gtk::ResponseType::Accept);
    let delete_button =
        dialog.add_button(tr("folder.delete.delete_notes"), gtk::ResponseType::Reject);
    delete_button.add_css_class("destructive-action");
    if let Some(action_area) = cancel_button.parent().and_downcast::<gtk::Box>() {
        action_area.set_orientation(gtk::Orientation::Vertical);
        action_area.set_spacing(6);
        action_area.set_halign(gtk::Align::Fill);
        action_area.set_hexpand(true);
        action_area.set_margin_top(0);
        action_area.set_margin_bottom(18);
        action_area.set_margin_start(18);
        action_area.set_margin_end(18);
        cancel_button.set_hexpand(true);
        keep_notes_button.set_hexpand(true);
        delete_button.set_hexpand(true);
    }

    let message = gtk::Label::builder()
        .label(memonotes::tr_format!(
            "folder.delete.confirm_message",
            folder_name = folder_name
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    let content_area = dialog.content_area();
    content_area.set_margin_top(18);
    content_area.set_margin_bottom(18);
    content_area.set_margin_start(18);
    content_area.set_margin_end(18);
    content_area.append(&message);

    let state_for_response = Rc::clone(state);
    let window_for_response = window.clone();
    let folder_id = folder_id.to_owned();
    dialog.connect_response(move |dialog, response| {
        let mode = match response {
            gtk::ResponseType::Accept => FolderDeleteMode::KeepNotes,
            gtk::ResponseType::Reject => FolderDeleteMode::DeleteNotes,
            _ => {
                dialog.close();
                return;
            }
        };

        let selected_note_id = state_for_response.note_id.borrow().clone();
        let deleted = state_for_response
            .store
            .borrow_mut()
            .delete_folder(&folder_id, mode);
        if deleted.is_ok() {
            state_for_response.suppress_folder_picker_events.set(true);
            let deleted_folder_is_active = {
                state_for_response
                    .active_filter
                    .borrow()
                    .folder_id()
                    .as_deref()
                    == Some(folder_id.as_str())
            };
            if deleted_folder_is_active {
                let account_id = state_for_response.active_filter.borrow().account_id();
                *state_for_response.active_filter.borrow_mut() =
                    ActiveNoteFilter::Unfiled(account_id.clone());
            }
            let _ = populate_sidebar(&state_for_response, &window_for_response);
            resync_folder_picker(&state_for_response);
            if let Some(note_id) = selected_note_id {
                let note = {
                    state_for_response
                        .store
                        .borrow()
                        .note(&note_id)
                        .ok()
                        .flatten()
                };
                if let Some(note) = note {
                    show_note(&state_for_response, &note);
                }
            }
            state_for_response.suppress_folder_picker_events.set(false);
            let _ = refresh_notes(&state_for_response);
        }
        dialog.close();
    });
    dialog.present();
}

fn folder_row_content(name: &str, count_label: &gtk::Label) -> gtk::Box {
    let row_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row_content.set_hexpand(true);
    row_content.set_margin_top(8);
    row_content.set_margin_bottom(8);
    let name_label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(12)
        .build();
    row_content.append(&name_label);
    row_content.append(count_label);
    row_content
}

fn begin_folder_rename(
    row: &gtk::ListBoxRow,
    state: &Rc<WindowState>,
    folder_id: &str,
    old_name: &str,
    count_label: &gtk::Label,
) {
    let entry = gtk::Entry::builder()
        .text(old_name)
        .hexpand(true)
        .margin_top(2)
        .margin_bottom(2)
        .margin_start(6)
        .margin_end(6)
        .build();
    row.set_child(Some(&entry));
    entry.grab_focus();
    entry.set_position(-1);

    let state_for_save = Rc::clone(state);
    let row_for_save = row.clone();
    let folder_id_for_save = folder_id.to_owned();
    let count_label_for_save = count_label.clone();
    entry.connect_activate(move |entry| {
        let new_name = entry.text();
        if state_for_save
            .store
            .borrow()
            .rename_folder(&folder_id_for_save, new_name.as_str())
            .is_ok()
        {
            row_for_save.set_child(Some(&folder_row_content(
                new_name.trim(),
                &count_label_for_save,
            )));
            resync_folder_picker(&state_for_save);
            let _ = refresh_notes(&state_for_save);
        }
    });

    let cancel_row = row.clone();
    let original_name = old_name.to_owned();
    let count_label_for_cancel = count_label.clone();
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            cancel_row.set_child(Some(&folder_row_content(
                &original_name,
                &count_label_for_cancel,
            )));
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    entry.add_controller(keys);
}

fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button
}

fn set_icon_button(button: &gtk::Button, icon_name: &str, tooltip: &str) {
    button.set_child(Some(&gtk::Image::from_icon_name(icon_name)));
    button.set_tooltip_text(Some(tooltip));
}

fn update_archive_button(button: &gtk::Button, archived: bool) {
    set_icon_button(
        button,
        if archived {
            "document-revert-symbolic"
        } else {
            "user-trash-symbolic"
        },
        if archived { "Terugzetten" } else { "Archiveer" },
    );
}

fn update_note_action_button(button: &gtk::Button, note: &Note) {
    if note.archived {
        update_archive_button(button, true);
    } else if note.content.trim().is_empty() {
        set_icon_button(button, "user-trash-symbolic", "Definitief wissen");
    } else {
        update_archive_button(button, false);
    }
}

fn install_shortcuts(
    application: &Application,
    window: &ApplicationWindow,
    state: &Rc<WindowState>,
    new_note: &gtk::Button,
    add_folder: &gtk::Button,
) {
    let quit_action = adw::gio::SimpleAction::new("quit", None);
    let application_for_quit = application.clone();
    quit_action.connect_activate(move |_, _| application_for_quit.quit());
    application.add_action(&quit_action);
    application.set_accels_for_action("app.quit", &["<Control>q"]);
    install_close_shortcut(window);

    let new_note_action = adw::gio::SimpleAction::new("new-note", None);
    let new_note_for_action = new_note.clone();
    new_note_action.connect_activate(move |_, _| new_note_for_action.emit_clicked());
    window.add_action(&new_note_action);
    application.set_accels_for_action("win.new-note", &["<Control>n"]);

    let add_folder = add_folder.clone();
    let new_folder_action = adw::gio::SimpleAction::new("new-folder", None);
    new_folder_action.connect_activate(move |_, _| add_folder.emit_clicked());
    window.add_action(&new_folder_action);
    application.set_accels_for_action("win.new-folder", &["<Control><Shift>n"]);

    let focus_search_action = adw::gio::SimpleAction::new("focus-search", None);
    let search = state.search.clone();
    focus_search_action.connect_activate(move |_, _| {
        search.grab_focus();
        search.set_position(-1);
    });
    window.add_action(&focus_search_action);
    application.set_accels_for_action("win.focus-search", &["<Control>f"]);

    let undo_action = adw::gio::SimpleAction::new("undo", None);
    let state_for_undo = Rc::clone(state);
    undo_action.connect_activate(move |_, _| {
        if let Some(editor) = focused_editor(&state_for_undo) {
            undo_focused_editor(&editor, false);
        } else {
            undo_last_operation(&state_for_undo);
        }
    });
    window.add_action(&undo_action);

    let undo_keys = gtk::EventControllerKey::new();
    undo_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state_for_undo_keys = Rc::clone(state);
    undo_keys.connect_key_pressed(move |_, key, _, modifiers| {
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
            || !matches!(key, gtk::gdk::Key::z | gtk::gdk::Key::Z)
            || editor_cell_has_focus(&state_for_undo_keys.content)
        {
            return gtk::glib::Propagation::Proceed;
        }
        if let Some(editor) = focused_editor(&state_for_undo_keys) {
            undo_focused_editor(
                &editor,
                modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
            );
        } else if !modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
            undo_last_operation(&state_for_undo_keys);
        } else {
            return gtk::glib::Propagation::Proceed;
        }
        gtk::glib::Propagation::Stop
    });
    window.add_controller(undo_keys);

    let search_keys = gtk::EventControllerKey::new();
    let search_for_escape = state.search.downgrade();
    search_keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(search) = search_for_escape.upgrade() {
                search.set_text("");
            }
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    state.search.add_controller(search_keys);
}

fn install_close_shortcut(window: &ApplicationWindow) {
    let close_action = adw::gio::SimpleAction::new("close-window", None);
    let window_for_close = window.clone();
    close_action.connect_activate(move |_, _| window_for_close.close());
    window.add_action(&close_action);
    let application = window
        .application()
        .expect("window is attached to an application");
    application.set_accels_for_action("win.close-window", &["<Control>w"]);
}

fn connect_note_list_factory(state: &Rc<WindowState>) {
    let weak_state = Rc::downgrade(state);
    state.note_list.factory.connect_bind(move |_, object| {
        let Some(state) = weak_state.upgrade() else {
            return;
        };
        let item = object
            .downcast_ref::<gtk::ListItem>()
            .expect("note list item");
        let object = item
            .item()
            .and_downcast::<gtk::glib::BoxedAnyObject>()
            .expect("note list entry");
        let entry = object.borrow::<NoteListEntry>();
        let row = build_note_row(&state, &entry.note);
        update_note_row_widgets(&row, &entry.note, &entry.folder_name);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.append(&row);
        if entry.divider {
            content.append(&build_pinned_separator_row());
        }
        item.set_child(Some(&content));
    });
    state.note_list.factory.connect_unbind(|_, object| {
        if let Some(item) = object.downcast_ref::<gtk::ListItem>() {
            item.set_child(None::<&gtk::Widget>);
        }
    });
}

fn focused_editor(state: &WindowState) -> Option<gtk::TextView> {
    if state.content.has_focus() {
        return Some(state.content.clone());
    }
    state
        .note_window_editors
        .borrow()
        .values()
        .filter_map(|editor| editor.upgrade())
        .find(|editor| editor.has_focus())
}

// Image and table changes live in the editor's own history, so plain buffer undo misses them.
fn undo_focused_editor(editor: &gtk::TextView, redo: bool) {
    match editor_image_history(editor) {
        Some(history) if redo => redo_editor_change(editor, &history),
        Some(history) => undo_editor_change(editor, &history),
        None if redo => editor.buffer().redo(),
        None => editor.buffer().undo(),
    }
}

fn connect_events(state: &Rc<WindowState>, new_note: &gtk::Button) {
    connect_note_list_factory(state);
    let state_for_activation = Rc::clone(state);
    let handler = state
        .note_list
        .view
        .connect_activate(move |view, position| {
            let Some(window) = view.root().and_downcast::<ApplicationWindow>() else {
                return;
            };
            let Some(application) = window.application() else {
                return;
            };
            let Some(entry) = state_for_activation.note_list.entry(position) else {
                return;
            };
            let note_id = entry.note.id;
            let note = {
                state_for_activation
                    .store
                    .borrow()
                    .note(&note_id)
                    .ok()
                    .flatten()
            };
            if let Some(note) = note {
                open_note_window(&application, &state_for_activation, &note);
                clear_note_selection(&state_for_activation);
                let state_for_clear = Rc::clone(&state_for_activation);
                gtk::glib::idle_add_local_once(move || {
                    clear_note_selection(&state_for_clear);
                });
            }
        });
    track_signal(state, &state.note_list.view, handler);

    let state_for_search = Rc::clone(state);
    let handler = state.search.connect_search_changed(move |_| {
        let _ = refresh_notes(&state_for_search);
    });
    track_signal(state, &state.search, handler);

    let delete_keys = gtk::EventControllerKey::new();
    delete_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state_for_delete = Rc::clone(state);
    let handler = delete_keys.connect_key_pressed(move |_, key, _, modifiers| match key {
        gtk::gdk::Key::Delete => {
            if state_for_delete.note_list.selected_ids().len() > 1 {
                if state_for_delete.active_filter.borrow().is_archived() {
                    delete_selected_notes(&state_for_delete);
                } else {
                    archive_selected_notes(&state_for_delete);
                }
            } else if state_for_delete.active_filter.borrow().is_archived() {
                delete_current_note(&state_for_delete);
            } else {
                archive_current_note(&state_for_delete);
            }
            gtk::glib::Propagation::Stop
        }
        gtk::gdk::Key::a | gtk::gdk::Key::A
            if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) =>
        {
            state_for_delete.note_list.select_all();
            gtk::glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => move_note_selection(&state_for_delete, -1),
        gtk::gdk::Key::Down => move_note_selection(&state_for_delete, 1),
        gtk::gdk::Key::Tab if !modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) => {
            state_for_delete.content.grab_focus();
            gtk::glib::Propagation::Stop
        }
        _ => gtk::glib::Propagation::Proceed,
    });
    track_signal(state, &delete_keys, handler);
    state.note_list_focus.add_controller(delete_keys);
    connect_tab_navigation(
        &state.content,
        state.pin_button.clone().upcast(),
        state.note_list_focus.clone().upcast(),
    );
    connect_tab_navigation(
        &state.pin_button,
        state.archive_button.clone().upcast(),
        state.content.clone().upcast(),
    );
    connect_tab_navigation(
        &state.archive_button,
        state.delete_button.clone().upcast(),
        state.pin_button.clone().upcast(),
    );
    connect_tab_navigation(
        &state.delete_button,
        state.folder_button.clone().upcast(),
        state.archive_button.clone().upcast(),
    );
    connect_tab_navigation(
        &state.folder_button,
        state.search.clone().upcast(),
        state.delete_button.clone().upcast(),
    );

    let state_for_new = Rc::clone(state);
    let handler = new_note.connect_clicked(move |_| {
        if state_for_new.active_filter.borrow().is_archived() {
            return;
        }
        *state_for_new.note_id.borrow_mut() = None;
        state_for_new.note_list.unselect_all();
        set_editor_text(&state_for_new, "");
        set_note_surfaces_color(
            &state_for_new.note_surfaces.borrow(),
            &state_for_new.note_color.borrow(),
        );
        state_for_new.pin_button.set_sensitive(false);
        state_for_new.archive_button.set_sensitive(false);
        state_for_new.delete_button.set_visible(false);
        state_for_new.folder_button.set_sensitive(true);
        state_for_new.folder_picker.set_sensitive(true);

        let active = state_for_new.active_filter.borrow().clone();
        sync_folder_picker(
            &state_for_new,
            active.account_id().as_deref(),
            active.folder_id().as_deref(),
        );
        state_for_new.content.grab_focus();
    });
    track_signal(state, new_note, handler);

    let state_for_content = Rc::clone(state);
    let content_for_change = state.content.downgrade();
    let buffer = state.content.buffer();
    let handler = buffer.connect_changed(move |buffer| {
        let Some(editor) = content_for_change.upgrade() else {
            return;
        };
        if editor.has_css_class("editor-image-moving")
            || editor.has_css_class("editor-content-replacing")
        {
            return;
        }
        style_note_heading(buffer);
        let markdown = markdown_from_editor_buffer(buffer);
        save_current(&state_for_content, &markdown);
        if editor.has_css_class("editor-image-commit") {
            let note_id = state_for_content.note_id.borrow().clone();
            let account_id = note_id
                .and_then(|id| state_for_content.store.borrow().note(&id).ok().flatten())
                .and_then(|note| note.account_id);
            upload_now(&state_for_content, account_id.as_deref());
        }
    });
    track_signal(state, &buffer, handler);

    let state_for_pin = Rc::clone(state);
    let handler = state.pin_button.connect_clicked(move |_| {
        let Some(note_id) = state_for_pin.note_id.borrow().clone() else {
            return;
        };
        let Ok(Some(note)) = state_for_pin.store.borrow().note(&note_id) else {
            return;
        };
        if state_for_pin
            .store
            .borrow()
            .set_pinned(&note_id, !note.pinned)
            .is_ok()
        {
            if let Ok(Some(updated)) = state_for_pin.store.borrow().note(&note_id) {
                show_note(&state_for_pin, &updated);
            }
            let _ = refresh_notes(&state_for_pin);
        }
    });
    track_signal(state, &state.pin_button, handler);

    let state_for_archive = Rc::clone(state);
    let handler = state.archive_button.connect_clicked(move |_| {
        let note = state_for_archive
            .note_id
            .borrow()
            .as_deref()
            .and_then(|note_id| {
                state_for_archive
                    .store
                    .borrow()
                    .note(note_id)
                    .ok()
                    .flatten()
            });
        match note {
            Some(note) if note.archived => {
                set_current_note_archived(&state_for_archive, false);
            }
            Some(note) if note.content.trim().is_empty() => {
                delete_current_note(&state_for_archive);
            }
            Some(_) => set_current_note_archived(&state_for_archive, true),
            None => {}
        }
    });
    track_signal(state, &state.archive_button, handler);

    let state_for_delete = Rc::clone(state);
    let handler = state.delete_button.connect_clicked(move |_| {
        delete_current_note(&state_for_delete);
    });
    track_signal(state, &state.delete_button, handler);

    let state_for_menu = Rc::clone(state);
    let handler = state.folder_menu.connect_row_activated(move |_, item| {
        let action = item.widget_name();
        let Some(folder_id) = action.strip_prefix("folder-choice:") else {
            return;
        };
        let folder_id = (folder_id != "unfiled").then_some(folder_id);
        state_for_menu.folder_popover.popdown();
        if let Some(note_id) = state_for_menu.note_id.borrow().clone() {
            move_note_to_folder(&state_for_menu, &note_id, folder_id);
        } else {
            let account_id = state_for_menu.active_filter.borrow().account_id();
            *state_for_menu.active_filter.borrow_mut() =
                ActiveNoteFilter::for_folder_picker(account_id, folder_id.map(str::to_owned));
        }
    });
    track_signal(state, &state.folder_menu, handler);
}

fn connect_tab_navigation(
    widget: &impl gtk::prelude::IsA<gtk::Widget>,
    next: gtk::Widget,
    previous: gtk::Widget,
) {
    let next = next.downgrade();
    let previous = previous.downgrade();
    let widget_for_keys = widget.clone().upcast::<gtk::Widget>().downgrade();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if !matches!(key, gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab) {
            return gtk::glib::Propagation::Proceed;
        }
        if let Some(editor) = widget_for_keys
            .upgrade()
            .and_then(|widget| widget.downcast::<gtk::TextView>().ok())
        {
            // Table cells handle Tab themselves.
            if editor_cell_has_focus(&editor) {
                return gtk::glib::Propagation::Proceed;
            }
            let buffer = editor.buffer();
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            if let Some(start) = buffer.iter_at_line(cursor.line()) {
                let mut end = start;
                end.forward_to_line_end();
                let line = buffer.slice(&start, &end, true);
                if editor_list_kind(&buffer, start, &line).is_some() {
                    return gtk::glib::Propagation::Proceed;
                }
            }
        }
        let target = if key == gtk::gdk::Key::ISO_Left_Tab
            || modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK)
        {
            &previous
        } else {
            &next
        };
        if let Some(target) = target.upgrade() {
            if target.is_visible() && target.is_sensitive() && target.grab_focus() {
                return gtk::glib::Propagation::Stop;
            }
        }
        gtk::glib::Propagation::Proceed
    });
    widget.add_controller(keys);
}

// Typing saves on every keystroke; reloading every note each time makes large collections lag.
fn schedule_note_list_refresh(state: &Rc<WindowState>) {
    let generation = state.note_list_refresh_generation.get() + 1;
    state.note_list_refresh_generation.set(generation);
    let weak_state = Rc::downgrade(state);
    gtk::glib::timeout_add_local_once(Duration::from_millis(500), move || {
        let Some(state) = weak_state.upgrade() else {
            return;
        };
        if state.note_list_refresh_generation.get() == generation {
            let _ = refresh_notes(&state);
        }
    });
}

fn refresh_notes(state: &Rc<WindowState>) -> Result<()> {
    state
        .note_list_refresh_generation
        .set(state.note_list_refresh_generation.get() + 1);
    let filter = state
        .active_filter
        .borrow()
        .to_note_filter(state.search.text().to_string());
    let notes = state.store.borrow().notes(&filter)?;
    let folder_names: HashMap<_, _> = state
        .store
        .borrow()
        .folders()?
        .into_iter()
        .map(|folder| (folder.id, folder.name))
        .collect();

    let last_pinned_position = notes.iter().rposition(|note| note.pinned);
    let entries = notes
        .into_iter()
        .enumerate()
        .map(|(position, note)| {
            let folder_name = note
                .folder_id
                .as_ref()
                .and_then(|id| folder_names.get(id))
                .cloned()
                .unwrap_or_default();
            NoteListEntry {
                note,
                folder_name,
                divider: last_pinned_position == Some(position),
            }
        })
        .collect();
    state.note_list.replace(entries);
    let selected_id = state.note_id.borrow().clone();
    let selected_position = selected_id
        .as_ref()
        .and_then(|id| state.note_list.position(id));
    if selected_position.is_some() {
        state
            .note_list
            .select(selected_id.as_deref().unwrap(), false);
    } else {
        clear_note_selection(state);
    }
    refresh_folder_counts(state)?;
    Ok(())
}

fn build_pinned_separator_row() -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name("pinned-divider");
    row.add_css_class("pinned-divider-row");
    row.set_selectable(false);
    row.set_activatable(false);
    row.set_focusable(false);
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(2);
    separator.set_margin_bottom(2);
    row.set_child(Some(&separator));
    row
}

fn install_pinned_separator_style(window: &ApplicationWindow) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "listview.native-note-list > row { padding: 0; } \
         .pinned-divider-row { padding: 0; min-height: 0; } \
         .pinned-divider-row separator { min-height: 1px; } \
         .note-drag-icon, .note-drag-preview, .note-drag-preview-clear { \
             background-color: transparent; background-image: none; \
         } \
         .note-drag-preview-card { \
             background-color: @card_bg_color; color: @theme_fg_color; \
             border: 1px solid alpha(@theme_fg_color, 0.16); \
             border-radius: 8px; padding: 0.25rem 0.5rem; \
         } \
         .note-drag-preview-front-shadow { \
             box-shadow: 0 2px 6px alpha(black, 0.20); \
         } \
         .note-drag-preview-grip { \
             color: alpha(@theme_fg_color, 0.55); \
         } \
         .note-drag-preview-title { \
             font-size: 0.9em; font-weight: bold; \
             color: alpha(@theme_fg_color, 0.65); \
         } \
         .note-drag-preview-back { \
             background-color: alpha(@card_bg_color, 0.92); border-radius: 8px; \
             box-shadow: 0 2px 6px alpha(black, 0.20); \
         } \
         .note-drag-preview-count { \
             background-color: @accent_bg_color; color: @accent_fg_color; \
             border-radius: 999px; padding: 2px 3px; \
             font-size: 0.85em; font-weight: bold; \
         } \
         .memonotes-sidebar, list.memonotes-sidebar { \
             background-color: shade(@window_bg_color, 0.96); \
         } \
         .sidebar-layout > separator { \
             min-width: 6px; background-color: transparent; border: none; \
             background-image: linear-gradient(to right, transparent 2px, alpha(@theme_fg_color, 0.22) 2px, alpha(@theme_fg_color, 0.22) 3px, transparent 3px); \
         } \
         .sidebar-layout > separator:hover { \
             background-image: linear-gradient(to right, transparent 2px, alpha(@accent_bg_color, 0.8) 2px, alpha(@accent_bg_color, 0.8) 3px, transparent 3px); \
         }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    window.connect_destroy(move |_| {
        gtk::style_context_remove_provider_for_display(&display, &provider);
    });
}

fn build_note_row(state: &Rc<WindowState>, note: &Note) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
    row.set_hexpand(true);
    row.set_widget_name(&note.id);
    row.set_focusable(false);
    let item = gtk::Box::new(gtk::Orientation::Vertical, 4);
    item.set_margin_top(10);
    item.set_margin_bottom(10);
    item.set_margin_start(12);
    item.set_margin_end(12);
    let pin_icon = gtk::Image::from_icon_name("view-pin-symbolic");
    pin_icon.set_pixel_size(12);
    pin_icon.set_visible(note.pinned);
    pin_icon.add_css_class("dim-label");
    let heading = gtk::Label::builder()
        .label(if note.title.is_empty() {
            "Nieuwe notitie"
        } else {
            &note.title
        })
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    heading.add_css_class("heading");
    heading.set_hexpand(true);
    let updated_at = gtk::Label::builder()
        .label(format_updated_at(note.updated_at))
        .xalign(1.0)
        .width_chars(13)
        .build();
    updated_at.add_css_class("dim-label");
    updated_at.add_css_class("caption");
    let metadata = gtk::Box::new(gtk::Orientation::Vertical, 2);
    metadata.set_halign(gtk::Align::End);
    metadata.append(&updated_at);
    let heading_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    heading_row.append(&pin_icon);
    heading_row.append(&heading);
    heading_row.append(&metadata);
    let preview = gtk::Label::builder()
        .label(note_preview_text(note))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    preview.add_css_class("dim-label");
    preview.set_hexpand(true);
    let folder = gtk::Label::builder()
        .label("")
        .xalign(1.0)
        .width_chars(13)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    folder.add_css_class("dim-label");
    folder.add_css_class("caption");
    let preview_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    preview_row.append(&preview);
    preview_row.append(&folder);
    item.append(&heading_row);
    item.append(&preview_row);
    row.append(&item);

    let context_popover = gtk::Popover::new();
    context_popover.set_has_arrow(false);
    let context_menu = gtk::Box::new(gtk::Orientation::Vertical, 3);
    context_menu.add_css_class("folder-context-menu");
    let context_items = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    context_items.set_activate_on_single_click(true);
    context_menu.append(&context_items);
    context_popover.set_child(Some(&context_menu));
    context_popover.set_parent(&row);

    let state_for_context_action = Rc::downgrade(state);
    let popover_for_context_action = context_popover.downgrade();
    context_items.connect_row_activated(move |_, item| {
        let (Some(state), Some(popover)) = (
            state_for_context_action.upgrade(),
            popover_for_context_action.upgrade(),
        ) else {
            return;
        };
        popover.popdown();
        let action = item.widget_name();
        if let Some(folder_id) = action.strip_prefix("move-folder:") {
            let folder_id = (folder_id != "unfiled").then_some(folder_id);
            if let Some(note_id) = state.note_id.borrow().clone() {
                move_note_to_folder(&state, &note_id, folder_id);
            }
            return;
        }
        match action.as_str() {
            "pin" => state.pin_button.emit_clicked(),
            "archive" | "restore" => state.archive_button.emit_clicked(),
            "delete" => state.delete_button.emit_clicked(),
            "folder" => show_move_note_dialog(&state),
            _ => {}
        }
    });

    let state_for_context = Rc::downgrade(state);
    let row_for_context = row.downgrade();
    let popover_for_context = context_popover.downgrade();
    let items_for_context = context_items.downgrade();
    let click = gtk::GestureClick::new();
    click.set_button(3);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _, x, y| {
        let (Some(state), Some(row), Some(popover), Some(items)) = (
            state_for_context.upgrade(),
            row_for_context.upgrade(),
            popover_for_context.upgrade(),
            items_for_context.upgrade(),
        ) else {
            return;
        };
        let note_id = row.widget_name();
        let note = state.store.borrow().note(&note_id).ok().flatten();
        let Some(note) = note else {
            return;
        };
        select_note_row(&state, &row);

        while let Some(child) = items.first_child() {
            items.remove(&child);
        }
        let (pin_label, action_label, action_icon) = (
            if note.pinned {
                "Losmaken"
            } else {
                "Vastpinnen"
            },
            if note.archived {
                "Terugzetten"
            } else if note.content.trim().is_empty() {
                "Definitief wissen"
            } else {
                "Archiveer"
            },
            if note.archived {
                "document-revert-symbolic"
            } else {
                "user-trash-symbolic"
            },
        );
        items.append(&folder_menu_item(
            "view-pin-symbolic",
            pin_label,
            "pin",
            false,
        ));
        items.append(&folder_menu_item(
            action_icon,
            action_label,
            if note.archived { "restore" } else { "archive" },
            false,
        ));
        if note.archived {
            items.append(&folder_menu_item(
                "user-trash-symbolic",
                "Definitief wissen",
                "delete",
                true,
            ));
        } else {
            items.append(&folder_menu_item(
                "folder-symbolic",
                "Verplaats naar map",
                "folder",
                false,
            ));
        }
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    row.add_controller(click);
    let popover_for_destroy = context_popover.downgrade();
    row.connect_destroy(move |_| {
        if let Some(popover) = popover_for_destroy.upgrade() {
            popover.unparent();
        }
    });

    let state_for_click = Rc::downgrade(state);
    let row_for_click = row.downgrade();
    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _, _, _| {
        let (Some(state_for_click), Some(row_for_click)) =
            (state_for_click.upgrade(), row_for_click.upgrade())
        else {
            return;
        };
        let modifiers = gesture.current_event_state();
        if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
            let note_id = row_for_click.widget_name().to_string();
            let anchor = state_for_click
                .selection_anchor
                .borrow()
                .clone()
                .unwrap_or_else(|| note_id.clone());
            show_note_from_row(&state_for_click, &row_for_click);
            let state_for_range = Rc::clone(&state_for_click);
            gtk::glib::idle_add_local_once(move || {
                select_note_range(&state_for_range, &anchor, &note_id);
            });
        } else if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            *state_for_click.selection_anchor.borrow_mut() =
                Some(row_for_click.widget_name().to_string());
            show_note_from_row(&state_for_click, &row_for_click);
        } else if state_for_click
            .note_list
            .selected_ids()
            .iter()
            .any(|id| id == row_for_click.widget_name().as_str())
        {
            show_note_from_row(&state_for_click, &row_for_click);
        } else {
            select_note_row(&state_for_click, &row_for_click);
        }
    });
    row.add_controller(click);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    let note_id = note.id.clone();
    let state_for_drag = Rc::downgrade(state);
    drag_source.connect_prepare(move |_, _, _| {
        let state_for_drag = state_for_drag.upgrade()?;
        let selected_ids = state_for_drag.note_list.selected_ids();
        let dragged_ids = if selected_ids.iter().any(|id| id == &note_id) {
            selected_ids
        } else {
            vec![note_id.clone()]
        };
        let value = dragged_ids.join("\n").to_value();
        Some(gtk::gdk::ContentProvider::for_value(&value))
    });
    let drag_icon_note_id = note.id.clone();
    let state_for_drag_icon = Rc::downgrade(state);
    drag_source.connect_drag_begin(move |_, drag| {
        let Some(state_for_drag_icon) = state_for_drag_icon.upgrade() else {
            return;
        };
        let selected_ids = state_for_drag_icon.note_list.selected_ids();
        let count = if selected_ids.iter().any(|id| id == &drag_icon_note_id) {
            selected_ids.len()
        } else {
            1
        };
        let icon = build_note_drag_icon("sleep naar map", count);
        let drag_icon = gtk::DragIcon::for_drag(drag);
        drag_icon.add_css_class("note-drag-icon");
        drag_icon.set_child(Some(&icon));
        drag.set_hotspot(-12, -6);
    });
    row.add_controller(drag_source);
    row
}

fn build_note_drag_icon(title: &str, count: usize) -> gtk::Overlay {
    let preview = gtk::Overlay::new();
    preview.add_css_class("note-drag-preview");
    preview.set_size_request(184, 56);
    let clear_background = gtk::Box::new(gtk::Orientation::Vertical, 0);
    clear_background.add_css_class("note-drag-preview-clear");
    preview.set_child(Some(&clear_background));

    if count > 1 {
        for offset in [6] {
            let back_card = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            back_card.add_css_class("note-drag-preview-back");
            back_card.set_size_request(164, 42);
            back_card.set_halign(gtk::Align::Start);
            back_card.set_valign(gtk::Align::Start);
            back_card.set_margin_start(offset);
            back_card.set_margin_top(offset);
            preview.add_overlay(&back_card);
        }
    }

    let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    card.add_css_class("note-drag-preview-card");
    card.set_size_request(164, 42);
    if count == 1 {
        card.add_css_class("note-drag-preview-front-shadow");
    }
    card.set_halign(gtk::Align::Start);
    card.set_valign(gtk::Align::Start);

    let icon = gtk::Image::from_icon_name("list-drag-handle-symbolic");
    icon.add_css_class("note-drag-preview-grip");
    card.append(&icon);

    let label = gtk::Label::builder()
        .label(title)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .xalign(0.0)
        .build();
    label.add_css_class("note-drag-preview-title");
    card.append(&label);

    let count_badge = gtk::Label::builder()
        .label(count.to_string())
        .xalign(0.5)
        .valign(gtk::Align::Center)
        .build();
    count_badge.add_css_class("note-drag-preview-count");
    card.append(&count_badge);
    preview.add_overlay(&card);

    preview
}

fn move_notes_to_folder(state: &Rc<WindowState>, note_ids: &[String], folder_id: &str) -> bool {
    if note_ids.is_empty() {
        return false;
    }

    let mut all_moved = true;
    for note_id in note_ids {
        if state
            .store
            .borrow()
            .move_note(note_id, Some(folder_id))
            .is_err()
        {
            all_moved = false;
        }
    }

    let selected_note_id = state.note_id.borrow().clone();
    if let Some(selected_note_id) = selected_note_id
        .as_ref()
        .filter(|selected_id| note_ids.contains(selected_id))
    {
        let selected_note = { state.store.borrow().note(selected_note_id).ok().flatten() };
        if let Some(note) = selected_note {
            show_note(state, &note);
        }
    }

    let refreshed = refresh_notes(state).is_ok();
    all_moved && refreshed
}

fn show_move_note_dialog(state: &Rc<WindowState>) {
    let Some(note_id) = state.note_id.borrow().clone() else {
        return;
    };
    let note = state.store.borrow().note(&note_id).ok().flatten();
    let Some(note) = note.filter(|note| !note.archived) else {
        return;
    };
    let Some(parent) = state.note_list.view.root().and_downcast::<gtk::Window>() else {
        return;
    };

    sync_folder_picker(state, note.account_id.as_deref(), note.folder_id.as_deref());
    let model = state
        .folder_picker
        .model()
        .and_downcast::<gtk::StringList>()
        .expect("folder dropdown uses a GtkStringList");
    let picker = gtk::DropDown::new(Some(model), None::<&gtk::Expression>);
    picker.set_selected(state.folder_picker.selected());
    let dialog = gtk::Dialog::builder()
        .title(tr("note.move_to_folder"))
        .transient_for(&parent)
        .modal(true)
        .default_width(360)
        .build();
    dialog.add_button(tr("action.cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(tr("action.move"), gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let label = gtk::Label::builder()
        .label(tr("folder.title"))
        .xalign(0.0)
        .build();
    content.append(&label);
    content.append(&picker);
    dialog.content_area().append(&content);

    let state_for_response = Rc::clone(state);
    let options = state.folder_options.borrow().clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept
            && let Some(folder_id) = options.get(picker.selected() as usize)
        {
            move_note_to_folder(&state_for_response, &note_id, folder_id.as_deref());
        }
        dialog.close();
    });
    dialog.present();
}

fn move_note_to_folder(state: &Rc<WindowState>, note_id: &str, folder_id: Option<&str>) -> bool {
    let current_folder = state
        .store
        .borrow()
        .note(note_id)
        .ok()
        .flatten()
        .and_then(|note| note.folder_id);
    if current_folder.as_deref() == folder_id {
        return true;
    }
    if state.store.borrow().move_note(note_id, folder_id).is_err() {
        return false;
    }
    refresh_notes(state).is_ok()
}

fn show_note_from_row(state: &Rc<WindowState>, row: &gtk::Box) {
    show_note_by_id(state, row.widget_name().as_str());
}

fn show_note_by_id(state: &Rc<WindowState>, note_id: &str) {
    state.note_list_focus.grab_focus();
    let note = { state.store.borrow().note(note_id).ok().flatten() };
    if let Some(note) = note {
        if note.account_id.is_some()
            && let Some(window) = state
                .note_list
                .view
                .root()
                .and_downcast::<ApplicationWindow>()
        {
            refresh_backend_note(state, &window, &note);
        }
        show_note(state, &note);
    }
}

fn select_note_row(state: &Rc<WindowState>, row: &gtk::Box) {
    state.note_list.select(row.widget_name().as_str(), true);
    *state.selection_anchor.borrow_mut() = Some(row.widget_name().to_string());
    show_note_from_row(state, row);
}

fn select_note_range(state: &Rc<WindowState>, anchor_id: &str, target_id: &str) {
    state.note_list.select_range(anchor_id, target_id);
}

fn move_note_selection(state: &Rc<WindowState>, direction: i32) -> gtk::glib::Propagation {
    let current_id = state.note_id.borrow().clone();
    if let Some(id) = state.note_list.next_id(current_id.as_deref(), direction) {
        state.note_list.select(&id, true);
        *state.selection_anchor.borrow_mut() = Some(id.clone());
        state.note_list.scroll_to(&id);
        show_note_by_id(state, &id);
    }
    gtk::glib::Propagation::Stop
}

fn refresh_folder_counts(state: &Rc<WindowState>) -> Result<()> {
    let store = state.store.borrow();
    for entry in state.sidebar_entries.borrow().iter() {
        let count = store.count_notes(&entry.filter.to_note_filter(String::new()))?;
        entry.count_label.set_text(&count.to_string());
    }
    Ok(())
}

/// Fills the "move to folder" picker with the folders of one account only.
fn sync_folder_picker(
    state: &WindowState,
    account_id: Option<&str>,
    selected_folder: Option<&str>,
) {
    let Ok(folders) = state.store.borrow().folders() else {
        return;
    };
    let mut names = vec!["Zonder map".to_owned()];
    let mut options = vec![None];
    while let Some(child) = state.folder_menu.first_child() {
        state.folder_menu.remove(&child);
    }
    state.folder_menu.append(&folder_menu_item(
        "folder-symbolic",
        "Zonder map",
        "folder-choice:unfiled",
        false,
    ));
    for folder in folders
        .into_iter()
        .filter(|folder| folder.account_id.as_deref() == account_id)
    {
        names.push(folder.name.clone());
        options.push(Some(folder.id.clone()));
        state.folder_menu.append(&folder_menu_item(
            "folder-symbolic",
            &folder.name,
            &format!("folder-choice:{}", folder.id),
            false,
        ));
    }
    let index = options
        .iter()
        .position(|option| option.as_deref() == selected_folder)
        .unwrap_or(0);
    let model = state
        .folder_picker
        .model()
        .and_downcast::<gtk::StringList>()
        .expect("folder dropdown uses a GtkStringList");
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    model.splice(0, model.n_items(), &names);
    *state.folder_options.borrow_mut() = options;
    state.folder_picker.set_selected(index as u32);
}

fn resync_folder_picker(state: &WindowState) {
    let note_id = state.note_id.borrow().clone();
    let note = note_id.and_then(|id| state.store.borrow().note(&id).ok().flatten());
    match note {
        Some(note) => {
            sync_folder_picker(state, note.account_id.as_deref(), note.folder_id.as_deref())
        }
        None => {
            let active = state.active_filter.borrow().clone();
            sync_folder_picker(
                state,
                active.account_id().as_deref(),
                active.folder_id().as_deref(),
            );
        }
    }
}

fn format_updated_at(timestamp: i64) -> String {
    let Ok(updated_at) = gtk::glib::DateTime::from_unix_local(timestamp / 1000) else {
        return String::new();
    };
    let Ok(now) = gtk::glib::DateTime::now_local() else {
        return String::new();
    };

    let is_today = updated_at.year() == now.year()
        && updated_at.month() == now.month()
        && updated_at.day_of_month() == now.day_of_month();
    let format = if is_today {
        "%H:%M"
    } else if now
        .add_years(-1)
        .map(|one_year_ago| updated_at.to_unix() < one_year_ago.to_unix())
        .unwrap_or(false)
    {
        "%Y"
    } else {
        "%d-%m"
    };

    updated_at
        .format(format)
        .map(|formatted| formatted.to_string())
        .unwrap_or_default()
}

fn update_note_row_widgets(row: &gtk::Box, note: &Note, folder_name: &str) {
    let Some(item) = row.first_child().and_downcast::<gtk::Box>() else {
        return;
    };
    let Some(heading_row) = item.first_child().and_downcast::<gtk::Box>() else {
        return;
    };
    if let Some(pin_icon) = heading_row.first_child().and_downcast::<gtk::Image>() {
        pin_icon.set_visible(note.pinned);
    }
    if let Some(title) = heading_row
        .first_child()
        .and_then(|icon| icon.next_sibling())
        .and_downcast::<gtk::Label>()
    {
        title.set_text(if note.title.is_empty() {
            "Nieuwe notitie"
        } else {
            &note.title
        });
    }
    if let Some(metadata) = heading_row.last_child().and_downcast::<gtk::Box>() {
        if let Some(updated_at) = metadata.first_child().and_downcast::<gtk::Label>() {
            updated_at.set_text(&format_updated_at(note.updated_at));
        }
    }
    if let Some(preview_row) = heading_row.next_sibling().and_downcast::<gtk::Box>() {
        if let Some(preview) = preview_row.first_child().and_downcast::<gtk::Label>() {
            preview.set_text(&note_preview_text(note));
        }
        if let Some(folder) = preview_row.last_child().and_downcast::<gtk::Label>() {
            folder.set_text(folder_name);
        }
    }
}

fn show_note(state: &Rc<WindowState>, note: &Note) {
    *state.note_id.borrow_mut() = Some(note.id.clone());
    state.pin_button.set_sensitive(true);
    state.folder_button.set_sensitive(!note.archived);
    state.folder_picker.set_sensitive(!note.archived);
    set_icon_button(
        &state.pin_button,
        "view-pin-symbolic",
        if note.pinned {
            "Losmaken"
        } else {
            "Vastpinnen"
        },
    );
    state.archive_button.set_sensitive(true);
    update_note_action_button(&state.archive_button, note);
    state.delete_button.set_visible(note.archived);
    sync_folder_picker(state, note.account_id.as_deref(), note.folder_id.as_deref());
    set_editor_text(state, &note.editor_document());
    set_note_surfaces_color(&state.note_surfaces.borrow(), &state.note_color.borrow());
}

fn note_preview_text(note: &Note) -> String {
    markdown_preview_text(note.editor_parts().1)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn open_note_window(application: &gtk::Application, state: &Rc<WindowState>, note: &Note) {
    if let Some(window) = state.note_windows.borrow().get(&note.id) {
        window.present();
        return;
    }

    let window = ApplicationWindow::builder()
        .application(application)
        .title(if note.title.is_empty() {
            "Nieuwe notitie"
        } else {
            &note.title
        })
        .default_width(720)
        .default_height(560)
        .build();
    window.set_icon_name(Some("icon"));

    let content = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(36)
        .bottom_margin(48)
        .left_margin(44)
        .right_margin(44)
        .build();
    content.add_css_class("note-detail-surface");
    prepare_empty_heading(&content);
    set_editor_view_content(&content, &note.editor_document());
    style_note_heading(&content.buffer());
    let image_history = new_editor_image_history();
    let format_toolbar = editor_format_toolbar(&content, Rc::clone(&image_history));
    install_editor_format_shortcuts(&content, image_history);

    let editor = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor.add_css_class("note-detail-surface");
    editor.append(&format_toolbar);
    let content_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&content)
        .build();
    content_scroll.add_css_class("note-detail-surface");
    editor.append(&content_scroll);
    let mut note_surfaces = collect_note_surfaces(&editor.clone().upcast());
    let note_header = HeaderBar::new();
    note_header.add_css_class("note-detail-surface");
    note_surfaces.push(note_header.clone().upcast());
    set_note_surfaces_color(&note_surfaces, &state.note_color.borrow());
    let toolbar = ToolbarView::new();
    toolbar.add_top_bar(&note_header);
    toolbar.set_content(Some(&editor));
    window.set_content(Some(&toolbar));
    install_close_shortcut(&window);

    let note_id = note.id.clone();
    let state_for_close = Rc::downgrade(state);
    window.connect_close_request(move |_| {
        if let Some(state) = state_for_close.upgrade() {
            state.note_windows.borrow_mut().remove(&note_id);
            state.note_window_editors.borrow_mut().remove(&note_id);
        }
        gtk::glib::Propagation::Proceed
    });

    let buffer = content.buffer();
    let content_for_change = content.downgrade();
    let note_id = note.id.clone();
    let state_for_content = Rc::downgrade(state);
    let content_handler = buffer.connect_changed(move |buffer| {
        let Some(editor) = content_for_change.upgrade() else {
            return;
        };
        if editor.has_css_class("editor-image-moving")
            || editor.has_css_class("editor-content-replacing")
        {
            return;
        }
        style_note_heading(buffer);
        let Some(state) = state_for_content.upgrade() else {
            return;
        };
        save_detached_note(&state, &note_id, buffer);
        if editor.has_css_class("editor-image-commit") {
            let account_id = state
                .store
                .borrow()
                .note(&note_id)
                .ok()
                .flatten()
                .and_then(|note| note.account_id);
            upload_now(&state, account_id.as_deref());
        }
    });
    let handlers = RefCell::new(Some(content_handler));
    let buffer_for_destroy = buffer.downgrade();
    let state_for_destroy = Rc::downgrade(state);
    let note_id_for_destroy = note.id.clone();
    window.connect_destroy(move |window| {
        if let Some(content_handler) = handlers.borrow_mut().take() {
            if let Some(buffer) = buffer_for_destroy.upgrade() {
                buffer.disconnect(content_handler);
            }
        }
        for action in window.list_actions() {
            window.remove_action(&action);
        }
        if let Some(state) = state_for_destroy.upgrade() {
            state.note_windows.borrow_mut().remove(&note_id_for_destroy);
            state
                .note_window_editors
                .borrow_mut()
                .remove(&note_id_for_destroy);
        }
    });

    state
        .note_windows
        .borrow_mut()
        .insert(note.id.clone(), window.clone());
    state
        .note_window_editors
        .borrow_mut()
        .insert(note.id.clone(), content.downgrade());
    window.present();
    content.grab_focus();
}

fn refresh_open_note_windows(state: &Rc<WindowState>) {
    let note_ids: Vec<_> = state.note_window_editors.borrow().keys().cloned().collect();
    for note_id in note_ids {
        let Some(note) = state.store.borrow().note(&note_id).ok().flatten() else {
            continue;
        };
        let editors = state.note_window_editors.borrow().get(&note_id).cloned();
        let Some(editor) = editors.and_then(|editor| editor.upgrade()) else {
            state.note_window_editors.borrow_mut().remove(&note_id);
            continue;
        };
        let buffer = editor.buffer();
        if let Some(window) = state.note_windows.borrow().get(&note_id) {
            window.set_title(Some(if note.title.is_empty() {
                "Nieuwe notitie"
            } else {
                &note.title
            }));
        }
        let previous_suppress = state.suppress_detached_editor_events.replace(true);
        set_editor_view_content(&editor, &note.editor_document());
        style_note_heading(&buffer);
        state.suppress_detached_editor_events.set(previous_suppress);
    }
}

// Image moves and resizes are deliberate, so they skip the typing debounce.
fn upload_now(state: &Rc<WindowState>, account_id: Option<&str>) {
    let Some(account_id) = account_id else {
        return;
    };
    state.upload_after.borrow_mut().remove(account_id);
    if let Err(error) = upload_backend_events(state) {
        eprintln!("{error:#}");
    }
}

fn debounce_backend_save(state: &WindowState, account_id: Option<&str>) {
    if let Some(account_id) = account_id {
        state
            .upload_after
            .borrow_mut()
            .insert(account_id.to_owned(), Instant::now() + SAVE_DEBOUNCE);
    }
}

fn save_detached_note(state: &Rc<WindowState>, note_id: &str, buffer: &gtk::TextBuffer) {
    if state.suppress_detached_editor_events.get() {
        return;
    }
    let content = markdown_from_editor_buffer(buffer);
    let (title, body) = memonotes::memo::editor_parts(content.as_str());
    let Some(current) = state.store.borrow().note(note_id).ok().flatten() else {
        return;
    };
    if current.content == memonotes::memo::editor_content(title, body) {
        return;
    }
    if state
        .store
        .borrow()
        .save_note_editor(note_id, title, body)
        .is_ok()
    {
        debounce_backend_save(state, current.account_id.as_deref());
        schedule_note_list_refresh(state);
    }
}

fn save_current(state: &Rc<WindowState>, content: &str) {
    if state.suppress_editor_events.get() {
        return;
    }
    let (title, body) = memonotes::memo::editor_parts(content);

    let (note_id, created_now) = if let Some(note_id) = state.note_id.borrow().clone() {
        (note_id, false)
    } else {
        if state.active_filter.borrow().is_archived()
            || (title.trim().is_empty() && body.trim().is_empty())
        {
            return;
        }
        let active = state.active_filter.borrow().clone();
        let create_result = {
            state.store.borrow().create_note(
                active.account_id().as_deref(),
                active.folder_id().as_deref(),
            )
        };
        let Ok(note_id) = create_result else {
            return;
        };
        *state.note_id.borrow_mut() = Some(note_id.clone());
        (note_id, true)
    };

    let current = state.store.borrow().note(&note_id).ok().flatten();
    if let Some(current) = current {
        if current.content == memonotes::memo::editor_content(title, body) {
            return;
        }
        if state
            .store
            .borrow()
            .save_note_editor(&note_id, title, body)
            .is_ok()
        {
            debounce_backend_save(state, current.account_id.as_deref());
            if let Ok(Some(updated)) = state.store.borrow().note(&note_id) {
                update_note_action_button(&state.archive_button, &updated);
            }
            if created_now {
                state.pin_button.set_sensitive(true);
                set_icon_button(&state.pin_button, "view-pin-symbolic", "Vastpinnen");
                state.archive_button.set_sensitive(true);
                update_archive_button(&state.archive_button, false);
                state.folder_button.set_sensitive(true);
                state.folder_picker.set_sensitive(true);
                resync_folder_picker(state);
                let _ = refresh_notes(state);
            } else {
                schedule_note_list_refresh(state);
            }
        }
    }
}

fn editor_format_toolbar(
    editor: &gtk::TextView,
    image_history: SharedEditorImageHistory,
) -> gtk::Box {
    let buffer = editor.buffer();
    buffer.set_enable_undo(true);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toolbar.add_css_class("linked");
    toolbar.set_halign(gtk::Align::Start);
    toolbar.set_margin_start(44);
    toolbar.set_margin_top(8);

    let undo_button = icon_button("edit-undo-symbolic", tr("editor.undo"));
    undo_button.set_size_request(36, 36);
    let undo_button_weak = undo_button.downgrade();
    let redo_button = icon_button("edit-redo-symbolic", tr("editor.redo"));
    redo_button.set_size_request(36, 36);
    let redo_button_weak = redo_button.downgrade();
    let update_buttons: Rc<dyn Fn(&gtk::TextBuffer)> = {
        let history = Rc::clone(&image_history);
        Rc::new(move |buffer| {
            let history = history.borrow();
            if let Some(button) = undo_button_weak.upgrade() {
                button.set_sensitive(buffer.can_undo() || !history.undo.is_empty());
            }
            if let Some(button) = redo_button_weak.upgrade() {
                button.set_sensitive(buffer.can_redo() || !history.redo.is_empty());
            }
        })
    };
    let editor_for_history = editor.downgrade();
    let history_for_change = Rc::clone(&image_history);
    buffer.connect_changed(move |_| {
        let Some(editor) = editor_for_history.upgrade() else {
            return;
        };
        let wipe_native_undo = {
            let mut history = history_for_change.borrow_mut();
            if history.internal_change || editor.has_css_class("editor-image-moving") {
                return;
            }
            history.redo.clear();
            if editor.has_css_class("editor-content-replacing") {
                history.undo.clear();
                history.pending_delete = None;
                return;
            }
            if let Some(before) = history.pending_delete.take() {
                // GTK would undo a deleted image into a bare placeholder character.
                let after = editor_snapshot(&editor);
                history.undo.push(EditorImageChange {
                    before,
                    after,
                    table_patch: None,
                });
                true
            } else {
                false
            }
        };
        if wipe_native_undo {
            // Resetting GTK's history inside the delete's own user action segfaults.
            history_for_change.borrow_mut().native_undo_wipe_pending = true;
            let editor = editor.downgrade();
            let history = Rc::clone(&history_for_change);
            gtk::glib::idle_add_local_once(move || {
                if let Some(editor) = editor.upgrade() {
                    wipe_native_undo_now(&editor, &history);
                }
            });
        }
        deselect_editor_image(&editor);
    });
    let update_after_change = Rc::clone(&update_buttons);
    let editor_for_update = editor.downgrade();
    buffer.connect_changed(move |buffer| {
        let loading = editor_for_update
            .upgrade()
            .is_some_and(|editor| editor.has_css_class("editor-content-replacing"));
        if !loading {
            update_after_change(buffer);
        }
    });
    let update_after_undo = Rc::clone(&update_buttons);
    buffer.connect_notify_local(Some("can-undo"), move |buffer, _| {
        update_after_undo(buffer);
    });
    let update_after_redo = Rc::clone(&update_buttons);
    buffer.connect_notify_local(Some("can-redo"), move |buffer, _| {
        update_after_redo(buffer);
    });
    update_buttons(&buffer);
    let editor_for_undo = editor.clone();
    let history_for_undo = Rc::clone(&image_history);
    undo_button.connect_clicked(move |_| undo_editor_change(&editor_for_undo, &history_for_undo));
    toolbar.append(&undo_button);

    let editor_for_redo = editor.clone();
    let history_for_redo = Rc::clone(&image_history);
    redo_button.connect_clicked(move |_| redo_editor_change(&editor_for_redo, &history_for_redo));
    toolbar.append(&redo_button);

    for (icon, tooltip, mark) in [
        (
            "format-text-bold-symbolic",
            "editor.format.bold",
            Mark::Bold,
        ),
        (
            "format-text-italic-symbolic",
            "editor.format.italic",
            Mark::Italic,
        ),
        (
            "format-text-underline-symbolic",
            "editor.format.underline",
            Mark::Underline,
        ),
    ] {
        let button = icon_button(icon, tr(tooltip));
        button.set_size_request(36, 36);
        let buffer = buffer.clone();
        button.connect_clicked(move |_| apply_inline_style(&buffer, mark));
        toolbar.append(&button);
    }

    let link_button = icon_button("insert-link-symbolic", tr("editor.format.link"));
    link_button.set_size_request(36, 36);
    let editor_for_link = editor.clone();
    link_button.connect_clicked(move |_| edit_editor_link(&editor_for_link));
    toolbar.append(&link_button);

    let image_button = icon_button("insert-image-symbolic", tr("editor.image.insert"));
    image_button.set_size_request(36, 36);
    let editor_for_image = editor.clone();
    let history_for_image = Rc::clone(&image_history);
    image_button
        .connect_clicked(move |_| choose_editor_image(&editor_for_image, &history_for_image));
    toolbar.append(&image_button);

    let table_button = icon_button("view-grid-symbolic", tr("editor.format.table"));
    table_button.set_size_request(36, 36);
    let editor_for_table = editor.clone();
    table_button.connect_clicked(move |_| editor_table::insert_table(&editor_for_table));
    toolbar.append(&table_button);

    toolbar.append(&editor_heading_menu(editor));

    for (icon, tooltip, kind) in [
        (
            "checkbox-checked-symbolic",
            "editor.format.task_list",
            ListPrefixKind::Task,
        ),
        (
            "view-list-bullet-symbolic",
            "editor.format.bullet_list",
            ListPrefixKind::Bullet,
        ),
    ] {
        let button = icon_button(icon, tr(tooltip));
        button.set_size_request(36, 36);
        let editor = editor.clone();
        button.connect_clicked(move |_| toggle_list_prefix(&editor, kind));
        toolbar.append(&button);
    }

    let numbered_button = icon_button(
        "view-list-ordered-symbolic",
        tr("editor.format.numbered_list"),
    );
    numbered_button.set_size_request(36, 36);
    let editor_for_numbered = editor.clone();
    numbered_button.connect_clicked(move |_| toggle_numbered_list(&editor_for_numbered));
    toolbar.append(&numbered_button);

    toolbar
}

fn choose_editor_image(editor: &gtk::TextView, image_history: &SharedEditorImageHistory) {
    let parent = editor.root().and_downcast::<gtk::Window>();
    let dialog = gtk::FileChooserNative::new(
        Some(tr("editor.image.insert")),
        parent.as_ref(),
        gtk::FileChooserAction::Open,
        Some(tr("editor.image.insert")),
        Some(tr("action.cancel")),
    );
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(tr("editor.image.insert")));
    filter.add_mime_type("image/*");
    dialog.add_filter(&filter);

    let editor = editor.clone();
    let image_history = Rc::clone(image_history);
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept
            && let Some(path) = dialog.file().and_then(|file| file.path())
            && let Err(error) = insert_editor_image_file(&editor, &path, &image_history)
        {
            eprintln!("{}", error);
        }
        dialog.hide();
    });
    dialog.show();
}

fn insert_editor_image_file(
    editor: &gtk::TextView,
    source: &std::path::Path,
    image_history: &SharedEditorImageHistory,
) -> Result<()> {
    let file_name = source
        .file_name()
        .context("the selected image has no file name")?;
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or("img")
        .to_ascii_lowercase();
    let directory = editor_image_directory()?;
    std::fs::create_dir_all(&directory).context("could not create the image directory")?;
    let destination = directory.join(format!("{}.{}", Uuid::new_v4(), extension));
    std::fs::copy(source, &destination).context("could not copy the selected image")?;
    let image_name = destination
        .file_name()
        .context("the copied image has no file name")?
        .to_string_lossy();
    let uri = format!("memonotes-image://{image_name}");
    let alt = source
        .file_stem()
        .unwrap_or(file_name)
        .to_string_lossy()
        .into_owned();
    let before = begin_editor_image_change(editor, image_history);
    insert_editor_image(editor, &uri, &alt);
    finish_editor_image_change(editor, image_history, before);
    Ok(())
}

fn begin_editor_image_change(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
) -> EditorSnapshot {
    let before = editor_snapshot(editor);
    editor.add_css_class("editor-image-moving");
    image_history.borrow_mut().internal_change = true;
    editor.buffer().set_enable_undo(false);
    before
}

fn editor_snapshot(editor: &gtk::TextView) -> EditorSnapshot {
    let buffer = editor.buffer();
    EditorSnapshot {
        markdown: markdown_from_editor_buffer(&buffer),
        cursor: buffer.iter_at_mark(&buffer.get_insert()).offset(),
    }
}

fn finish_editor_image_change(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
    before: EditorSnapshot,
) {
    let buffer = editor.buffer();
    buffer.set_enable_undo(true);
    let after = editor_snapshot(editor);
    {
        let mut history = image_history.borrow_mut();
        history.undo.push(EditorImageChange {
                    before,
                    after,
                    table_patch: None,
                });
        history.redo.clear();
    }
    editor.remove_css_class("editor-image-moving");
    editor.add_css_class("editor-image-commit");
    buffer.emit_by_name::<()>("changed", &[]);
    editor.remove_css_class("editor-image-commit");
    image_history.borrow_mut().internal_change = false;
}

fn editor_image_directory() -> Result<PathBuf> {
    memonotes::images::image_directory()
}

fn undo_editor_image_change(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
) -> bool {
    restore_editor_image_change(editor, image_history, true)
}

fn redo_editor_image_change(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
) -> bool {
    restore_editor_image_change(editor, image_history, false)
}

// Image steps wipe GTK's undo stack, so anything GTK can still undo is newer than the image history.
fn wipe_native_undo_now(editor: &gtk::TextView, image_history: &SharedEditorImageHistory) {
    if !std::mem::take(&mut image_history.borrow_mut().native_undo_wipe_pending) {
        return;
    }
    let buffer = editor.buffer();
    buffer.set_enable_undo(false);
    buffer.set_enable_undo(true);
}

fn undo_editor_change(editor: &gtk::TextView, image_history: &SharedEditorImageHistory) {
    wipe_native_undo_now(editor, image_history);
    let buffer = editor.buffer();
    if buffer.can_undo() {
        buffer.undo();
    } else {
        undo_editor_image_change(editor, image_history);
    }
}

fn redo_editor_change(editor: &gtk::TextView, image_history: &SharedEditorImageHistory) {
    if !redo_editor_image_change(editor, image_history) {
        editor.buffer().redo();
    }
}

fn restore_editor_image_change(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
    undo: bool,
) -> bool {
    let change = {
        let mut history = image_history.borrow_mut();
        let change = if undo {
            history.undo.pop()
        } else {
            history.redo.pop()
        };
        let Some(change) = change else {
            return false;
        };
        history.internal_change = true;
        change
    };
    editor.add_css_class("editor-image-moving");
    let patched = change
        .table_patch
        .as_ref()
        .is_some_and(|patch| patch.apply(undo));
    if !patched {
        let target = if undo { &change.before } else { &change.after };
        let scroll = editor.vadjustment().map(|adjustment| adjustment.value());
        set_editor_view_content(editor, &target.markdown);
        if let Some(scroll) = scroll {
            keep_editor_scroll(editor, scroll);
        }
        let buffer = editor.buffer();
        buffer.place_cursor(&buffer.iter_at_offset(target.cursor.min(buffer.char_count())));
    }
    editor.remove_css_class("editor-image-moving");
    {
        let mut history = image_history.borrow_mut();
        if undo {
            history.redo.push(change);
        } else {
            history.undo.push(change);
        }
    }
    editor.buffer().emit_by_name::<()>("changed", &[]);
    image_history.borrow_mut().internal_change = false;
    true
}

// Rebuilt content is laid out over many frames, so the old position is re-applied until it holds.
fn keep_editor_scroll(editor: &gtk::TextView, value: f64) {
    let Some(adjustment) = editor.vadjustment() else {
        return;
    };
    let frames = Cell::new(0);
    let stable_frames = Cell::new(0);
    editor.add_tick_callback(move |_, _| {
        adjustment.set_value(value);
        frames.set(frames.get() + 1);
        if (adjustment.value() - value).abs() < 1.0 {
            stable_frames.set(stable_frames.get() + 1);
        } else {
            stable_frames.set(0);
        }
        if stable_frames.get() >= 15 || frames.get() >= 240 {
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
}

fn insert_editor_image(editor: &gtk::TextView, uri: &str, alt: &str) {
    let buffer = editor.buffer();
    let mut position = buffer.iter_at_mark(&buffer.get_insert());
    let anchor = buffer.create_child_anchor(&mut position);
    insert_editor_image_at_anchor(editor, &anchor, uri, alt);
    editor.grab_focus();
}

fn insert_editor_image_at_anchor(
    editor: &gtk::TextView,
    anchor: &gtk::TextChildAnchor,
    uri: &str,
    alt: &str,
) {
    let widget = editor_image_widget(uri, alt);
    attach_editor_image_widget(editor, &widget, anchor);
}

fn attach_editor_image_widget(
    editor: &gtk::TextView,
    widget: &gtk::Widget,
    anchor: &gtk::TextChildAnchor,
) {
    widget.set_focusable(true);
    widget.set_cursor_from_name(Some("grab"));
    editor.add_child_at_anchor(widget, anchor);
    install_editor_image_interactions(editor, widget, anchor);
}

fn clear_editor_image_interactions(widget: &gtk::Widget) {
    let controllers = widget.observe_controllers();
    let controllers: Vec<_> = (0..controllers.n_items())
        .filter_map(|index| controllers.item(index))
        .filter_map(|controller| controller.downcast::<gtk::EventController>().ok())
        .filter(|controller| {
            controller.is::<gtk::GestureClick>() || controller.is::<gtk::DragSource>()
        })
        .collect();
    for controller in controllers {
        widget.remove_controller(&controller);
    }
}

fn install_editor_image_interactions(
    editor: &gtk::TextView,
    widget: &gtk::Widget,
    anchor: &gtk::TextChildAnchor,
) {
    let click = gtk::GestureClick::new();
    let editor_for_click = editor.downgrade();
    let widget_for_click = widget.downgrade();
    let anchor_for_click = anchor.clone();
    click.connect_pressed(move |_, _, _, _| {
        let (Some(editor), Some(widget)) = (editor_for_click.upgrade(), widget_for_click.upgrade())
        else {
            return;
        };
        if !anchor_for_click.is_deleted() {
            select_editor_image(&editor, &widget, &anchor_for_click);
        }
    });
    widget.add_controller(click);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::COPY);
    let widget_for_drag = widget.downgrade();
    drag_source.connect_prepare(move |_, _, _| {
        let widget = widget_for_drag.upgrade()?;
        let value = gtk::glib::BoxedAnyObject::new(widget.widget_name().to_string()).to_value();
        Some(gtk::gdk::ContentProvider::for_value(&value))
    });
    widget.add_controller(drag_source);
}

fn range_has_image(start: &gtk::TextIter, end: &gtk::TextIter) -> bool {
    let mut iter = *start;
    while iter.offset() < end.offset() {
        if let Some(anchor) = iter.child_anchor()
            && anchor.widgets().iter().any(|widget| {
                widget.widget_name().starts_with("memonotes-image:")
                    || editor_table::is_table_widget(widget)
                    || editor_table::is_rule_widget(widget)
            })
        {
            return true;
        }
        iter.forward_char();
    }
    false
}

fn editor_cell_has_focus(editor: &gtk::TextView) -> bool {
    editor
        .root()
        .and_downcast::<gtk::Window>()
        .and_then(|window| GtkWindowExt::focus(&window))
        .is_some_and(|focus| focus.is_ancestor(editor) && focus.is::<gtk::Text>())
}

fn delete_selected_editor_image(editor: &gtk::TextView) -> bool {
    let Some(picture) = resize_target_picture(editor) else {
        return false;
    };
    if !picture.has_css_class("editor-image-selected") {
        return false;
    }
    let buffer = editor.buffer();
    let mut iter = buffer.start_iter();
    while iter.offset() < buffer.char_count() {
        if let Some(anchor) = iter.child_anchor()
            && anchor
                .widgets()
                .contains(picture.upcast_ref::<gtk::Widget>())
        {
            let mut start = iter;
            let mut end = iter;
            end.forward_char();
            buffer.delete(&mut start, &mut end);
            deselect_editor_image(editor);
            return true;
        }
        iter.forward_char();
    }
    false
}

fn find_image_anchor(
    buffer: &gtk::TextBuffer,
    widget: &gtk::Widget,
) -> Option<gtk::TextChildAnchor> {
    let mut iter = buffer.start_iter();
    while iter.offset() < buffer.char_count() {
        if let Some(anchor) = iter.child_anchor()
            && anchor.widgets().contains(widget)
        {
            return Some(anchor);
        }
        iter.forward_char();
    }
    None
}

fn select_editor_image(
    editor: &gtk::TextView,
    widget: &gtk::Widget,
    anchor: &gtk::TextChildAnchor,
) {
    let buffer = editor.buffer();
    deselect_editor_image(editor);
    // A buffer selection lets GtkTextView start its own text drag, which moves the anchor without its widget.
    buffer.place_cursor(&buffer.iter_at_child_anchor(anchor));
    widget.add_css_class("editor-image-selected");
    show_editor_image_handles(editor, widget);
    editor.grab_focus();
}

fn show_editor_image_handles(editor: &gtk::TextView, widget: &gtk::Widget) {
    let (Some(history), Ok(picture)) = (
        editor_image_history(editor),
        widget.clone().downcast::<gtk::Picture>(),
    ) else {
        return;
    };
    ensure_editor_image_handles(editor, &history);
    history.borrow_mut().resize_target = Some(widget.downgrade());
    update_editor_cursor_visibility(editor);
    position_editor_image_handles(editor, &picture);
    // A freshly attached image has no allocation yet, so place the handles again once it has one.
    for delay in [0_u64, 60] {
        let editor = editor.downgrade();
        let picture = picture.downgrade();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
            if let (Some(editor), Some(picture)) = (editor.upgrade(), picture.upgrade())
                && resize_target_picture(&editor).as_ref() == Some(&picture)
            {
                position_editor_image_handles(&editor, &picture);
            }
        });
    }
}

// Selection is its own state: the TextView moves the cursor around clicks, so it cannot track it.
fn deselect_editor_image(editor: &gtk::TextView) {
    let Some(history) = editor_image_history(editor) else {
        return;
    };
    let target = {
        let mut history = history.borrow_mut();
        for handle in &history.handles {
            handle.set_visible(false);
        }
        history.resize_target.take()
    };
    if let Some(widget) = target.and_then(|target| target.upgrade()) {
        widget.remove_css_class("editor-image-selected");
    }
    update_editor_cursor_visibility(editor);
}

// A caret on a line with an image or table is as tall as that widget, so it is hidden there.
fn update_editor_cursor_visibility(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let mut iter = buffer.iter_at_line(cursor.line()).unwrap_or(cursor);
    let mut tall_line = false;
    while !iter.ends_line() {
        if iter.char() == '\u{fffc}'
            && iter.child_anchor().is_some_and(|anchor| {
                anchor.widgets().iter().any(|widget| {
                    widget.widget_name().starts_with("memonotes-image:")
                        || editor_table::is_table_widget(widget)
                        || editor_table::is_rule_widget(widget)
                })
            })
        {
            tall_line = true;
            break;
        }
        if !iter.forward_char() {
            break;
        }
    }
    editor.set_cursor_visible(!tall_line && resize_target_picture(editor).is_none());
}

fn editor_image_at(editor: &gtk::TextView, x: f64, y: f64) -> Option<gtk::Widget> {
    let mut widget = editor.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(current) = widget {
        if current.widget_name().starts_with("memonotes-image:") {
            return Some(current);
        }
        if current == *editor.upcast_ref::<gtk::Widget>() {
            return None;
        }
        widget = current.parent();
    }
    None
}

// Selecting here, rather than only in the picture's own gesture, does not depend on how
// GtkTextView hands the click down to its children.
fn handle_editor_press(editor: &gtk::TextView, x: f64, y: f64) {
    if image_handle_direction_at(editor, x, y).is_some() {
        return;
    }
    let Some(picture) = editor_image_at(editor, x, y) else {
        deselect_editor_image(editor);
        return;
    };
    let Some(anchor) = find_image_anchor(&editor.buffer(), &picture) else {
        return;
    };
    select_editor_image(editor, &picture, &anchor);
    let editor = editor.downgrade();
    let picture = picture.downgrade();
    // GtkTextView handles the same press afterwards; make sure that did not undo the selection.
    gtk::glib::idle_add_local_once(move || {
        let (Some(editor), Some(picture)) = (editor.upgrade(), picture.upgrade()) else {
            return;
        };
        let still_selected = resize_target_picture(&editor)
            .is_some_and(|target| target.upcast_ref::<gtk::Widget>() == &picture);
        if !still_selected && let Some(anchor) = find_image_anchor(&editor.buffer(), &picture) {
            select_editor_image(&editor, &picture, &anchor);
        }
    });
}

fn is_modifier_key(key: gtk::gdk::Key) -> bool {
    matches!(
        key,
        gtk::gdk::Key::Shift_L
            | gtk::gdk::Key::Shift_R
            | gtk::gdk::Key::Control_L
            | gtk::gdk::Key::Control_R
            | gtk::gdk::Key::Alt_L
            | gtk::gdk::Key::Alt_R
            | gtk::gdk::Key::Meta_L
            | gtk::gdk::Key::Meta_R
            | gtk::gdk::Key::Super_L
            | gtk::gdk::Key::Super_R
            | gtk::gdk::Key::Caps_Lock
    )
}

fn image_origin(editor: &gtk::TextView, picture: &gtk::Picture) -> Option<(i32, i32)> {
    let bounds = picture.compute_bounds(editor)?;
    Some(editor.window_to_buffer_coords(
        gtk::TextWindowType::Widget,
        bounds.x() as i32,
        bounds.y() as i32,
    ))
}

fn place_editor_image_handles(editor: &gtk::TextView, origin: (i32, i32), width: i32, height: i32) {
    let Some(history) = editor_image_history(editor) else {
        return;
    };
    let (x, y) = origin;
    let half = IMAGE_HANDLE_SIZE / 2;
    let corners = [
        (x, y),
        (x + width, y),
        (x, y + height),
        (x + width, y + height),
    ];
    let mut history = history.borrow_mut();
    history.handle_positions = corners
        .iter()
        .map(|(corner_x, corner_y)| (corner_x - half, corner_y - half))
        .collect();
    for (handle, (left, top)) in history.handles.iter().zip(&history.handle_positions) {
        editor.move_overlay(handle, *left, *top);
        handle.set_visible(true);
    }
}

fn position_editor_image_handles(editor: &gtk::TextView, picture: &gtk::Picture) {
    let Some(origin) = image_origin(editor, picture) else {
        return;
    };
    let (width, height) = picture.size_request();
    let width = if width > 0 { width } else { picture.width() };
    let height = if height > 0 { height } else { picture.height() };
    place_editor_image_handles(editor, origin, width, height);
}

const IMAGE_HANDLE_DIRECTIONS: [i32; 4] = [-1, 1, -1, 1];

const IMAGE_HANDLE_CURSORS: [&str; 4] =
    ["nwse-resize", "nesw-resize", "nesw-resize", "nwse-resize"];

// Hit-tests against the positions we assigned, since allocations lag behind while resizing.
fn image_handle_index_at(editor: &gtk::TextView, x: f64, y: f64) -> Option<usize> {
    let history = editor_image_history(editor)?;
    let history = history.borrow();
    let (x, y) = editor.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let slop = 4;
    history
        .handle_positions
        .iter()
        .zip(&history.handles)
        .position(|((left, top), handle)| {
            handle.is_visible()
                && (left - slop..=left + IMAGE_HANDLE_SIZE + slop).contains(&x)
                && (top - slop..=top + IMAGE_HANDLE_SIZE + slop).contains(&y)
        })
}

fn image_handle_direction_at(editor: &gtk::TextView, x: f64, y: f64) -> Option<i32> {
    image_handle_index_at(editor, x, y).map(|index| IMAGE_HANDLE_DIRECTIONS[index])
}

// The overlay container sits above images that were added before it existed and would swallow their clicks.
fn let_editor_overlays_pass_events_through(editor: &gtk::TextView) {
    let mut child = editor.first_child();
    while let Some(current) = child {
        if current.type_().name() == "GtkTextViewChild" {
            current.set_can_target(false);
        }
        child = current.next_sibling();
    }
}

// Overlay handles cannot be targeted, so the resize cursor follows the pointer instead.
fn connect_editor_image_handle_cursor(editor: &gtk::TextView) {
    let motion = gtk::EventControllerMotion::new();
    let editor_for_motion = editor.downgrade();
    motion.connect_motion(move |_, x, y| {
        let Some(editor) = editor_for_motion.upgrade() else {
            return;
        };
        let cursor = image_handle_index_at(&editor, x, y).map(|index| IMAGE_HANDLE_CURSORS[index]);
        if let Some(picture) = resize_target_picture(&editor) {
            picture.set_cursor_from_name(Some(cursor.unwrap_or("grab")));
        }
        editor.set_cursor_from_name(Some(cursor.unwrap_or("text")));
    });
    editor.add_controller(motion);
}

fn ensure_editor_image_handles(editor: &gtk::TextView, history: &SharedEditorImageHistory) {
    if !history.borrow().handles.is_empty() {
        return;
    }
    let mut handles = Vec::new();
    for _ in IMAGE_HANDLE_CURSORS {
        let handle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        handle.add_css_class("editor-image-handle");
        handle.set_size_request(IMAGE_HANDLE_SIZE, IMAGE_HANDLE_SIZE);
        handle.set_visible(false);
        editor.add_overlay(&handle, 0, 0);
        handles.push(handle.upcast());
    }
    let_editor_overlays_pass_events_through(editor);
    history.borrow_mut().handles = handles;
    connect_editor_image_resize(editor, history);
}

fn resize_target_picture(editor: &gtk::TextView) -> Option<gtk::Picture> {
    let history = editor_image_history(editor)?;
    let widget = history.borrow().resize_target.as_ref()?.upgrade()?;
    widget.downcast().ok()
}

fn dragged_image_width(start_width: i32, direction: i32, offset_x: f64) -> i32 {
    (f64::from(start_width) + f64::from(direction) * offset_x)
        .round()
        .clamp(f64::from(IMAGE_MIN_WIDTH), f64::from(IMAGE_MAX_WIDTH)) as i32
}

#[derive(Clone, Copy)]
struct ImageResize {
    direction: i32,
    start_width: i32,
    origin: (i32, i32),
}

// The gesture lives on the editor: a gesture on a handle sees offsets in the handle's own,
// moving coordinate space, which makes the image jitter while it is resized.
fn connect_editor_image_resize(editor: &gtk::TextView, history: &SharedEditorImageHistory) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let active: Rc<Cell<Option<ImageResize>>> = Rc::new(Cell::new(None));

    let editor_for_begin = editor.downgrade();
    let active_for_begin = Rc::clone(&active);
    drag.connect_drag_begin(move |gesture, x, y| {
        let Some(editor) = editor_for_begin.upgrade() else {
            return;
        };
        let Some((picture, direction)) =
            resize_target_picture(&editor).zip(image_handle_direction_at(&editor, x, y))
        else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        let Some(origin) = image_origin(&editor, &picture) else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        active_for_begin.set(Some(ImageResize {
            direction,
            start_width: picture_width(&picture),
            origin,
        }));
    });

    let editor_for_update = editor.downgrade();
    let active_for_update = Rc::clone(&active);
    drag.connect_drag_update(move |_, offset_x, _| {
        let (Some(editor), Some(resize)) = (editor_for_update.upgrade(), active_for_update.get())
        else {
            return;
        };
        let Some(picture) = resize_target_picture(&editor) else {
            return;
        };
        let width = dragged_image_width(resize.start_width, resize.direction, offset_x);
        let (width, height) = image_size_for_width(&picture, width);
        picture.set_size_request(width, height);
        place_editor_image_handles(&editor, resize.origin, width, height);
    });

    let editor_for_end = editor.downgrade();
    drag.connect_drag_end(move |_, offset_x, _| {
        let (Some(editor), Some(resize)) = (editor_for_end.upgrade(), active.take()) else {
            return;
        };
        let Some(picture) = resize_target_picture(&editor) else {
            return;
        };
        let width = dragged_image_width(resize.start_width, resize.direction, offset_x);
        commit_editor_image_resize(&editor, &picture, width);
    });
    history.borrow_mut().resize_gesture = Some(drag.clone());
    editor.add_controller(drag);
    connect_editor_image_handle_cursor(editor);
}

fn commit_editor_image_resize(editor: &gtk::TextView, picture: &gtk::Picture, width: i32) {
    let Some((alt, uri)) = editor_image_values(&picture.widget_name()) else {
        return;
    };
    let (width, height) = image_size_for_width(picture, width);
    picture.set_size_request(width, height);
    let new_uri = with_image_width(&uri, width);
    if new_uri != uri {
        let new_name = editor_image_name(&alt, &new_uri);
        if let Some(history) = editor_image_history(editor) {
            let before = begin_editor_image_change(editor, &history);
            picture.set_widget_name(&new_name);
            finish_editor_image_change(editor, &history, before);
        } else {
            picture.set_widget_name(&new_name);
            editor.buffer().emit_by_name::<()>("changed", &[]);
        }
    }
    position_editor_image_handles(editor, picture);
    let editor = editor.downgrade();
    let picture = picture.downgrade();
    gtk::glib::idle_add_local_once(move || {
        if let (Some(editor), Some(picture)) = (editor.upgrade(), picture.upgrade()) {
            position_editor_image_handles(&editor, &picture);
        }
    });
}

fn install_editor_image_drop_target(
    editor: &gtk::TextView,
    image_history: &SharedEditorImageHistory,
) {
    let drop_target = gtk::DropTarget::new(
        gtk::glib::BoxedAnyObject::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    let indicator: DropIndicator = Rc::new(RefCell::new(None));
    let editor_for_motion = editor.downgrade();
    let indicator_for_motion = Rc::clone(&indicator);
    drop_target.connect_motion(move |_, x, y| {
        if let Some(editor) = editor_for_motion.upgrade() {
            show_editor_image_drop_indicator(&editor, &indicator_for_motion, x, y);
        }
        gtk::gdk::DragAction::COPY
    });
    let indicator_for_leave = Rc::clone(&indicator);
    drop_target.connect_leave(move |_| hide_editor_image_drop_indicator(&indicator_for_leave));
    let editor_for_drop = editor.downgrade();
    let history_for_drop = Rc::clone(image_history);
    drop_target.connect_drop(move |_, value, x, y| {
        hide_editor_image_drop_indicator(&indicator);
        editor_for_drop
            .upgrade()
            .is_some_and(|editor| handle_editor_image_drop(&editor, value, x, y, &history_for_drop))
    });
    editor.add_controller(drop_target);
}

type DropIndicator = Rc<RefCell<Option<gtk::Widget>>>;

// Dropping beside or below a line lands after that line's text; the title line is never a target.
fn editor_drop_iter(editor: &gtk::TextView, x: f64, y: f64) -> gtk::TextIter {
    let buffer = editor.buffer();
    let (bx, by) = editor.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let (line_start, _) = editor.line_at_y(by);
    let mut line_end = line_start;
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    let end_rect = editor.iter_location(&line_end);
    let on_end_row = by >= end_rect.y() && by < end_rect.y() + end_rect.height();
    let below_text =
        line_start.line() == buffer.line_count() - 1 && by >= end_rect.y() + end_rect.height();
    let mut target = if below_text || (on_end_row && bx >= end_rect.x()) {
        line_end
    } else {
        editor.iter_at_location(bx, by).unwrap_or(line_end)
    };
    if target.line() == 0
        && let Some(second_line) = buffer.iter_at_line(1)
    {
        target = second_line;
    }
    target
}

fn show_editor_image_drop_indicator(
    editor: &gtk::TextView,
    indicator: &DropIndicator,
    x: f64,
    y: f64,
) {
    let rect = editor.iter_location(&editor_drop_iter(editor, x, y));
    let height = rect.height().max(1);
    let mut slot = indicator.borrow_mut();
    if let Some(widget) = slot.as_ref() {
        widget.set_size_request(2, height);
        editor.move_overlay(widget, rect.x(), rect.y());
        widget.set_visible(true);
        return;
    }
    let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    widget.add_css_class("editor-image-drop-indicator");
    widget.set_can_target(false);
    widget.set_size_request(2, height);
    editor.add_overlay(&widget, rect.x(), rect.y());
    let_editor_overlays_pass_events_through(editor);
    *slot = Some(widget.upcast());
}

fn hide_editor_image_drop_indicator(indicator: &DropIndicator) {
    if let Some(widget) = indicator.borrow().as_ref() {
        widget.set_visible(false);
    }
}

fn handle_editor_image_drop(
    editor: &gtk::TextView,
    value: &gtk::glib::Value,
    x: f64,
    y: f64,
    image_history: &SharedEditorImageHistory,
) -> bool {
    let Ok(payload) = value.get::<gtk::glib::BoxedAnyObject>() else {
        return false;
    };
    let metadata = payload.borrow::<String>().to_owned();
    if editor_image_values(&metadata).is_none() {
        return false;
    }
    let offset = editor_drop_iter(editor, x, y).offset();
    let editor = editor.downgrade();
    let image_history = Rc::clone(image_history);
    gtk::glib::idle_add_local_once(move || {
        if let Some(editor) = editor.upgrade() {
            move_editor_image_to_offset(&editor, &metadata, offset, &image_history);
        }
    });
    true
}

fn move_editor_image_to_offset(
    editor: &gtk::TextView,
    metadata: &str,
    target_offset: i32,
    image_history: &SharedEditorImageHistory,
) -> bool {
    if editor_image_values(metadata).is_none() {
        return false;
    }
    let buffer = editor.buffer();
    let mut iter = buffer.start_iter();
    let mut source = None;
    while iter.offset() < buffer.char_count() {
        if let Some(anchor) = iter.child_anchor()
            && let Some(widget) = anchor
                .widgets()
                .into_iter()
                .find(|widget| widget.widget_name() == metadata)
        {
            source = Some((anchor, widget, iter.offset()));
            break;
        }
        iter.forward_char();
    }
    let Some((anchor, widget, source_offset)) = source else {
        return false;
    };
    let target_offset = target_offset.clamp(0, buffer.char_count());
    if (source_offset..=source_offset + 1).contains(&target_offset) {
        return true;
    }

    let target = buffer.iter_at_offset(target_offset);
    let target_mark = buffer.create_mark(None, &target, true);
    let before = begin_editor_image_change(editor, image_history);
    clear_editor_image_interactions(&widget);
    editor.remove(&widget);
    let mut source_start = buffer.iter_at_child_anchor(&anchor);
    let mut source_end = source_start;
    source_end.forward_char();
    buffer.delete(&mut source_start, &mut source_end);

    let mut insertion = buffer.iter_at_mark(&target_mark);
    let new_anchor = buffer.create_child_anchor(&mut insertion);
    attach_editor_image_widget(editor, &widget, &new_anchor);
    finish_editor_image_change(editor, image_history, before);
    buffer.delete_mark(&target_mark);
    select_editor_image(editor, &widget, &new_anchor);
    true
}

fn editor_image_values(metadata: &str) -> Option<(String, String)> {
    let (alt, uri) = metadata.strip_prefix("memonotes-image:")?.split_once(':')?;
    Some((
        decode_editor_image_value(alt)?,
        decode_editor_image_value(uri)?,
    ))
}

fn editor_image_widget(uri: &str, alt: &str) -> gtk::Widget {
    let (base_uri, requested_width) = split_image_width(uri);
    let path = base_uri
        .strip_prefix("memonotes-image://")
        .filter(|name| {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                })
        })
        .and_then(|name| {
            editor_image_directory()
                .ok()
                .map(|directory| directory.join(name))
        })
        .or_else(|| gio::File::for_uri(base_uri).path());
    let picture = path
        .and_then(|path| gtk::gdk::Texture::from_file(&gio::File::for_path(path)).ok())
        .map(|texture| {
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_can_shrink(true);
            let width = requested_width
                .unwrap_or_else(|| texture.width().clamp(1, DEFAULT_IMAGE_WIDTH))
                .clamp(IMAGE_MIN_WIDTH, IMAGE_MAX_WIDTH);
            let (width, height) = image_size_for_width(&picture, width);
            picture.set_size_request(width, height);
            picture.set_halign(gtk::Align::Start);
            picture.upcast::<gtk::Widget>()
        });
    let widget = picture.unwrap_or_else(|| {
        let image = gtk::Image::from_icon_name("image-x-generic-symbolic");
        image.set_pixel_size(48);
        image.upcast::<gtk::Widget>()
    });
    widget.add_css_class("editor-image");
    widget.set_widget_name(&editor_image_name(alt, uri));
    widget.set_tooltip_text((!alt.is_empty()).then_some(alt));
    widget
}

const DEFAULT_IMAGE_WIDTH: i32 = 320;
const IMAGE_MIN_WIDTH: i32 = 32;
const IMAGE_MAX_WIDTH: i32 = 2000;
const IMAGE_HANDLE_SIZE: i32 = 12;

fn editor_image_name(alt: &str, uri: &str) -> String {
    format!(
        "memonotes-image:{}:{}",
        encode_editor_image_value(alt),
        encode_editor_image_value(uri)
    )
}

// The chosen width travels in the URL fragment so other Markdown clients ignore it.
fn split_image_width(uri: &str) -> (&str, Option<i32>) {
    match uri.rsplit_once("#w=") {
        Some((base, width)) => (base, width.parse().ok().filter(|width| *width > 0)),
        None => (uri, None),
    }
}

fn with_image_width(uri: &str, width: i32) -> String {
    format!("{}#w={width}", split_image_width(uri).0)
}

fn image_size_for_width(picture: &gtk::Picture, width: i32) -> (i32, i32) {
    let ratio = picture
        .paintable()
        .map_or(1.0, |paintable| paintable.intrinsic_aspect_ratio());
    let ratio = if ratio > 0.0 { ratio } else { 1.0 };
    (width, (f64::from(width) / ratio).round().max(1.0) as i32)
}

fn picture_width(picture: &gtk::Picture) -> i32 {
    let requested = picture.size_request().0;
    if requested > 0 {
        requested
    } else {
        picture.width()
    }
}

fn editor_image_markdown(widget: &gtk::Widget) -> Option<String> {
    let (alt, uri) = editor_image_values(&widget.widget_name())?;
    Some(format!("!{}", markdown_link(&alt, &uri)))
}

fn encode_editor_image_value(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_editor_image_value(value: &str) -> Option<String> {
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn editor_heading_menu(editor: &gtk::TextView) -> gtk::MenuButton {
    let pending_heading = install_pending_heading_style(editor);
    let menu_button = gtk::MenuButton::new();
    menu_button.set_label("P");
    menu_button.set_tooltip_text(Some(tr("editor.format.heading")));
    menu_button.set_size_request(36, 36);

    let menu_button_weak = menu_button.downgrade();
    let pending_for_label = Rc::clone(&pending_heading);
    let update_label: Rc<dyn Fn(&gtk::TextBuffer)> = Rc::new(move |buffer| {
        if let Some(menu_button) = menu_button_weak.upgrade() {
            menu_button.set_label(&editor_block_style_label(buffer, &pending_for_label));
        }
    });
    let buffer = editor.buffer();
    let update_after_change = Rc::clone(&update_label);
    let editor_for_update = editor.downgrade();
    buffer.connect_changed(move |buffer| {
        let loading = editor_for_update
            .upgrade()
            .is_some_and(|editor| editor.has_css_class("editor-content-replacing"));
        if !loading {
            update_after_change(buffer);
        }
    });
    let update_after_cursor_move = Rc::clone(&update_label);
    let pending_for_cursor = Rc::clone(&pending_heading);
    buffer.connect_notify_local(Some("cursor-position"), move |buffer, _| {
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        if pending_for_cursor
            .get()
            .is_some_and(|(line, _)| line != cursor.line())
        {
            pending_for_cursor.set(None);
        }
        update_after_cursor_move(buffer);
    });

    let popover = gtk::Popover::new();
    let choices = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (level, label) in [
        (None, "editor.format.paragraph"),
        (Some(1), "editor.format.heading_1"),
        (Some(2), "editor.format.heading_2"),
        (Some(3), "editor.format.heading_3"),
        (Some(4), "editor.format.heading_4"),
        (Some(5), "editor.format.heading_5"),
        (Some(6), "editor.format.heading_6"),
    ] {
        let choice = gtk::Button::with_label(tr(label));
        choice.add_css_class("flat");
        choice.set_halign(gtk::Align::Fill);
        let editor = editor.clone();
        let pending_heading = Rc::clone(&pending_heading);
        let popover = popover.clone();
        choice.connect_clicked(move |_| {
            apply_editor_block_style(&editor, level, &pending_heading);
            popover.popdown();
            editor.grab_focus();
        });
        choices.append(&choice);
    }
    popover.set_child(Some(&choices));
    menu_button.set_popover(Some(&popover));
    update_label(&buffer);
    menu_button
}

fn editor_block_style_label(
    buffer: &gtk::TextBuffer,
    pending_heading: &Cell<Option<(i32, u8)>>,
) -> String {
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let line = cursor.line();
    if let Some((pending_line, level)) = pending_heading.get()
        && pending_line == line
    {
        return format!("H{level}");
    }
    if line == 0 {
        return "H1".to_owned();
    }
    let Some(line_start) = buffer.iter_at_line(line) else {
        return "P".to_owned();
    };
    (1..=6)
        .find(|level| line_start.has_tag(&heading_style_tag(buffer, *level)))
        .map_or_else(|| "P".to_owned(), |level| format!("H{level}"))
}

fn install_pending_heading_style(editor: &gtk::TextView) -> Rc<Cell<Option<(i32, u8)>>> {
    let pending_heading = Rc::new(Cell::new(None));
    let pending_for_change = Rc::clone(&pending_heading);
    editor.buffer().connect_changed(move |buffer| {
        let Some((pending_line, level)) = pending_for_change.get() else {
            return;
        };
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        if cursor.line() != pending_line {
            return;
        }
        let Some(line_start) = buffer.iter_at_line(pending_line) else {
            return;
        };
        let mut line_end = line_start;
        line_end.forward_to_line_end();
        if buffer
            .slice(&line_start, &line_end, true)
            .to_string()
            .trim()
            .is_empty()
        {
            return;
        }
        buffer.apply_tag(&heading_style_tag(buffer, level), &line_start, &line_end);
        pending_for_change.set(None);
    });
    pending_heading
}

fn apply_editor_block_style(
    editor: &gtk::TextView,
    level: Option<u8>,
    pending_heading: &Cell<Option<(i32, u8)>>,
) {
    let buffer = editor.buffer();
    let cursor_line = buffer.iter_at_mark(&buffer.get_insert()).line();
    let selection = buffer.selection_bounds();
    let (first_line, mut last_line) = selection
        .as_ref()
        .map(|(start, end)| (start.line(), end.line()))
        .unwrap_or_else(|| {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (cursor.line(), cursor.line())
        });
    if let Some((_, end)) = selection.as_ref()
        && end.line_offset() == 0
        && last_line > first_line
    {
        last_line -= 1;
    }

    for line_number in first_line..=last_line {
        if line_number == 0 {
            continue;
        }
        let Some(line_start) = buffer.iter_at_line(line_number) else {
            continue;
        };
        let mut line_end = line_start;
        line_end.forward_to_line_end();
        let line_text = buffer.slice(&line_start, &line_end, true).to_string();
        for existing_level in 1..=6 {
            let tag = heading_style_tag(&buffer, existing_level);
            buffer.remove_tag(&tag, &line_start, &line_end);
        }
        if let Some(level) = level {
            if line_text.trim().is_empty() && line_number == cursor_line {
                pending_heading.set(Some((line_number, level)));
            } else {
                let tag = heading_style_tag(&buffer, level);
                buffer.apply_tag(&tag, &line_start, &line_end);
            }
        } else if line_text.trim().is_empty() && line_number == cursor_line {
            pending_heading.set(None);
        }
    }
    buffer.emit_by_name::<()>("changed", &[]);
}

#[derive(Clone, Copy)]
enum ListPrefixKind {
    Task,
    Bullet,
}

fn install_editor_format_shortcuts(
    editor: &gtk::TextView,
    image_history: SharedEditorImageHistory,
) {
    let buffer = editor.buffer();
    register_editor_image_history(editor, &image_history);
    install_editor_image_drop_target(editor, &image_history);
    let editor_for_delete = editor.downgrade();
    let history_for_delete = Rc::clone(&image_history);
    buffer.connect_delete_range(move |_, start, end| {
        let Some(editor) = editor_for_delete.upgrade() else {
            return;
        };
        let own_change = editor.has_css_class("editor-image-moving")
            || editor.has_css_class("editor-content-replacing");
        if !own_change && range_has_image(start, end) {
            let mut history = history_for_delete.borrow_mut();
            if !history.internal_change {
                history.pending_delete = Some(editor_snapshot(&editor));
            }
        }
    });
    let editor_for_cursor = editor.downgrade();
    buffer.connect_mark_set(move |_, _, mark| {
        if mark.name().as_deref() != Some("insert") {
            return;
        }
        if let Some(editor) = editor_for_cursor.upgrade()
            && !editor.has_css_class("editor-content-replacing")
        {
            update_editor_cursor_visibility(&editor);
        }
    });
    let editor_for_cursor = editor.downgrade();
    buffer.connect_changed(move |_| {
        if let Some(editor) = editor_for_cursor.upgrade()
            && !editor.has_css_class("editor-content-replacing")
        {
            update_editor_cursor_visibility(&editor);
        }
    });
    // The clipboard is read asynchronously, so a copied spreadsheet range can become a table.
    editor.connect_paste_clipboard(move |view| {
        if !view.is_editable() {
            return;
        }
        view.stop_signal_emission_by_name("paste-clipboard");
        let view = view.clone();
        let clipboard = view.clipboard();
        gtk::glib::MainContext::default().spawn_local(async move {
            let text = clipboard.read_text_future().await.ok().flatten();
            match text
                .as_deref()
                .and_then(memonotes::editor_document::pasted_table_rows)
            {
                Some(rows) => editor_table::insert_pasted_table(&view, &rows),
                None => {
                    let buffer = view.buffer();
                    buffer.paste_clipboard(&clipboard, None, true);
                    view.scroll_mark_onscreen(&buffer.get_insert());
                }
            }
        });
    });
    // Dragged or pasted text carries anchor characters without their widget; drop them.
    let editor_for_insert = editor.downgrade();
    buffer.connect_insert_text(move |buffer, iter, text| {
        if !text.contains('\u{fffc}') {
            return;
        }
        let Some(editor) = editor_for_insert.upgrade() else {
            return;
        };
        if editor.has_css_class("editor-content-replacing") {
            return;
        }
        buffer.stop_signal_emission_by_name("insert-text");
        let cleaned = text.replace('\u{fffc}', "");
        if !cleaned.is_empty() {
            buffer.insert(iter, &cleaned);
        }
    });
    install_bullet_selection_guard(editor);
    let auto_linking = Rc::new(Cell::new(false));
    let auto_linking_for_change = Rc::clone(&auto_linking);
    let editor_for_auto_link = editor.downgrade();
    buffer.connect_changed(move |buffer| {
        let Some(editor) = editor_for_auto_link.upgrade() else {
            return;
        };
        if editor.has_css_class("editor-content-replacing") {
            return;
        }
        if auto_linking_for_change.replace(true) {
            return;
        }
        auto_link_plain_urls(buffer);
        auto_linking_for_change.set(false);
    });
    let editor_for_enter = editor.clone();
    let image_history_for_keys = Rc::clone(&image_history);
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        // Keys typed into a table cell belong to that cell, not to the note text.
        if editor_cell_has_focus(&editor_for_enter) {
            return gtk::glib::Propagation::Proceed;
        }
        if resize_target_picture(&editor_for_enter).is_some()
            && !is_modifier_key(key)
            && !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
            && !matches!(
                key,
                gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete | gtk::gdk::Key::BackSpace
            )
        {
            deselect_editor_image(&editor_for_enter);
        }
        if matches!(key, gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab)
            && !modifiers
                .intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK)
            && indent_editor_list(
                &editor_for_enter,
                key == gtk::gdk::Key::ISO_Left_Tab
                    || modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
            )
        {
            return gtk::glib::Propagation::Stop;
        }
        if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
            && !modifiers.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SHIFT_MASK,
            )
            && continue_editor_list_on_enter(&editor_for_enter)
        {
            return gtk::glib::Propagation::Stop;
        }
        if matches!(
            key,
            gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete | gtk::gdk::Key::BackSpace
        ) && modifiers.is_empty()
            && delete_selected_editor_image(&editor_for_enter)
        {
            return gtk::glib::Propagation::Stop;
        }
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            return gtk::glib::Propagation::Proceed;
        }
        let shifted = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        match key {
            gtk::gdk::Key::z | gtk::gdk::Key::Z if shifted => {
                redo_editor_change(&editor_for_enter, &image_history_for_keys)
            }
            gtk::gdk::Key::z if !shifted => {
                undo_editor_change(&editor_for_enter, &image_history_for_keys)
            }
            gtk::gdk::Key::y | gtk::gdk::Key::Y if !shifted => {
                redo_editor_change(&editor_for_enter, &image_history_for_keys)
            }
            gtk::gdk::Key::b if !shifted => apply_inline_style(&buffer, Mark::Bold),
            gtk::gdk::Key::i if !shifted => apply_inline_style(&buffer, Mark::Italic),
            gtk::gdk::Key::u if !shifted => apply_inline_style(&buffer, Mark::Underline),
            gtk::gdk::Key::k if !shifted => edit_editor_link(&editor_for_enter),
            gtk::gdk::Key::t | gtk::gdk::Key::T if shifted => {
                toggle_list_prefix(&editor_for_enter, ListPrefixKind::Task)
            }
            gtk::gdk::Key::l | gtk::gdk::Key::L if shifted => {
                toggle_list_prefix(&editor_for_enter, ListPrefixKind::Bullet)
            }
            gtk::gdk::Key::o | gtk::gdk::Key::O if shifted => {
                toggle_numbered_list(&editor_for_enter)
            }
            _ => return gtk::glib::Propagation::Proceed,
        }
        gtk::glib::Propagation::Stop
    });
    editor.add_controller(keys);

    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let editor_for_click = editor.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        handle_editor_press(&editor_for_click, x, y);
        if !gesture
            .current_event_state()
            .contains(gtk::gdk::ModifierType::CONTROL_MASK)
        {
            return;
        }
        let (x, y) = editor_for_click.window_to_buffer_coords(
            gtk::TextWindowType::Widget,
            x as i32,
            y as i32,
        );
        let Some(url) = editor_for_click
            .iter_at_location(x, y)
            .and_then(|iter| link_url_at(&iter))
        else {
            return;
        };
        if is_openable_link(&url)
            && let Err(error) =
                adw::gio::AppInfo::launch_default_for_uri(&url, None::<&adw::gio::AppLaunchContext>)
        {
            eprintln!(
                "{}",
                memonotes::tr_format!("link.error.open_failed", error = error)
            );
        }
    });
    editor.add_controller(click);
}

const LINK_TAG_PREFIX: &str = "editor-link:";

fn auto_link_plain_urls(buffer: &gtk::TextBuffer) {
    let (start, end) = buffer.bounds();
    let text = buffer.slice(&start, &end, true).to_string();
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;

    while index < characters.len() {
        while index < characters.len() && characters[index].is_whitespace() {
            index += 1;
        }
        let token_start = index;
        while index < characters.len() && !characters[index].is_whitespace() {
            index += 1;
        }
        let token_end = index;
        if token_start == token_end {
            continue;
        }

        let mut url_end = token_end;
        while url_end > token_start
            && matches!(characters[url_end - 1], '.' | ',' | '!' | '?' | ';' | ':')
        {
            url_end -= 1;
        }
        while url_end > token_start {
            let closing = characters[url_end - 1];
            let (open, close) = match closing {
                ')' => ('(', ')'),
                ']' => ('[', ']'),
                '}' => ('{', '}'),
                _ => break,
            };
            let candidate: String = characters[token_start..url_end].iter().collect();
            if candidate.matches(close).count() <= candidate.matches(open).count() {
                break;
            }
            url_end -= 1;
        }

        let candidate: String = characters[token_start..url_end].iter().collect();
        if !is_auto_link_candidate(&candidate) {
            continue;
        }
        let Some(url) = normalize_link_url(&candidate) else {
            continue;
        };
        let link_start = buffer.iter_at_offset(token_start as i32);
        let link_end = buffer.iter_at_offset(url_end as i32);
        remove_link_tags(buffer, &link_start, &link_end);
        buffer.apply_tag(&link_tag(buffer, &url), &link_start, &link_end);
    }
}

fn is_auto_link_candidate(candidate: &str) -> bool {
    let address = candidate
        .strip_prefix("https://")
        .or_else(|| candidate.strip_prefix("http://"))
        .unwrap_or(candidate);
    let host = address
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    if host.parse::<std::net::IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let labels: Vec<_> = host.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
        && labels.last().is_some_and(|label| {
            label.len() >= 2
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
        })
}

fn link_tag(buffer: &gtk::TextBuffer, url: &str) -> gtk::TextTag {
    let name = format!("{LINK_TAG_PREFIX}{url}");
    editor_text_tag(buffer, &name.clone(), || {
        gtk::TextTag::builder()
            .name(name)
            .foreground("#3584e4")
            .underline(gtk::pango::Underline::Single)
            .build()
    })
}

fn link_url_at(iter: &gtk::TextIter) -> Option<String> {
    iter.tags().iter().find_map(|tag| {
        tag.name()
            .and_then(|name| name.strip_prefix(LINK_TAG_PREFIX).map(str::to_owned))
    })
}

fn remove_link_tags(buffer: &gtk::TextBuffer, start: &gtk::TextIter, end: &gtk::TextIter) {
    let mut tags = Vec::new();
    buffer.tag_table().foreach(|tag| {
        if tag
            .name()
            .is_some_and(|name| name.starts_with(LINK_TAG_PREFIX))
        {
            tags.push(tag.clone());
        }
    });
    for tag in tags {
        buffer.remove_tag(&tag, start, end);
    }
}

/// The URL and bounds of the link touching `cursor`, if any.
fn link_at_cursor(
    buffer: &gtk::TextBuffer,
    cursor: &gtk::TextIter,
) -> Option<(String, gtk::TextIter, gtk::TextIter)> {
    let mut inside = *cursor;
    let url = match link_url_at(&inside) {
        Some(url) => url,
        None => {
            if !inside.backward_char() {
                return None;
            }
            link_url_at(&inside)?
        }
    };
    let tag = link_tag(buffer, &url);
    let mut start = inside;
    if !start.starts_tag(Some(&tag)) {
        start.backward_to_tag_toggle(Some(&tag));
    }
    let mut end = inside;
    end.forward_to_tag_toggle(Some(&tag));
    Some((url, start, end))
}

fn edit_editor_link(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let existing = link_at_cursor(&buffer, &cursor);
    let (initial_url, start, end) = match (buffer.selection_bounds(), existing) {
        (Some((start, end)), existing) => (
            existing.map(|(url, _, _)| url).unwrap_or_default(),
            start.offset(),
            end.offset(),
        ),
        (None, Some((url, start, end))) => (url, start.offset(), end.offset()),
        (None, None) => (String::new(), cursor.offset(), cursor.offset()),
    };
    let editing_link = !initial_url.is_empty();

    let entry = gtk::Entry::builder()
        .placeholder_text("https://")
        .text(&initial_url)
        .width_chars(36)
        .activates_default(true)
        .build();
    entry.connect_map(|entry| {
        entry.grab_focus();
    });
    let dialog = adw::AlertDialog::builder()
        .heading(tr("editor.link.title"))
        .build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", tr("action.cancel"));
    if editing_link {
        dialog.add_response("remove", tr("editor.link.remove"));
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    }
    dialog.add_response("apply", tr("action.save"));
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");

    let editor_for_response = editor.clone();
    dialog.choose(
        Some(editor),
        None::<&adw::gio::Cancellable>,
        move |response| {
            let buffer = editor_for_response.buffer();
            let mut start = buffer.iter_at_offset(start);
            let mut end = buffer.iter_at_offset(end);
            match response.as_str() {
                "remove" => remove_link_tags(&buffer, &start, &end),
                "apply" => {
                    let Some(url) = normalize_link_url(entry.text().as_str()) else {
                        return;
                    };
                    buffer.begin_user_action();
                    if start == end {
                        buffer.insert(&mut end, &url);
                        start = buffer.iter_at_offset(start.offset());
                    }
                    remove_link_tags(&buffer, &start, &end);
                    buffer.apply_tag(&link_tag(&buffer, &url), &start, &end);
                    buffer.end_user_action();
                }
                _ => return,
            }
            buffer.emit_by_name::<()>("changed", &[]);
            editor_for_response.grab_focus();
        },
    );
}

fn editor_list_kind(
    buffer: &gtk::TextBuffer,
    mut start: gtk::TextIter,
    line: &str,
) -> Option<ListPrefixKind> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let indentation = line.chars().count() - trimmed.chars().count();
    start.forward_chars(indentation as i32);
    if trimmed.starts_with("• ") {
        Some(ListPrefixKind::Bullet)
    } else if trimmed.starts_with("☐ ") || trimmed.starts_with("☑ ") {
        Some(ListPrefixKind::Task)
    } else if trimmed.starts_with("\u{fffc} ") {
        Some(if start.has_tag(&bullet_marker_tag(buffer)) {
            ListPrefixKind::Bullet
        } else {
            ListPrefixKind::Task
        })
    } else {
        None
    }
}

fn indent_editor_list(editor: &gtk::TextView, outdent: bool) -> bool {
    let buffer = editor.buffer();
    let selection = buffer.selection_bounds();
    let (first, mut last) = selection
        .as_ref()
        .map(|(start, end)| (start.line(), end.line()))
        .unwrap_or_else(|| {
            let line = buffer.iter_at_mark(&buffer.get_insert()).line();
            (line, line)
        });
    if let Some((_, end)) = selection.as_ref()
        && end.line_offset() == 0
        && last > first
    {
        last -= 1;
    }
    let mut handled = false;
    buffer.begin_user_action();
    for line_number in (first..=last).rev() {
        let Some(mut start) = buffer.iter_at_line(line_number) else {
            continue;
        };
        let mut end = start;
        end.forward_to_line_end();
        let line = buffer.slice(&start, &end, true).to_string();
        if editor_list_kind(&buffer, start, &line).is_none() {
            continue;
        }
        handled = true;
        if outdent {
            let count = if line.starts_with('\t') {
                1
            } else {
                line.chars()
                    .take_while(|character| *character == ' ')
                    .take(4)
                    .count()
            };
            let mut end = start;
            end.forward_chars(count as i32);
            buffer.delete(&mut start, &mut end);
        } else {
            buffer.insert(&mut start, "    ");
        }
    }
    buffer.end_user_action();
    handled
}

fn continue_editor_list_on_enter(editor: &gtk::TextView) -> bool {
    let buffer = editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let Some(mut line_start) = buffer.iter_at_line(cursor.line()) else {
        return false;
    };
    let mut line_end = line_start;
    line_end.forward_to_line_end();
    let line = buffer.slice(&line_start, &line_end, true).to_string();
    let Some(kind) = editor_list_kind(&buffer, line_start, &line) else {
        return continue_plain_list_on_enter(editor, line_start, &line);
    };
    let trimmed = line.trim_start_matches([' ', '\t']);
    let indentation = &line[..line.len() - trimmed.len()];
    let prefix_chars = indentation.chars().count() as i32 + 2;
    let empty = trimmed
        .chars()
        .skip(2)
        .collect::<String>()
        .trim()
        .is_empty();
    let marker = match kind {
        ListPrefixKind::Bullet => "• ",
        ListPrefixKind::Task => "☐ ",
    };

    if empty {
        if !indentation.is_empty() {
            return indent_editor_list(editor, true);
        }
        let mut prefix_end = line_start;
        prefix_end.forward_chars(prefix_chars);
        remove_editor_anchor_widgets(editor, line_start, prefix_end);
        buffer.delete(&mut line_start, &mut prefix_end);
        return true;
    }

    buffer.begin_user_action();
    buffer.insert_at_cursor(&format!("\n{indentation}{marker}"));
    install_task_checkbuttons(editor);
    buffer.end_user_action();
    true
}

/// Replaces the typed `---` line with a rule and moves the caret to a new line below it.
fn insert_editor_rule_from_line(editor: &gtk::TextView, mut line_start: gtk::TextIter) {
    let history = editor_image_history(editor);
    let before = history
        .as_ref()
        .map(|history| begin_editor_image_change(editor, history));

    let buffer = editor.buffer();
    let mut line_end = line_start;
    line_end.forward_to_line_end();
    buffer.delete(&mut line_start, &mut line_end);
    let anchor = buffer.create_child_anchor(&mut line_start);
    editor.add_child_at_anchor(&editor_table::build_rule_widget(editor), &anchor);
    buffer.insert(&mut line_start, "\n");
    buffer.place_cursor(&line_start);

    match (history, before) {
        (Some(history), Some(before)) => {
            finish_editor_image_change(editor, &history, before)
        }
        _ => buffer.emit_by_name::<()>("changed", &[]),
    }
}

/// Handles lists typed as plain text: `- `/`* `/`+ ` become bullets, `1. ` is numbered on.
fn continue_plain_list_on_enter(
    editor: &gtk::TextView,
    mut line_start: gtk::TextIter,
    line: &str,
) -> bool {
    let buffer = editor.buffer();
    // The first line is the note title.
    if line_start.line() == 0 {
        return false;
    }
    let trimmed = line.trim_start_matches([' ', '\t']);
    let indentation = &line[..line.len() - trimmed.len()];
    let cursor_offset = buffer.iter_at_mark(&buffer.get_insert()).line_offset() as usize;

    if memonotes::editor_document::is_thematic_break(line)
        && cursor_offset == line.chars().count()
    {
        insert_editor_rule_from_line(editor, line_start);
        return true;
    }

    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits)
        && let Some(rest) = trimmed[digits..].strip_prefix(". ")
    {
        let marker_len = indentation.len() + digits + 2;
        if cursor_offset < marker_len {
            return false;
        }
        if rest.trim().is_empty() {
            let mut line_end = line_start;
            line_end.forward_to_line_end();
            buffer.delete(&mut line_start, &mut line_end);
        } else if let Ok(number) = trimmed[..digits].parse::<u64>() {
            buffer.insert_at_cursor(&format!("\n{indentation}{}. ", number + 1));
        } else {
            return false;
        }
        return true;
    }

    let is_bullet = ["- ", "* ", "+ "]
        .iter()
        .any(|marker| trimmed.starts_with(marker));
    let is_task = trimmed.get(2..).is_some_and(|rest| {
        rest.starts_with("[ ] ") || rest.starts_with("[x] ") || rest.starts_with("[X] ")
    });
    if !is_bullet || is_task || cursor_offset < indentation.len() + 2 {
        return false;
    }
    buffer.begin_user_action();
    line_start.forward_chars(indentation.len() as i32);
    let mut prefix_end = line_start;
    prefix_end.forward_chars(2);
    buffer.delete(&mut line_start, &mut prefix_end);
    buffer.insert(&mut line_start, "• ");
    buffer.end_user_action();
    install_task_checkbuttons(editor);
    continue_editor_list_on_enter(editor)
}

fn apply_inline_style(buffer: &gtk::TextBuffer, mark: Mark) {
    let Some((start, end)) = buffer.selection_bounds() else {
        return;
    };
    let tag = inline_style_tag(buffer, mark);
    let mut cursor = start;
    let mut all_styled = true;
    while cursor.offset() < end.offset() {
        if !cursor.has_tag(&tag) {
            all_styled = false;
            break;
        }
        cursor.forward_char();
    }
    if all_styled {
        buffer.remove_tag(&tag, &start, &end);
    } else {
        buffer.apply_tag(&tag, &start, &end);
    }
    buffer.emit_by_name::<()>("changed", &[]);
}

fn toggle_list_prefix(editor: &gtk::TextView, kind: ListPrefixKind) {
    let buffer = editor.buffer();
    let selection = buffer.selection_bounds();
    let (start_line, mut end_line) = selection
        .as_ref()
        .map(|(start, end)| (start.line(), end.line()))
        .unwrap_or_else(|| {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (cursor.line(), cursor.line())
        });
    if let Some((_, end)) = selection.as_ref()
        && end.line_offset() == 0
        && end_line > start_line
    {
        end_line -= 1;
    }

    for line_number in (start_line..=end_line).rev() {
        let Some(mut line_start) = buffer.iter_at_line(line_number) else {
            continue;
        };
        let mut line_end = line_start;
        line_end.forward_to_line_end();
        let line = buffer.slice(&line_start, &line_end, true).to_string();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indentation = line.chars().count() - trimmed.chars().count();
        line_start.forward_chars(indentation as i32);
        let line = trimmed;
        let bullet = line_start.has_tag(&bullet_marker_tag(&buffer));
        if line.starts_with("\u{fffc} ") && !bullet && matches!(kind, ListPrefixKind::Task) {
            let checked_tag = task_checkbox_checked_tag(&buffer);
            if let Some(anchor) = line_start.child_anchor()
                && let Some(checkbox) = anchor
                    .widgets()
                    .into_iter()
                    .find_map(|widget| widget.downcast::<gtk::ToggleButton>().ok())
            {
                checkbox.set_active(!line_start.has_tag(&checked_tag));
                continue;
            }
        }
        let (remove_chars, insert_prefix) = if line.starts_with("\u{fffc} ") {
            if bullet {
                list_prefix_change("• ", kind)
            } else {
                (2, "• ")
            }
        } else {
            list_prefix_change(&line, kind)
        };
        let mut prefix_end = line_start;
        prefix_end.forward_chars(remove_chars);
        remove_editor_anchor_widgets(editor, line_start, prefix_end);
        buffer.delete(&mut line_start, &mut prefix_end);
        buffer.insert(&mut line_start, insert_prefix);
    }
    install_task_checkbuttons(editor);
    editor.grab_focus();
}

/// Length in characters of a leading `N. ` marker.
fn numbered_prefix_chars(line: &str) -> Option<i32> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    ((1..=9).contains(&digits) && line[digits..].starts_with(". ")).then_some(digits as i32 + 2)
}

/// Characters of an existing list marker (bullet, checkbox or number) at the start of `line`.
fn list_marker_chars(line: &str) -> i32 {
    if let Some(chars) = numbered_prefix_chars(line) {
        chars
    } else if ["• ", "☐ ", "☑ ", "\u{fffc} "]
        .iter()
        .any(|marker| line.starts_with(marker))
    {
        2
    } else {
        0
    }
}

fn toggle_numbered_list(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let selection = buffer.selection_bounds();
    let (start_line, mut end_line) = selection
        .as_ref()
        .map(|(start, end)| (start.line(), end.line()))
        .unwrap_or_else(|| {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (cursor.line(), cursor.line())
        });
    if let Some((_, end)) = selection.as_ref()
        && end.line_offset() == 0
        && end_line > start_line
    {
        end_line -= 1;
    }

    // The first line is the note title.
    let lines = (start_line.max(1)..=end_line).collect::<Vec<_>>();
    if lines.is_empty() {
        return;
    }
    let line_text = |line_number: i32| {
        let start = buffer.iter_at_line(line_number)?;
        let mut end = start;
        end.forward_to_line_end();
        Some(buffer.slice(&start, &end, true).to_string())
    };
    let all_numbered = lines.iter().all(|line_number| {
        line_text(*line_number).is_some_and(|line| {
            numbered_prefix_chars(line.trim_start_matches([' ', '\t'])).is_some()
        })
    });

    buffer.begin_user_action();
    for (index, line_number) in lines.iter().enumerate().rev() {
        let Some(line) = line_text(*line_number) else {
            continue;
        };
        let Some(mut line_start) = buffer.iter_at_line(*line_number) else {
            continue;
        };
        let trimmed = line.trim_start_matches([' ', '\t']);
        line_start.forward_chars((line.chars().count() - trimmed.chars().count()) as i32);
        let mut prefix_end = line_start;
        prefix_end.forward_chars(list_marker_chars(trimmed));
        remove_editor_anchor_widgets(editor, line_start, prefix_end);
        buffer.delete(&mut line_start, &mut prefix_end);
        if !all_numbered {
            buffer.insert(&mut line_start, &format!("{}. ", index + 1));
        }
    }
    buffer.end_user_action();
    install_task_checkbuttons(editor);
    editor.grab_focus();
}

fn list_prefix_change(line: &str, kind: ListPrefixKind) -> (i32, &'static str) {
    match kind {
        ListPrefixKind::Task if line.starts_with("☐ ") => (2, "☑ "),
        ListPrefixKind::Task if line.starts_with("☑ ") => (2, "☐ "),
        ListPrefixKind::Task if line.starts_with("• ") => (2, "☐ "),
        ListPrefixKind::Task => (0, "☐ "),
        ListPrefixKind::Bullet if line.starts_with("☐ ") || line.starts_with("☑ ") => (2, "• "),
        ListPrefixKind::Bullet if line.starts_with("• ") => (2, ""),
        ListPrefixKind::Bullet => (0, "• "),
    }
}

fn map_list_markers(text: &str, to_editor: bool) -> String {
    let mut mapped = String::with_capacity(text.len());
    let mut in_fenced_code = false;

    for line_with_ending in text.split_inclusive('\n') {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fenced_code = !in_fenced_code;
            mapped.push_str(line_with_ending);
            continue;
        }

        if in_fenced_code {
            mapped.push_str(line_with_ending);
            continue;
        }

        if to_editor && memonotes::editor_document::is_thematic_break(line) {
            mapped.push_str(line_with_ending);
            continue;
        }
        let indentation = line.len() - trimmed.len();
        let (rest, replacement) = if to_editor {
            if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
                (Some(rest), "☐ ")
            } else if let Some(rest) = trimmed
                .strip_prefix("- [x] ")
                .or_else(|| trimmed.strip_prefix("- [X] "))
            {
                (Some(rest), "☑ ")
            } else if let Some(rest) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("+ "))
            {
                (Some(rest), "• ")
            } else {
                (None, "")
            }
        } else if let Some(rest) = trimmed.strip_prefix("☐ ") {
            (Some(rest), "- [ ] ")
        } else if let Some(rest) = trimmed.strip_prefix("☑ ") {
            (Some(rest), "- [x] ")
        } else if let Some(rest) = trimmed.strip_prefix("• ") {
            (Some(rest), "- ")
        } else {
            (None, "")
        };

        if let Some(rest) = rest {
            mapped.push_str(&line[..indentation]);
            mapped.push_str(replacement);
            mapped.push_str(rest);
        } else {
            mapped.push_str(line);
        }
        if line_with_ending.ends_with('\n') {
            mapped.push('\n');
        }
    }
    mapped
}

fn editor_text_from_markdown(markdown: &str) -> String {
    map_list_markers(markdown, true)
}

fn markdown_from_editor_text(text: &str) -> String {
    map_list_markers(text, false)
}

#[cfg(test)]
mod editor_format_tests {
    use super::*;

    fn table_widgets(buffer: &gtk::TextBuffer) -> Vec<gtk::Widget> {
        let mut widgets = Vec::new();
        let mut iter = buffer.start_iter();
        while !iter.is_end() {
            if let Some(anchor) = iter.child_anchor() {
                widgets.extend(
                    anchor
                        .widgets()
                        .into_iter()
                        .filter(editor_table::is_table_widget),
                );
            }
            iter.forward_char();
        }
        widgets
    }

    fn descendants_of_type<T: IsA<gtk::Widget>>(widget: &gtk::Widget) -> Vec<T> {
        let mut found = Vec::new();
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(typed) = current.downcast_ref::<T>() {
                found.push(typed.clone());
            }
            found.extend(descendants_of_type::<T>(&current));
            child = current.next_sibling();
        }
        found
    }

    #[test]
    fn pipe_tables_round_trip_through_cell_widgets() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::pipe_tables_round_trip_through_cell_widgets",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        let source = "# Title\n\nBefore\n\n| A | B |\n| :--- | ---: |\n| 1 | 2 \\| 3 |\n\nAfter";
        set_editor_view_content(&editor, source);
        let buffer = editor.buffer();
        assert_eq!(table_widgets(&buffer).len(), 1);
        assert_eq!(markdown_from_editor_buffer(&buffer), source);

        let table = table_widgets(&buffer).remove(0);
        let entries = descendants_of_type::<gtk::Entry>(&table);
        assert_eq!(entries.len(), 4);
        entries[3].set_text("x | y");
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            source.replace("2 \\| 3", "x \\| y")
        );

        let history = editor_image_history(&editor).unwrap();
        let add_row = descendants_of_type::<gtk::Button>(&table)
            .into_iter()
            .find(|button| button.label().as_deref() == Some(tr("editor.table.add_row")))
            .unwrap();
        add_row.emit_clicked();
        assert!(markdown_from_editor_buffer(&buffer).contains("| 1 | x \\| y |\n|  |  |\n"));
        assert!(undo_editor_image_change(&editor, &history));
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            source.replace("2 \\| 3", "x \\| y")
        );
        assert_eq!(table_widgets(&buffer).len(), 1);

        buffer.place_cursor(&buffer.end_iter());
        editor_table::insert_table(&editor);
        assert_eq!(table_widgets(&buffer).len(), 2);
        let markdown = markdown_from_editor_buffer(&buffer);
        assert!(markdown.ends_with(&format!(
            "After\n\n| {} | {} |\n| --- | --- |\n|  |  |\n|  |  |\n",
            memonotes::tr_format!("editor.table.column_title", n = 1),
            memonotes::tr_format!("editor.table.column_title", n = 2)
        )));
        Ok(())
    }

    #[test]
    fn horizontal_rules_round_trip_through_separator_widgets() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::horizontal_rules_round_trip_through_separator_widgets",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        let source = "# Title\n\nBefore\n\n---\n\nAfter\n\n---";
        set_editor_view_content(&editor, source);
        let buffer = editor.buffer();
        let rules = |buffer: &gtk::TextBuffer| {
            let mut count = 0;
            let mut iter = buffer.start_iter();
            while !iter.is_end() {
                if let Some(anchor) = iter.child_anchor() {
                    count += anchor
                        .widgets()
                        .iter()
                        .filter(|widget| editor_table::is_rule_widget(widget))
                        .count();
                }
                iter.forward_char();
            }
            count
        };
        assert_eq!(rules(&buffer), 2);
        assert_eq!(markdown_from_editor_buffer(&buffer), source);

        set_editor_view_content(&editor, "Title\n\n* * *\n\n- item");
        assert_eq!(rules(&buffer), 1);
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n\n---\n\n- item"
        );

        set_editor_view_content(&editor, "Title\nBefore\n---");
        buffer.place_cursor(&buffer.end_iter());
        assert!(continue_editor_list_on_enter(&editor));
        assert_eq!(rules(&buffer), 1);
        assert_eq!(buffer.iter_at_mark(&buffer.get_insert()).line(), 3);
        assert!(markdown_from_editor_buffer(&buffer).starts_with("# Title\nBefore\n\n---"));
        Ok(())
    }

    #[test]
    fn bullet_markers_cannot_be_selected_or_copied() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::bullet_markers_cannot_be_selected_or_copied",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        set_editor_view_content(&editor, "Title\n- First\n- Second");
        let buffer = editor.buffer();
        let line_start = buffer.iter_at_line(1).unwrap();
        buffer.select_range(&line_start, &buffer.end_iter());
        let (start, _) = buffer.selection_bounds().unwrap();
        assert_eq!(start.offset(), line_start.offset() + 2);

        let mut inside = line_start;
        inside.forward_char();
        let mut end = line_start;
        end.forward_to_line_end();
        buffer.select_range(&end, &inside);
        let (start, _) = buffer.selection_bounds().unwrap();
        assert_eq!(start.offset(), line_start.offset() + 2);

        buffer.select_range(&line_start, &buffer.end_iter());
        editor.emit_by_name::<()>("copy-clipboard", &[]);
        let copied = gtk::glib::MainContext::default()
            .block_on(editor.clipboard().read_text_future())?
            .unwrap();
        assert_eq!(copied, "First\nSecond");
        Ok(())
    }

    #[test]
    fn list_tab_shortcuts_preserve_nesting_and_enter_level() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::list_tab_shortcuts_preserve_nesting_and_enter_level",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        let controllers = editor.observe_controllers();
        let existing: Vec<_> = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .collect();
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        let keys = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .filter(|controller| !existing.contains(controller))
            .find_map(|controller| controller.downcast::<gtk::EventControllerKey>().ok())
            .unwrap();
        let existing: Vec<_> = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .collect();
        connect_tab_navigation(
            &editor,
            gtk::Button::new().upcast(),
            gtk::Button::new().upcast(),
        );
        let navigation = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .find(|controller| !existing.contains(controller))
            .unwrap()
            .downcast::<gtk::EventControllerKey>()
            .unwrap();
        let buffer = editor.buffer();
        set_editor_view_content(&editor, "Title\n- Parent\n- Child\n- [x] Task\nParagraph");
        buffer.place_cursor(&buffer.iter_at_line(2).unwrap());
        let press = |key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType| {
            if keys.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers]) {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        };
        assert!(!navigation.emit_by_name::<bool>(
            "key-pressed",
            &[&gtk::gdk::Key::Tab, &0u32, &gtk::gdk::ModifierType::empty()]
        ));
        assert_eq!(
            press(gtk::gdk::Key::Tab, gtk::gdk::ModifierType::empty()),
            gtk::glib::Propagation::Stop
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- Parent\n    - Child\n- [x] Task\nParagraph"
        );
        buffer.undo();
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- Parent\n- Child\n- [x] Task\nParagraph"
        );
        buffer.redo();
        assert_eq!(
            press(
                gtk::gdk::Key::ISO_Left_Tab,
                gtk::gdk::ModifierType::SHIFT_MASK
            ),
            gtk::glib::Propagation::Stop
        );
        buffer.select_range(
            &buffer.iter_at_line(2).unwrap(),
            &buffer.iter_at_line(4).unwrap(),
        );
        assert!(indent_editor_list(&editor, false));
        let nested = "# Title\n- Parent\n    - Child\n    - [x] Task\nParagraph";
        assert_eq!(markdown_from_editor_buffer(&buffer), nested);
        set_editor_view_content(&editor, "Other\nBody");
        set_editor_view_content(&editor, nested);
        assert_eq!(markdown_from_editor_buffer(&buffer), nested);
        let mut end = buffer.iter_at_line(2).unwrap();
        end.forward_to_line_end();
        buffer.place_cursor(&end);
        assert_eq!(
            press(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty()),
            gtk::glib::Propagation::Stop
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- Parent\n    - Child\n    - \n    - [x] Task\nParagraph"
        );
        assert!(continue_editor_list_on_enter(&editor));
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- Parent\n    - Child\n- \n    - [x] Task\nParagraph"
        );
        assert!(continue_editor_list_on_enter(&editor));
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- Parent\n    - Child\n\n    - [x] Task\nParagraph"
        );
        buffer.place_cursor(&buffer.end_iter());
        assert_eq!(
            press(gtk::gdk::Key::Tab, gtk::gdk::ModifierType::empty()),
            gtk::glib::Propagation::Proceed
        );
        Ok(())
    }

    #[test]
    fn markdown_headings_render_as_styled_blocks_and_round_trip() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::markdown_headings_render_as_styled_blocks_and_round_trip",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        set_editor_view_content(
            &editor,
            "Title\n# Main section\n## Section\n\n### Subsection\n```md\n#### literal\n```",
        );
        let buffer = editor.buffer();
        let rendered_text = buffer
            .slice(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string();
        assert_eq!(
            rendered_text,
            "Title\nMain section\nSection\n\nSubsection\n```md\n#### literal\n```"
        );

        let main_heading_start = buffer.iter_at_line(1).unwrap();
        assert!(main_heading_start.has_tag(&heading_style_tag(&buffer, 1)));
        let section_start = buffer.iter_at_line(2).unwrap();
        assert!(section_start.has_tag(&heading_style_tag(&buffer, 2)));
        let subsection_start = buffer.iter_at_line(4).unwrap();
        assert!(subsection_start.has_tag(&heading_style_tag(&buffer, 3)));
        let code_heading_start = buffer.iter_at_line(6).unwrap();
        assert!(!code_heading_start.has_tag(&heading_style_tag(&buffer, 4)));
        let mut main_heading_end = main_heading_start;
        main_heading_end.forward_to_line_end();
        buffer.insert(&mut main_heading_end, " updated");
        style_note_heading(&buffer);
        assert!(
            buffer
                .iter_at_line(1)
                .unwrap()
                .has_tag(&heading_style_tag(&buffer, 1))
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n# Main section updated\n## Section\n\n### Subsection\n```md\n#### literal\n```"
        );
        Ok(())
    }

    #[test]
    fn links_render_as_tagged_text_and_round_trip() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::links_render_as_tagged_text_and_round_trip",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        let source = "Title\nSee [**docs**](https://example.test/a) now";
        set_editor_view_content(&editor, source);
        let buffer = editor.buffer();
        let text = buffer
            .slice(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string();
        assert_eq!(text, "Title\nSee docs now");
        let mut inside = buffer.iter_at_line(1).unwrap();
        inside.forward_chars(5);
        assert_eq!(
            link_url_at(&inside).as_deref(),
            Some("https://example.test/a")
        );
        let (_, link_start, link_end) = link_at_cursor(&buffer, &inside).unwrap();
        assert_eq!(buffer.slice(&link_start, &link_end, true), "docs");
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            format!("# Title\n{}", &source[6..])
        );

        let mut start = buffer.iter_at_line(1).unwrap();
        let mut end = start;
        end.forward_chars(3);
        buffer.apply_tag(&link_tag(&buffer, "https://x.test"), &start, &end);
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n[See](https://x.test) [**docs**](https://example.test/a) now"
        );
        end.forward_to_line_end();
        start = buffer.iter_at_line(1).unwrap();
        remove_link_tags(&buffer, &start, &end);
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nSee **docs** now"
        );
        Ok(())
    }

    #[test]
    fn images_render_inline_and_round_trip_through_markdown() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let image_data = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::images_render_inline_and_round_trip_through_markdown",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .env("XDG_DATA_HOME", image_data.path())
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let directory = tempfile::tempdir()?;
        let image_path = directory.path().join("diagram.svg");
        std::fs::write(
            &image_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#,
        )?;
        let editor = gtk::TextView::new();
        let image_history = new_editor_image_history();
        let _toolbar = editor_format_toolbar(&editor, Rc::clone(&image_history));
        install_editor_format_shortcuts(&editor, Rc::clone(&image_history));
        let editor_controllers = editor.observe_controllers();
        assert!(
            (0..editor_controllers.n_items())
                .filter_map(|index| editor_controllers.item(index))
                .any(|controller| controller.is::<gtk::DropTarget>())
        );
        set_editor_view_content(&editor, "# Title\n");
        let saved_markdown = Rc::new(RefCell::new(String::new()));
        let saved_markdown_for_change = Rc::clone(&saved_markdown);
        let save_count = Rc::new(Cell::new(0));
        let save_count_for_change = Rc::clone(&save_count);
        let editor_for_change = editor.downgrade();
        editor.buffer().connect_changed(move |buffer| {
            if editor_for_change
                .upgrade()
                .is_some_and(|editor| editor.has_css_class("editor-image-moving"))
            {
                return;
            }
            save_count_for_change.set(save_count_for_change.get() + 1);
            *saved_markdown_for_change.borrow_mut() = markdown_from_editor_buffer(buffer);
        });
        insert_editor_image_file(&editor, &image_path, &image_history)?;
        let buffer = editor.buffer();
        let source = markdown_from_editor_buffer(&buffer);
        assert!(source.contains("(memonotes-image://"));
        assert_eq!(std::fs::read_dir(editor_image_directory()?)?.count(), 1);
        assert_eq!(*saved_markdown.borrow(), source);
        assert_eq!(markdown_from_editor_buffer(&buffer), source);

        let key_controller = (0..editor_controllers.n_items())
            .filter_map(|index| editor_controllers.item(index))
            .find_map(|controller| controller.downcast::<gtk::EventControllerKey>().ok())
            .unwrap();
        assert!(key_controller.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::z,
                &0u32,
                &gtk::gdk::ModifierType::CONTROL_MASK,
            ]
        ));
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title");
        assert!(key_controller.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::z,
                &0u32,
                &(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK),
            ]
        ));
        assert_eq!(markdown_from_editor_buffer(&buffer), source);
        save_count.set(0);

        let image_anchor = buffer.iter_at_line(1).unwrap().child_anchor().unwrap();
        let image_widget = image_anchor.widgets().into_iter().next().unwrap();
        let controllers = image_widget.observe_controllers();
        assert!(
            (0..controllers.n_items())
                .filter_map(|index| controllers.item(index))
                .any(|controller| controller.is::<gtk::DragSource>())
        );
        assert!(
            (0..controllers.n_items())
                .filter_map(|index| controllers.item(index))
                .any(|controller| controller.is::<gtk::GestureClick>())
        );
        let drag_source = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .find_map(|controller| controller.downcast::<gtk::DragSource>().ok())
            .unwrap();
        assert_eq!(drag_source.actions(), gtk::gdk::DragAction::COPY);
        select_editor_image(&editor, &image_widget, &image_anchor);
        assert!(image_widget.has_css_class("editor-image-selected"));
        assert!(buffer.selection_bounds().is_none());
        assert_eq!(
            buffer.iter_at_mark(&buffer.get_insert()).offset(),
            buffer.iter_at_child_anchor(&image_anchor).offset()
        );

        let mut text_end = buffer.end_iter();
        buffer.insert(&mut text_end, "left right");
        let before_move = markdown_from_editor_buffer(&buffer);
        let image_markdown = editor_image_markdown(&image_widget).unwrap();
        let target_offset = buffer.iter_at_line(1).unwrap().offset() + 1 + "left".len() as i32;
        save_count.set(0);
        assert!(move_editor_image_to_offset(
            &editor,
            &image_widget.widget_name(),
            target_offset,
            &image_history,
        ));
        let moved = format!("# Title\nleft{image_markdown} right");
        assert_eq!(markdown_from_editor_buffer(&buffer), moved);
        assert_eq!(*saved_markdown.borrow(), moved);
        assert_eq!(save_count.get(), 1);
        let mut moved_image_position = buffer.iter_at_line(1).unwrap();
        moved_image_position.forward_chars(4);
        let moved_anchor = moved_image_position.child_anchor().unwrap();
        let moved_widget = moved_anchor.widgets().into_iter().next().unwrap();
        assert_eq!(moved_widget, image_widget);
        assert!(moved_widget.has_css_class("editor-image-selected"));

        assert_eq!(image_history.borrow().undo.len(), 2);
        assert!(key_controller.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::z,
                &0u32,
                &gtk::gdk::ModifierType::CONTROL_MASK,
            ]
        ));
        assert_eq!(markdown_from_editor_buffer(&buffer), before_move);
        assert!(key_controller.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::z,
                &0u32,
                &(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK),
            ]
        ));
        assert_eq!(markdown_from_editor_buffer(&buffer), moved);

        let mut moved_image_position = buffer.iter_at_line(1).unwrap();
        moved_image_position.forward_chars(4);
        let moved_anchor = moved_image_position.child_anchor().unwrap();
        let moved_widget = moved_anchor.widgets().into_iter().next().unwrap();
        assert!(moved_widget.is::<gtk::Picture>());
        let drag_value =
            gtk::glib::BoxedAnyObject::new(moved_widget.widget_name().to_string()).to_value();
        let drop_x = 10_000.0_f64;
        let drop_y = 10_000.0_f64;
        save_count.set(0);
        assert!(handle_editor_image_drop(
            &editor,
            &drag_value,
            drop_x,
            drop_y,
            &image_history,
        ));
        while gtk::glib::MainContext::default().pending() {
            gtk::glib::MainContext::default().iteration(false);
        }
        let dropped = format!("# Title\nleft right{image_markdown}");
        assert_eq!(markdown_from_editor_buffer(&buffer), dropped);
        assert_eq!(*saved_markdown.borrow(), dropped);
        assert_eq!(save_count.get(), 1);

        let press = |key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType| {
            assert!(key_controller.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers]));
        };
        let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
        buffer.insert_at_cursor("!");
        let typed = markdown_from_editor_buffer(&buffer);
        assert_ne!(typed, dropped);
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), dropped);
        press(gtk::gdk::Key::y, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), typed);
        press(gtk::gdk::Key::z, ctrl);
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), moved);
        press(gtk::gdk::Key::y, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), dropped);
        press(gtk::gdk::Key::z, ctrl);
        press(gtk::gdk::Key::z, ctrl);
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title");
        press(gtk::gdk::Key::y, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), source);
        press(gtk::gdk::Key::z, ctrl);
        set_editor_view_content(&editor, "Other\nNote");
        assert!(image_history.borrow().undo.is_empty());
        assert!(image_history.borrow().redo.is_empty());

        let reloaded = gtk::TextView::new();
        install_editor_format_shortcuts(&reloaded, new_editor_image_history());
        set_editor_view_content(&reloaded, &dropped);
        let reloaded_buffer = reloaded.buffer();
        let mut image_position = reloaded_buffer.iter_at_line(1).unwrap();
        image_position.forward_to_line_end();
        image_position.backward_char();
        let image_anchor = image_position.child_anchor();
        let image_widget = image_anchor.unwrap().widgets().into_iter().next().unwrap();
        assert!(image_widget.is::<gtk::Picture>());
        assert_eq!(markdown_from_editor_buffer(&reloaded_buffer), dropped);
        Ok(())
    }

    #[test]
    fn backspace_next_to_an_image_removes_it_and_can_be_undone() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let image_data = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::backspace_next_to_an_image_removes_it_and_can_be_undone",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .env("XDG_DATA_HOME", image_data.path())
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let directory = tempfile::tempdir()?;
        let image_path = directory.path().join("diagram.svg");
        std::fs::write(
            &image_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#,
        )?;
        let editor = gtk::TextView::new();
        let image_history = new_editor_image_history();
        let _toolbar = editor_format_toolbar(&editor, Rc::clone(&image_history));
        install_editor_format_shortcuts(&editor, Rc::clone(&image_history));
        set_editor_view_content(&editor, "# Title\n");
        insert_editor_image_file(&editor, &image_path, &image_history)?;
        let buffer = editor.buffer();
        let with_image = markdown_from_editor_buffer(&buffer);
        assert!(with_image.contains("(memonotes-image://"));

        let mut after_image = buffer.iter_at_line(1).unwrap();
        after_image.forward_char();
        buffer.place_cursor(&after_image);
        editor.emit_by_name::<()>("backspace", &[]);
        while gtk::glib::MainContext::default().pending() {
            gtk::glib::MainContext::default().iteration(false);
        }
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title");

        undo_editor_change(&editor, &image_history);
        assert_eq!(markdown_from_editor_buffer(&buffer), with_image);
        Ok(())
    }

    #[test]
    fn bare_anchor_characters_never_enter_or_leave_the_editor() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::bare_anchor_characters_never_enter_or_leave_the_editor",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        set_editor_view_content(&editor, "Title\nzonder\u{fffc}afbeelding");
        let buffer = editor.buffer();
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nzonderafbeelding"
        );

        let mut end = buffer.end_iter();
        buffer.insert(&mut end, " en\u{fffc} meer");
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nzonderafbeelding en meer"
        );
        Ok(())
    }

    #[test]
    fn image_drop_snaps_after_line_text_and_shows_a_cursor_indicator() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::image_drop_snaps_after_line_text_and_shows_a_cursor_indicator",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        set_editor_view_content(&editor, "Title\nfirst line here\nsecond line");
        let window = gtk::Window::builder()
            .default_width(480)
            .default_height(240)
            .child(&editor)
            .build();
        window.present();
        while gtk::glib::MainContext::default().pending() {
            gtk::glib::MainContext::default().iteration(false);
        }
        let buffer = editor.buffer();
        let widget_point = |offset: i32, dx: i32| {
            let rect = editor.iter_location(&buffer.iter_at_offset(offset));
            let (x, y) = editor.buffer_to_window_coords(
                gtk::TextWindowType::Widget,
                rect.x() + dx,
                rect.y() + rect.height() / 2,
            );
            (f64::from(x), f64::from(y))
        };
        let first_line = buffer.iter_at_line(1).unwrap();
        let mut first_end = first_line;
        first_end.forward_to_line_end();

        let (x, y) = widget_point(first_end.offset(), 150);
        assert_eq!(editor_drop_iter(&editor, x, y).offset(), first_end.offset());
        let (x, y) = widget_point(first_line.offset() + 5, 1);
        assert_eq!(
            editor_drop_iter(&editor, x, y).offset(),
            first_line.offset() + 5
        );
        let (x, y) = widget_point(0, 1);
        assert_eq!(
            editor_drop_iter(&editor, x, y).offset(),
            first_line.offset()
        );
        assert_eq!(
            editor_drop_iter(&editor, 100.0, 5_000.0).offset(),
            buffer.end_iter().offset()
        );

        let indicator: DropIndicator = Rc::new(RefCell::new(None));
        let (x, y) = widget_point(first_end.offset(), 150);
        show_editor_image_drop_indicator(&editor, &indicator, x, y);
        let bar = indicator.borrow().clone().unwrap();
        assert_eq!(
            bar.ancestor(gtk::TextView::static_type()).as_ref(),
            Some(editor.upcast_ref::<gtk::Widget>())
        );
        assert!(!bar.can_target());
        hide_editor_image_drop_indicator(&indicator);
        assert!(!bar.is_visible());
        assert!(indicator.borrow().is_some());
        window.set_child(gtk::Widget::NONE);
        window.destroy();
        Ok(())
    }

    #[test]
    fn image_corner_handles_resize_with_aspect_ratio_and_undo() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let image_data = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::image_corner_handles_resize_with_aspect_ratio_and_undo",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .env("XDG_DATA_HOME", image_data.path())
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let directory = tempfile::tempdir()?;
        let image_path = directory.path().join("wide.svg");
        std::fs::write(
            &image_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="blue"/></svg>"#,
        )?;
        let editor = gtk::TextView::new();
        editor.set_left_margin(44);
        editor.set_top_margin(12);
        let history = new_editor_image_history();
        let _toolbar = editor_format_toolbar(&editor, Rc::clone(&history));
        install_editor_format_shortcuts(&editor, Rc::clone(&history));
        set_editor_view_content(&editor, "# Title\n");
        let window = gtk::Window::builder()
            .default_width(480)
            .default_height(300)
            .child(&editor)
            .build();
        window.present();
        let pump = || {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
        };
        pump();
        insert_editor_image_file(&editor, &image_path, &history)?;
        pump();
        let buffer = editor.buffer();
        let widget_at_line = |editor: &gtk::TextView| {
            let anchor = editor
                .buffer()
                .iter_at_line(1)
                .unwrap()
                .child_anchor()
                .unwrap();
            let widget = anchor.widgets().into_iter().next().unwrap();
            (anchor, widget)
        };
        let (anchor, widget) = widget_at_line(&editor);
        let picture = widget.clone().downcast::<gtk::Picture>().unwrap();
        assert_eq!(picture.size_request(), (40, 20));

        select_editor_image(&editor, &widget, &anchor);
        pump();
        let handles = history.borrow().handles.clone();
        assert_eq!(handles.len(), 4);
        assert!(handles.iter().all(|handle| handle.is_visible()));
        // The handles must straddle the picture's corners, margins included.
        let anchor_rect = editor.iter_location(&buffer.iter_at_child_anchor(&anchor));
        let positions = history.borrow().handle_positions.clone();
        let half = IMAGE_HANDLE_SIZE / 2;
        assert_eq!(positions[0].0 + half, anchor_rect.x());
        assert_eq!(positions[1].0 + half, anchor_rect.x() + 40);
        assert_eq!(positions[3].1 - positions[0].1, 20);
        assert!(!editor.is_cursor_visible());
        let bounds = picture.compute_bounds(&editor).unwrap();
        let center = (
            f64::from(bounds.x() + bounds.width() / 2.0),
            f64::from(bounds.y() + bounds.height() / 2.0),
        );
        assert!(editor_image_at(&editor, center.0, center.1).is_some());
        assert!(editor_image_at(&editor, 400.0, 250.0).is_none());
        // Pressing selects, repeated presses keep the selection, pressing elsewhere deselects.
        handle_editor_press(&editor, 400.0, 250.0);
        assert!(!picture.has_css_class("editor-image-selected"));
        assert!(handles.iter().all(|handle| !handle.is_visible()));
        for _ in 0..3 {
            handle_editor_press(&editor, center.0, center.1);
            pump();
            assert!(picture.has_css_class("editor-image-selected"));
            assert!(history.borrow().resize_target.is_some());
            assert!(handles.iter().all(|handle| handle.is_visible()));
        }
        let drag = |index: usize, offset_x: f64| {
            let gesture = history.borrow().resize_gesture.clone().unwrap();
            let (left, top) = history.borrow().handle_positions[index];
            let half = IMAGE_HANDLE_SIZE / 2;
            let (x, y) = editor.buffer_to_window_coords(
                gtk::TextWindowType::Widget,
                left + half,
                top + half,
            );
            gesture.emit_by_name::<()>("drag-begin", &[&f64::from(x), &f64::from(y)]);
            // Repeated updates must be idempotent; moving handles may not feed back into the size.
            gesture.emit_by_name::<()>("drag-update", &[&offset_x, &0.0_f64]);
            gesture.emit_by_name::<()>("drag-update", &[&offset_x, &0.0_f64]);
            gesture.emit_by_name::<()>("drag-end", &[&offset_x, &0.0_f64]);
            pump();
        };

        let before = markdown_from_editor_buffer(&buffer);
        drag(3, 60.0);
        assert_eq!(picture.size_request(), (100, 50));
        let resized = markdown_from_editor_buffer(&buffer);
        assert!(resized.contains("#w=100)"), "{resized}");

        let key_controller = (0..editor.observe_controllers().n_items())
            .filter_map(|index| editor.observe_controllers().item(index))
            .find_map(|controller| controller.downcast::<gtk::EventControllerKey>().ok())
            .unwrap();
        let press = |key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType| {
            assert!(key_controller.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers]));
        };
        let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), before);
        press(gtk::gdk::Key::y, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), resized);
        pump();

        let (anchor, widget) = widget_at_line(&editor);
        let picture = widget.clone().downcast::<gtk::Picture>().unwrap();
        assert_eq!(picture.size_request(), (100, 50));
        select_editor_image(&editor, &widget, &anchor);
        pump();
        drag(0, -40.0);
        assert_eq!(picture.size_request(), (140, 70));
        drag(3, -10_000.0);
        assert_eq!(
            picture.size_request(),
            (IMAGE_MIN_WIDTH, IMAGE_MIN_WIDTH / 2)
        );
        drag(3, 100_000.0);
        assert_eq!(
            picture.size_request(),
            (IMAGE_MAX_WIDTH, IMAGE_MAX_WIDTH / 2)
        );

        let reloaded = gtk::TextView::new();
        set_editor_view_content(&reloaded, &markdown_from_editor_buffer(&buffer));
        let reloaded_picture = reloaded
            .buffer()
            .iter_at_line(1)
            .unwrap()
            .child_anchor()
            .unwrap()
            .widgets()
            .into_iter()
            .next()
            .unwrap()
            .downcast::<gtk::Picture>()
            .unwrap();
        assert_eq!(reloaded_picture.size_request(), picture.size_request());

        // Deleting an image, by key or by plain buffer edit, must be undoable as a real image.
        let (anchor, widget) = widget_at_line(&editor);
        select_editor_image(&editor, &widget, &anchor);
        let with_image = markdown_from_editor_buffer(&buffer);
        press(gtk::gdk::Key::Delete, gtk::gdk::ModifierType::empty());
        assert!(!markdown_from_editor_buffer(&buffer).contains("!["));
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), with_image);
        assert!(widget_at_line(&editor).1.is::<gtk::Picture>());
        let mut start = buffer.iter_at_line(1).unwrap();
        let mut end = start;
        end.forward_char();
        buffer.delete(&mut start, &mut end);
        assert!(!markdown_from_editor_buffer(&buffer).contains("!["));
        press(gtk::gdk::Key::z, ctrl);
        assert_eq!(markdown_from_editor_buffer(&buffer), with_image);
        assert!(widget_at_line(&editor).1.is::<gtk::Picture>());
        press(gtk::gdk::Key::y, ctrl);
        assert!(!markdown_from_editor_buffer(&buffer).contains("!["));
        press(gtk::gdk::Key::z, ctrl);

        // One click selects and hides the caret; a key press or an edit deselects again.
        let (_, widget) = widget_at_line(&editor);
        let click = (0..widget.observe_controllers().n_items())
            .filter_map(|index| widget.observe_controllers().item(index))
            .find_map(|controller| controller.downcast::<gtk::GestureClick>().ok())
            .unwrap();
        click.emit_by_name::<()>("pressed", &[&1_i32, &5.0_f64, &5.0_f64]);
        assert!(widget.has_css_class("editor-image-selected"));
        assert!(history.borrow().resize_target.is_some());
        assert!(!editor.is_cursor_visible());
        let handled = key_controller.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::Left,
                &0u32,
                &gtk::gdk::ModifierType::empty(),
            ],
        );
        assert!(!handled);
        assert!(!widget.has_css_class("editor-image-selected"));
        // A caret on the image's own line would be as tall as the image.
        assert!(!editor.is_cursor_visible());
        buffer.place_cursor(&buffer.iter_at_line(0).unwrap());
        assert!(editor.is_cursor_visible());
        click.emit_by_name::<()>("pressed", &[&1_i32, &5.0_f64, &5.0_f64]);
        assert!(widget.has_css_class("editor-image-selected"));
        buffer.insert_at_cursor("x");
        assert!(!widget.has_css_class("editor-image-selected"));
        assert!(!editor.is_cursor_visible());
        assert!(handles.iter().all(|handle| !handle.is_visible()));

        set_editor_view_content(&editor, "Other\nNote");
        assert!(handles.iter().all(|handle| !handle.is_visible()));
        window.set_child(gtk::Widget::NONE);
        window.destroy();
        Ok(())
    }

    #[test]
    fn images_stay_clickable_once_the_overlay_container_exists() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let image_data = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::images_stay_clickable_once_the_overlay_container_exists",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .env("XDG_DATA_HOME", image_data.path())
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let directory = tempfile::tempdir()?;
        let image_path = directory.path().join("wide.svg");
        std::fs::write(
            &image_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="200"><rect width="400" height="200" fill="blue"/></svg>"#,
        )?;
        let editor = gtk::TextView::new();
        editor.set_left_margin(44);
        let history = new_editor_image_history();
        let _toolbar = editor_format_toolbar(&editor, Rc::clone(&history));
        install_editor_format_shortcuts(&editor, Rc::clone(&history));
        set_editor_view_content(&editor, "# Title\n");
        let scroll = gtk::ScrolledWindow::builder().child(&editor).build();
        let window = gtk::Window::builder()
            .default_width(480)
            .default_height(700)
            .child(&scroll)
            .build();
        window.present();
        let pump = || {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while std::time::Instant::now() < end {
                while gtk::glib::MainContext::default().pending() {
                    gtk::glib::MainContext::default().iteration(false);
                }
            }
        };
        pump();
        let buffer = editor.buffer();
        let assert_every_image_is_hit = |editor: &gtk::TextView| {
            let mut iter = buffer.start_iter();
            let mut images = 0;
            while iter.offset() < buffer.char_count() {
                if let Some(anchor) = iter.child_anchor() {
                    let widget = anchor.widgets().into_iter().next().unwrap();
                    let bounds = widget.compute_bounds(editor).unwrap();
                    for (fx, fy) in [(0.05, 0.05), (0.5, 0.5), (0.95, 0.95), (0.05, 0.95)] {
                        let x = f64::from(bounds.x() + bounds.width() * fx);
                        let y = f64::from(bounds.y() + bounds.height() * fy);
                        assert_eq!(editor_image_at(editor, x, y).as_ref(), Some(&widget));
                    }
                    images += 1;
                }
                iter.forward_char();
            }
            images
        };
        insert_editor_image_file(&editor, &image_path, &history)?;
        pump();
        let anchor = buffer.iter_at_line(1).unwrap().child_anchor().unwrap();
        let widget = anchor.widgets().into_iter().next().unwrap();
        // Selecting creates the overlay container, which must not cover images added earlier.
        select_editor_image(&editor, &widget, &anchor);
        pump();
        assert_eq!(assert_every_image_is_hit(&editor), 1);
        deselect_editor_image(&editor);
        buffer.place_cursor(&buffer.end_iter());
        buffer.insert_at_cursor("\ntext\n");
        insert_editor_image_file(&editor, &image_path, &history)?;
        pump();
        assert_eq!(assert_every_image_is_hit(&editor), 2);
        let markdown = markdown_from_editor_buffer(&buffer);
        set_editor_view_content(&editor, "Other\nNote");
        set_editor_view_content(&editor, &markdown);
        pump();
        assert_eq!(assert_every_image_is_hit(&editor), 2);
        window.set_child(gtk::Widget::NONE);
        window.destroy();
        Ok(())
    }

    #[test]
    fn typing_a_url_and_delimiter_automatically_creates_a_link() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::typing_a_url_and_delimiter_automatically_creates_a_link",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        let buffer = editor.buffer();
        buffer.set_text("Title\n");
        install_editor_format_shortcuts(&editor, new_editor_image_history());
        buffer.place_cursor(&buffer.end_iter());
        for character in "www.example.test, ".chars() {
            buffer.insert_at_cursor(&character.to_string());
        }

        let url_start = buffer.iter_at_line(1).unwrap();
        let mut url_end = url_start;
        url_end.forward_chars("www.example.test".chars().count() as i32);
        assert_eq!(
            link_url_at(&url_start).as_deref(),
            Some("https://www.example.test")
        );
        assert_eq!(buffer.slice(&url_start, &url_end, true), "www.example.test");
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n[www.example.test](https://www.example.test), "
        );

        buffer.set_text("Title\n");
        buffer.place_cursor(&buffer.end_iter());
        for character in "https://example.com".chars() {
            buffer.insert_at_cursor(&character.to_string());
        }
        let complete_url = buffer.iter_at_line(1).unwrap();
        assert_eq!(
            link_url_at(&complete_url).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n[https://example.com](https://example.com)"
        );

        buffer.set_text("Title\n");
        buffer.place_cursor(&buffer.end_iter());
        for character in "https://nos.nl ".chars() {
            buffer.insert_at_cursor(&character.to_string());
        }
        let suffix_start = buffer.iter_at_offset("Title\nhttps://nos".chars().count() as i32);
        let suffix_start_offset = suffix_start.offset();
        let mut delete_start = suffix_start;
        let mut suffix_end = buffer.iter_at_offset("Title\nhttps://nos.nl".chars().count() as i32);
        buffer.delete(&mut delete_start, &mut suffix_end);
        buffer.place_cursor(&buffer.iter_at_offset(suffix_start_offset));
        for character in ".com".chars() {
            buffer.insert_at_cursor(&character.to_string());
        }
        let edited_url_start = buffer.iter_at_line(1).unwrap();
        assert_eq!(
            link_url_at(&edited_url_start).as_deref(),
            Some("https://nos.com")
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n[https://nos.com](https://nos.com) "
        );

        buffer.set_text("Title\nhttps://example.test");
        assert_eq!(
            link_url_at(&buffer.iter_at_line(1).unwrap()).as_deref(),
            Some("https://example.test")
        );
        buffer.place_cursor(&buffer.end_iter());
        for character in " example.net ".chars() {
            buffer.insert_at_cursor(&character.to_string());
        }
        let bare_domain =
            buffer.iter_at_offset("Title\nhttps://example.test ".chars().count() as i32);
        assert_eq!(
            link_url_at(&bare_domain).as_deref(),
            Some("https://example.net")
        );
        Ok(())
    }

    #[test]
    fn heading_menu_formats_and_clears_cursor_block() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::heading_menu_formats_and_clears_cursor_block",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        set_editor_view_content(&editor, "Title\nFirst paragraph\nSecond paragraph");
        let buffer = editor.buffer();
        style_note_heading(&buffer);
        buffer.place_cursor(&buffer.start_iter());
        let menu = editor_heading_menu(&editor);
        assert_eq!(menu.label().as_deref(), Some("H1"));
        buffer.place_cursor(&buffer.iter_at_line(1).unwrap());
        assert_eq!(menu.label().as_deref(), Some("P"));
        let second_line = buffer.iter_at_line(2).unwrap();
        buffer.place_cursor(&second_line);
        assert_eq!(menu.label().as_deref(), Some("P"));

        fn activate_menu_choice(menu: &gtk::MenuButton, index: usize) {
            let choices = menu
                .popover()
                .unwrap()
                .child()
                .unwrap()
                .downcast::<gtk::Box>()
                .unwrap();
            let mut option = choices.first_child().unwrap();
            for _ in 0..index {
                option = option.next_sibling().unwrap();
            }
            option.downcast::<gtk::Button>().unwrap().emit_clicked();
        }

        activate_menu_choice(&menu, 2);
        assert_eq!(menu.label().as_deref(), Some("H2"));
        assert!(
            buffer
                .iter_at_line(2)
                .unwrap()
                .has_tag(&heading_style_tag(&buffer, 2))
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nFirst paragraph\n## Second paragraph"
        );

        activate_menu_choice(&menu, 0);
        assert_eq!(menu.label().as_deref(), Some("P"));
        assert!(
            !buffer
                .iter_at_line(2)
                .unwrap()
                .has_tag(&heading_style_tag(&buffer, 2))
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nFirst paragraph\nSecond paragraph"
        );

        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");
        let blank_line = buffer.iter_at_line(3).unwrap();
        buffer.place_cursor(&blank_line);
        activate_menu_choice(&menu, 3);
        assert_eq!(menu.label().as_deref(), Some("H3"));
        buffer.insert_at_cursor("New heading");
        assert_eq!(menu.label().as_deref(), Some("H3"));
        assert!(
            buffer
                .iter_at_line(3)
                .unwrap()
                .has_tag(&heading_style_tag(&buffer, 3))
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nFirst paragraph\nSecond paragraph\n### New heading"
        );
        Ok(())
    }

    #[test]
    fn editor_text_undo_and_redo_preserve_user_edits_only() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::editor_text_undo_and_redo_preserve_user_edits_only",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        set_editor_view_content(&editor, "Title\nBody");
        let toolbar = editor_format_toolbar(&editor, new_editor_image_history());
        let undo_button = toolbar
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let redo_button = undo_button
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let buffer = editor.buffer();
        assert!(!buffer.can_undo());
        assert!(!undo_button.is_sensitive());
        assert!(!redo_button.is_sensitive());

        buffer.insert_at_cursor(" edited");
        assert!(buffer.can_undo());
        assert!(undo_button.is_sensitive());
        assert!(!redo_button.is_sensitive());
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\nBody edited");
        buffer.undo();
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\nBody");
        assert!(buffer.can_redo());
        assert!(!undo_button.is_sensitive());
        assert!(redo_button.is_sensitive());
        buffer.redo();
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\nBody edited");
        assert!(undo_button.is_sensitive());
        assert!(!redo_button.is_sensitive());

        set_editor_view_content(&editor, "Remote\nReplacement");
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert!(!undo_button.is_sensitive());
        assert!(!redo_button.is_sensitive());
        Ok(())
    }

    #[test]
    fn task_and_bullet_prefixes_toggle_and_convert() {
        assert_eq!(
            list_prefix_change("Buy tickets", ListPrefixKind::Task),
            (0, "☐ ")
        );
        assert_eq!(
            list_prefix_change("☐ Buy tickets", ListPrefixKind::Task),
            (2, "☑ ")
        );
        assert_eq!(
            list_prefix_change("☑ Buy tickets", ListPrefixKind::Task),
            (2, "☐ ")
        );
        assert_eq!(
            list_prefix_change("☑ Buy tickets", ListPrefixKind::Bullet),
            (2, "• ")
        );
        assert_eq!(
            list_prefix_change("• Buy tickets", ListPrefixKind::Bullet),
            (2, "")
        );
        assert_eq!(
            editor_text_from_markdown("- [ ] Task\n- [x] Done\n- Bullet\n* Star\n+ Plus"),
            "☐ Task\n☑ Done\n• Bullet\n• Star\n• Plus"
        );
        assert_eq!(
            markdown_from_editor_text("☐ Task\n☑ Done\n• Bullet"),
            "- [ ] Task\n- [x] Done\n- Bullet"
        );
        assert_eq!(
            editor_text_from_markdown("```md\n- [ ] literal\n```"),
            "```md\n- [ ] literal\n```"
        );
        assert_eq!(serialize_marked_text("[label](url)", &[]), "[label](url)");
    }

    #[test]
    fn task_checkbox_toggles_and_enter_continues_then_ends_the_list() -> Result<()> {
        if std::env::var_os("MEMONOTES_EDITOR_FORMAT_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_format_tests::task_checkbox_toggles_and_enter_continues_then_ends_the_list",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_EDITOR_FORMAT_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let display = gtk::gdk::Display::default().unwrap();
        let provider = gtk::CssProvider::new();
        provider.load_from_data(&note_detail_css(false));
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let editor = gtk::TextView::new();
        set_editor_view_content(&editor, "Title\n- [ ] Task");
        let window = gtk::Window::builder()
            .default_width(480)
            .default_height(180)
            .child(&editor)
            .build();
        window.present();
        while gtk::glib::MainContext::default().pending() {
            gtk::glib::MainContext::default().iteration(false);
        }
        let buffer = editor.buffer();
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- [ ] Task");

        let anchor = buffer.iter_at_offset(6).child_anchor().unwrap();
        let checkbox = anchor
            .widgets()
            .into_iter()
            .next()
            .unwrap()
            .downcast::<gtk::ToggleButton>()
            .unwrap();
        let checkbox_bounds = checkbox.compute_bounds(&editor).unwrap();
        let text_bounds = editor.iter_location(&buffer.iter_at_offset(8));
        let checkbox_center = checkbox_bounds.y() + checkbox_bounds.height() / 2.0;
        let text_center = text_bounds.y() as f32 + text_bounds.height() as f32 / 2.0;
        assert!(
            (checkbox_center - text_center).abs() <= 1.0,
            "checkbox center {checkbox_center}; text center {text_center}; checkbox {checkbox_bounds:?}; text {text_bounds:?}"
        );
        let (cursor, _) = editor.cursor_locations(Some(&buffer.iter_at_offset(8)));
        assert!(
            cursor.height() <= 20,
            "text cursor height is {}px",
            cursor.height()
        );
        assert_eq!(checkbox.valign(), gtk::Align::End);
        assert_eq!(
            checkbox.cursor().unwrap().name().as_deref(),
            Some("pointer")
        );
        let checkmark = checkbox.child().unwrap().downcast::<gtk::Label>().unwrap();
        assert_eq!(checkmark.halign(), gtk::Align::Center);
        assert_eq!(checkmark.valign(), gtk::Align::Center);
        assert_eq!(checkmark.xalign(), 0.5);
        checkbox.set_active(true);
        assert_eq!(checkmark.text(), "✓");
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- [x] Task");

        let changed_handler = buffer.connect_changed(|buffer| {
            style_note_heading(buffer);
            let _ = markdown_from_editor_buffer(buffer);
        });
        for click in 0..100 {
            let checked = click % 2 != 0;
            checkbox.set_active(checked);
            assert_eq!(checkmark.text(), if checked { "✓" } else { "" });
            assert_eq!(
                markdown_from_editor_buffer(&buffer),
                if checked {
                    "# Title\n- [x] Task"
                } else {
                    "# Title\n- [ ] Task"
                }
            );
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
        }
        buffer.disconnect(changed_handler);

        for refresh in 0..20 {
            let anchor = buffer.iter_at_offset(6).child_anchor().unwrap();
            let widget = anchor.widgets().into_iter().next().unwrap();
            let buffer_for_unrealize = buffer.clone();
            let anchor_for_unrealize = anchor.clone();
            let removed_before_delete = Rc::new(Cell::new(false));
            let removed_for_unrealize = Rc::clone(&removed_before_delete);
            widget.connect_unrealize(move |_| {
                removed_for_unrealize.set(!anchor_for_unrealize.is_deleted());
                let _ = buffer_for_unrealize.selection_bounds();
            });
            let content = if refresh % 2 == 0 {
                "Title\n- [ ] Task"
            } else {
                "Title\n- [x] Task"
            };
            set_editor_view_content(&editor, content);
            assert!(removed_before_delete.get());
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            assert_eq!(markdown_from_editor_buffer(&buffer), format!("# {content}"));
        }

        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        assert!(continue_editor_list_on_enter(&editor));
        assert!(
            buffer
                .slice(&buffer.start_iter(), &buffer.end_iter(), true)
                .contains("\n\u{fffc} ")
        );
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- [x] Task\n- [ ] "
        );

        assert!(continue_editor_list_on_enter(&editor));
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n- [x] Task\n"
        );

        set_editor_view_content(&editor, "Title\n- Bullet");
        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        assert!(continue_editor_list_on_enter(&editor));
        assert!(continue_editor_list_on_enter(&editor));
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- Bullet\n");

        set_editor_view_content(&editor, "Title\nTask");
        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        toggle_list_prefix(&editor, ListPrefixKind::Task);
        assert!(
            buffer
                .slice(&buffer.start_iter(), &buffer.end_iter(), true)
                .contains("\n\u{fffc} Task")
        );
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let mut item_end = buffer.iter_at_line(cursor.line()).unwrap();
        item_end.forward_to_line_end();
        assert_eq!(cursor.offset(), item_end.offset());
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- [ ] Task");
        toggle_list_prefix(&editor, ListPrefixKind::Task);
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- [x] Task");
        toggle_list_prefix(&editor, ListPrefixKind::Bullet);
        assert_eq!(markdown_from_editor_buffer(&buffer), "# Title\n- Task");

        set_editor_view_content(&editor, "Title\nOne\n- Two\nThree");
        buffer.select_range(&buffer.iter_at_line(1).unwrap(), &buffer.end_iter());
        toggle_numbered_list(&editor);
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\n1. One\n2. Two\n3. Three"
        );
        toggle_numbered_list(&editor);
        assert_eq!(
            markdown_from_editor_buffer(&buffer),
            "# Title\nOne\nTwo\nThree"
        );

        set_editor_view_content(
            &editor,
            "Title\n- [ ] One\n- [ ] Two\n- [ ] Three\n- [ ] Four\n- [ ] Five\n- [ ] Last",
        );
        let mut typing_position = buffer.iter_at_line(5).unwrap();
        let typing_anchor = typing_position.child_anchor().unwrap();
        typing_position.forward_chars(4);
        buffer.place_cursor(&typing_position);
        for _ in 0..40 {
            buffer.insert_at_cursor("n");
            let cursor_offset = buffer.iter_at_mark(&buffer.get_insert()).offset();
            let content = markdown_from_editor_buffer(&buffer);
            set_editor_view_content(&editor, &content);
            assert_eq!(
                buffer.iter_at_mark(&buffer.get_insert()).offset(),
                cursor_offset
            );
            assert_eq!(buffer.iter_at_mark(&buffer.get_insert()).line(), 5);
            assert!(!typing_anchor.is_deleted());
        }
        let selection_start = buffer.iter_at_line(5).unwrap();
        let mut selection_end = selection_start;
        selection_end.forward_chars(6);
        buffer.select_range(&selection_start, &selection_end);
        let content = markdown_from_editor_buffer(&buffer);
        set_editor_view_content(&editor, &content);
        assert_eq!(
            buffer.iter_at_mark(&buffer.get_insert()).offset(),
            selection_start.offset()
        );
        assert_eq!(
            buffer.iter_at_mark(&buffer.selection_bound()).offset(),
            selection_end.offset()
        );
        Ok(())
    }
}

fn prepare_empty_heading(content: &gtk::TextView) {
    content.add_css_class("empty-note-heading");
    let editor = content.downgrade();
    content.buffer().connect_changed(move |buffer| {
        let Some(editor) = editor.upgrade() else {
            return;
        };
        if buffer.char_count() == 0 {
            editor.add_css_class("empty-note-heading");
        } else {
            editor.remove_css_class("empty-note-heading");
        }
    });
}

fn style_note_heading(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();
    let tag = table.lookup("note-h1").unwrap_or_else(|| {
        let tag = gtk::TextTag::builder()
            .name("note-h1")
            .weight(700)
            .scale(1.65)
            .pixels_above_lines(8)
            .pixels_below_lines(8)
            .build();
        table.add(&tag);
        tag
    });
    let (start, end) = buffer.bounds();
    buffer.remove_tag(&tag, &start, &end);
    let mut heading_end = start;
    heading_end.forward_to_line_end();
    buffer.apply_tag(&tag, &start, &heading_end);
}

fn heading_style_tag(buffer: &gtk::TextBuffer, level: u8) -> gtk::TextTag {
    let (name, scale, spacing) = match level {
        1 => ("editor-h1", 1.65, 8),
        2 => ("editor-h2", 1.4, 7),
        3 => ("editor-h3", 1.2, 6),
        4 => ("editor-h4", 1.1, 5),
        5 => ("editor-h5", 1.0, 4),
        _ => ("editor-h6", 0.95, 4),
    };
    let table = buffer.tag_table();
    table.lookup(name).unwrap_or_else(|| {
        let tag = gtk::TextTag::builder()
            .name(name)
            .weight(700)
            .scale(scale)
            .pixels_above_lines(spacing)
            .pixels_below_lines(spacing)
            .build();
        table.add(&tag);
        tag
    })
}

fn inline_style_tag(buffer: &gtk::TextBuffer, mark: Mark) -> gtk::TextTag {
    let (name, build): (&str, fn() -> gtk::TextTag) = match mark {
        Mark::Bold => ("editor-mark-bold", || {
            gtk::TextTag::builder()
                .name("editor-mark-bold")
                .weight(700)
                .build()
        }),
        Mark::Italic => ("editor-mark-italic", || {
            gtk::TextTag::builder()
                .name("editor-mark-italic")
                .style(gtk::pango::Style::Italic)
                .build()
        }),
        Mark::Underline => ("editor-mark-underline", || {
            gtk::TextTag::builder()
                .name("editor-mark-underline")
                .underline(gtk::pango::Underline::Single)
                .build()
        }),
        Mark::Strikethrough => ("editor-mark-strikethrough", || {
            gtk::TextTag::builder()
                .name("editor-mark-strikethrough")
                .strikethrough(true)
                .build()
        }),
        Mark::Code => ("editor-mark-code", || {
            gtk::TextTag::builder()
                .name("editor-mark-code")
                .family("monospace")
                .build()
        }),
    };
    editor_text_tag(buffer, name, build)
}

fn apply_rendered_spans(
    editor: &gtk::TextView,
    buffer: &gtk::TextBuffer,
    text: &str,
    spans: &[MarkupSpan],
    headings: &[memonotes::editor_document::HeadingSpan],
    links: &[LinkSpan],
    images: &[ImageSpan],
    tables: &[memonotes::editor_document::TableSpan],
    rules: &[std::ops::Range<usize>],
) {
    let (start, end) = buffer.bounds();
    let char_offsets = memonotes::editor_document::byte_to_char_offsets(text);
    remove_link_tags(buffer, &start, &end);
    let marks = [
        Mark::Bold,
        Mark::Italic,
        Mark::Underline,
        Mark::Strikethrough,
        Mark::Code,
    ];
    let tags: Vec<_> = marks
        .into_iter()
        .map(|mark| (mark, inline_style_tag(buffer, mark)))
        .collect();
    for (_, tag) in &tags {
        buffer.remove_tag(tag, &start, &end);
    }

    for span in spans {
        let Some(mark) = span.mark else {
            continue;
        };
        let start_offset = char_offsets[span.range.start] as i32;
        let end_offset = char_offsets[span.range.end] as i32;
        let start = buffer.iter_at_offset(start_offset);
        let end = buffer.iter_at_offset(end_offset);
        if let Some((_, tag)) = tags.iter().find(|(tag_mark, _)| *tag_mark == mark) {
            buffer.apply_tag(tag, &start, &end);
        }
    }

    for heading in headings {
        let start_offset = char_offsets[heading.range.start] as i32;
        let end_offset = char_offsets[heading.range.end] as i32;
        let start = buffer.iter_at_offset(start_offset);
        let end = buffer.iter_at_offset(end_offset);
        buffer.apply_tag(&heading_style_tag(buffer, heading.level), &start, &end);
    }

    for link in links {
        let start = buffer.iter_at_offset(char_offsets[link.range.start] as i32);
        let end = buffer.iter_at_offset(char_offsets[link.range.end] as i32);
        buffer.apply_tag(&link_tag(buffer, &link.url), &start, &end);
    }

    for image in images.iter().rev() {
        let offset = char_offsets[image.range.start] as i32;
        let mut start = buffer.iter_at_offset(offset);
        let mut end = start;
        end.forward_char();
        buffer.delete(&mut start, &mut end);
        let anchor = buffer.create_child_anchor(&mut start);
        insert_editor_image_at_anchor(editor, &anchor, &image.url, &image.alt);
    }

    for table in tables.iter().rev() {
        let offset = char_offsets[table.range.start] as i32;
        let mut start = buffer.iter_at_offset(offset);
        let mut end = start;
        end.forward_char();
        buffer.delete(&mut start, &mut end);
        let anchor = buffer.create_child_anchor(&mut start);
        let widget = editor_table::build_table_widget(editor, &table.alignments, &table.rows);
        editor.add_child_at_anchor(&widget, &anchor);
    }

    for rule in rules.iter().rev() {
        let mut start = buffer.iter_at_offset(char_offsets[rule.start] as i32);
        let mut end = start;
        end.forward_char();
        buffer.delete(&mut start, &mut end);
        let anchor = buffer.create_child_anchor(&mut start);
        editor.add_child_at_anchor(&editor_table::build_rule_widget(editor), &anchor);
    }
}

fn remove_editor_anchor_widgets(
    editor: &gtk::TextView,
    mut iter: gtk::TextIter,
    end: gtk::TextIter,
) {
    while iter.offset() < end.offset() {
        if let Some(anchor) = iter.child_anchor() {
            for widget in anchor.widgets() {
                editor.remove(&widget);
            }
        }
        iter.forward_char();
    }
}

fn set_editor_view_content(editor: &gtk::TextView, content: &str) {
    let content = content.replace('\u{fffc}', "");
    let content = content.as_str();
    let buffer = editor.buffer();
    let (title, body) = memonotes::memo::editor_parts(content);
    if markdown_from_editor_buffer(&buffer) == memonotes::memo::editor_content(title, body) {
        return;
    }
    let mapped = editor_text_from_markdown(content);
    let rendered = render_markdown_source(&mapped);
    let (start, end) = buffer.bounds();
    editor.add_css_class("editor-content-replacing");
    deselect_editor_image(editor);
    buffer.set_enable_undo(false);
    remove_editor_anchor_widgets(editor, start, end);
    buffer.set_text(&rendered.text);
    apply_rendered_spans(
        editor,
        &buffer,
        &rendered.text,
        &rendered.spans,
        &rendered.headings,
        &rendered.links,
        &rendered.images,
        &rendered.tables,
        &rendered.rules,
    );
    install_task_checkbuttons(editor);
    auto_link_plain_urls(&buffer);
    buffer.set_enable_undo(true);
    editor.remove_css_class("editor-content-replacing");
    // The skipped change handlers refresh from these notifications.
    buffer.notify("can-undo");
    buffer.notify("can-redo");
    buffer.notify("cursor-position");
    update_editor_cursor_visibility(editor);
}

fn task_checkbox_checked_tag(buffer: &gtk::TextBuffer) -> gtk::TextTag {
    editor_text_tag(buffer, "task-checkbox-checked", || {
        gtk::TextTag::builder()
            .name("task-checkbox-checked")
            .build()
    })
}

fn bullet_marker_tag(buffer: &gtk::TextBuffer) -> gtk::TextTag {
    editor_text_tag(buffer, "bullet-marker", || {
        gtk::TextTag::builder().name("bullet-marker").build()
    })
}

fn after_bullet_marker(iter: &gtk::TextIter, tag: &gtk::TextTag) -> bool {
    let mut previous = *iter;
    previous.backward_char() && previous.has_tag(tag)
}

// Selection text without bullet markers (anchor plus its space), or None when none are selected.
fn selection_text_without_bullets(buffer: &gtk::TextBuffer) -> Option<String> {
    let (start, end) = buffer.selection_bounds()?;
    let tag = bullet_marker_tag(buffer);
    let mut text = String::new();
    let mut has_bullet = false;
    let mut iter = start;
    while iter < end {
        if iter.has_tag(&tag) {
            has_bullet = true;
            iter.forward_chars(2);
            continue;
        }
        if iter.char() != '\u{fffc}' {
            text.push(iter.char());
        }
        iter.forward_char();
    }
    has_bullet.then_some(text)
}

fn install_bullet_selection_guard(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let adjusting = Cell::new(false);
    buffer.connect_mark_set(move |buffer, _, mark| {
        if adjusting.get() || !matches!(mark.name().as_deref(), Some("insert" | "selection_bound"))
        {
            return;
        }
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        let tag = bullet_marker_tag(buffer);
        let mut new_start = start;
        if start.has_tag(&tag) {
            new_start.forward_chars(2);
        } else if after_bullet_marker(&start, &tag) {
            new_start.forward_char();
        }
        let mut new_end = end;
        if after_bullet_marker(&end, &tag) {
            new_end.backward_char();
        }
        if new_start >= new_end {
            new_end = new_start;
        }
        if new_start == start && new_end == end {
            return;
        }
        let insert_is_start = buffer.iter_at_mark(&buffer.get_insert()) == start;
        let (insert, bound) = if insert_is_start {
            (new_start, new_end)
        } else {
            (new_end, new_start)
        };
        adjusting.set(true);
        buffer.move_mark(&buffer.get_insert(), &insert);
        buffer.move_mark(&buffer.selection_bound(), &bound);
        adjusting.set(false);
    });
    editor.connect_copy_clipboard(|editor| {
        if let Some(text) = selection_text_without_bullets(&editor.buffer()) {
            editor.stop_signal_emission_by_name("copy-clipboard");
            editor.clipboard().set_text(&text);
        }
    });
    editor.connect_cut_clipboard(|editor| {
        let buffer = editor.buffer();
        if let Some(text) = selection_text_without_bullets(&buffer) {
            editor.stop_signal_emission_by_name("cut-clipboard");
            editor.clipboard().set_text(&text);
            buffer.delete_selection(true, editor.is_editable());
        }
    });
}

fn install_task_checkbuttons(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let (start, end) = buffer.bounds();
    let text = buffer.slice(&start, &end, true).to_string();
    let mut checkbox_offsets = Vec::new();
    let mut offset = 0;
    let mut in_fenced_code = false;

    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fenced_code = !in_fenced_code;
        } else if !in_fenced_code {
            let indentation = line.chars().count() - trimmed.chars().count();
            if trimmed.starts_with("☐ ") {
                checkbox_offsets.push((offset + indentation, false));
            } else if trimmed.starts_with("☑ ") {
                checkbox_offsets.push((offset + indentation, true));
            } else if trimmed.starts_with("• ") {
                checkbox_offsets.push((offset + indentation, false));
            }
        }
        offset += line.chars().count() + 1;
    }

    let checked_tag = task_checkbox_checked_tag(&buffer);
    for (offset, checked) in checkbox_offsets.into_iter().rev() {
        let mut marker_start = buffer.iter_at_offset(offset as i32);
        let bullet = marker_start.char() == '•';
        let mut marker_end = marker_start;
        marker_end.forward_chars(2);
        buffer.delete(&mut marker_start, &mut marker_end);

        let mut anchor_iter = buffer.iter_at_offset(offset as i32);
        let anchor = buffer.create_child_anchor(&mut anchor_iter);
        buffer.insert(&mut anchor_iter, " ");
        let anchor_start = buffer.iter_at_child_anchor(&anchor);
        let mut anchor_end = anchor_start;
        anchor_end.forward_char();
        buffer.remove_tag(&bullet_marker_tag(&buffer), &anchor_start, &anchor_end);
        buffer.remove_tag(&checked_tag, &anchor_start, &anchor_end);
        if bullet {
            let anchor_start = buffer.iter_at_child_anchor(&anchor);
            let mut anchor_end = anchor_start;
            anchor_end.forward_char();
            buffer.apply_tag(&bullet_marker_tag(&buffer), &anchor_start, &anchor_end);
            let marker = gtk::Label::new(Some("•"));
            marker.set_selectable(false);
            marker.set_can_target(false);
            marker.add_css_class("editor-bullet");
            marker.set_valign(gtk::Align::Baseline);
            let text_layout = editor.create_pango_layout(Some(" "));
            let (_, text_extent) = text_layout.pixel_extents();
            let (bullet_ink, bullet_extent) = marker.layout().pixel_extents();
            let bullet_center = bullet_ink.y() + bullet_ink.height() / 2;
            let rise = text_layout.baseline()
                - (text_extent.height() / 2 + bullet_extent.height() - bullet_center)
                    * gtk::pango::SCALE;
            bullet_marker_tag(&buffer).set_rise(rise);
            editor.add_child_at_anchor(&marker, &anchor);
            continue;
        }
        if checked {
            let anchor_start = buffer.iter_at_child_anchor(&anchor);
            let mut anchor_end = anchor_start;
            anchor_end.forward_char();
            buffer.apply_tag(&checked_tag, &anchor_start, &anchor_end);
        }

        let checkbox = gtk::ToggleButton::new();
        checkbox.set_size_request(16, 16);
        checkbox.set_halign(gtk::Align::Center);
        checkbox.set_valign(gtk::Align::End);
        checkbox.set_focus_on_click(false);
        checkbox.set_cursor_from_name(Some("pointer"));
        checkbox.add_css_class("task-checkbox");
        checkbox.set_tooltip_text(Some(tr("editor.format.task_list")));
        let checkmark = gtk::Label::builder()
            .label(if checked { "✓" } else { "" })
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .xalign(0.5)
            .yalign(0.5)
            .build();
        checkbox.set_child(Some(&checkmark));
        checkbox.set_active(checked);

        let buffer_for_toggle = buffer.clone();
        let anchor_for_toggle = anchor.clone();
        let checked_tag_for_toggle = checked_tag.clone();
        checkbox.connect_toggled(move |checkbox| {
            let start = buffer_for_toggle.iter_at_child_anchor(&anchor_for_toggle);
            let mut end = start;
            end.forward_char();
            if checkbox.is_active() {
                buffer_for_toggle.apply_tag(&checked_tag_for_toggle, &start, &end);
                checkmark.set_text("✓");
            } else {
                buffer_for_toggle.remove_tag(&checked_tag_for_toggle, &start, &end);
                checkmark.set_text("");
            }
            buffer_for_toggle.emit_by_name::<()>("changed", &[]);
        });
        editor.add_child_at_anchor(&checkbox, &anchor);
    }
}

fn markdown_from_editor_buffer(buffer: &gtk::TextBuffer) -> String {
    let (start, end) = buffer.bounds();
    let text = buffer.slice(&start, &end, true).to_string();
    let mut lines = text.split_inclusive('\n');
    let title_line = lines.next().unwrap_or_default();
    let title = title_line.strip_suffix('\n').unwrap_or(title_line);
    let mut body = String::new();
    let mut char_offset = title_line.chars().count();
    let mut after_table = false;

    for line_with_ending in lines {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indentation = line.chars().count() - trimmed.chars().count();
        let marker_offset = char_offset + indentation;
        let line_chars: Vec<_> = line.chars().collect();
        if std::mem::take(&mut after_table) && !line.trim().is_empty() {
            body.push('\n');
        }
        // A table is its own Markdown block; text beside its anchor becomes separate blocks.
        let table_at = line_chars.iter().enumerate().find_map(|(index, character)| {
            (*character == '\u{fffc}')
                .then(|| table_markdown_at(buffer, char_offset + index))
                .flatten()
                .map(|table| (index, table))
        });
        if let Some((index, table)) = table_at {
            let before = serialize_inline_range(buffer, &line_chars[..index], char_offset);
            let after = serialize_inline_range(
                buffer,
                &line_chars[index + 1..],
                char_offset + index + 1,
            );
            if !before.trim().is_empty() {
                start_markdown_block(&mut body);
                body.push_str(&before);
            }
            start_markdown_block(&mut body);
            body.push_str(&table);
            if !after.trim().is_empty() {
                start_markdown_block(&mut body);
                body.push_str(&after);
            } else {
                after_table = true;
            }
            if line_with_ending.ends_with('\n') {
                body.push('\n');
            }
            char_offset += line_with_ending.chars().count();
            continue;
        }
        let line_start = buffer.iter_at_offset(char_offset as i32);
        let heading_level =
            (1..=6).find(|level| line_start.has_tag(&heading_style_tag(buffer, *level)));
        let (prefix, prefix_chars) = if trimmed.starts_with("\u{fffc} ") {
            let checkbox = buffer.iter_at_offset(marker_offset as i32);
            let checked = checkbox.has_tag(&task_checkbox_checked_tag(buffer));
            let marker = if checkbox.has_tag(&bullet_marker_tag(buffer)) {
                "• "
            } else if checked {
                "☑ "
            } else {
                "☐ "
            };
            (marker.to_owned(), 2)
        } else if trimmed.starts_with("☐ ") {
            ("☐ ".to_owned(), 2)
        } else if trimmed.starts_with("☑ ") {
            ("☑ ".to_owned(), 2)
        } else if trimmed.starts_with("• ") {
            ("• ".to_owned(), 2)
        } else if let Some(level) = heading_level {
            (format!("{} ", "#".repeat(level as usize)), 0)
        } else {
            (String::new(), 0)
        };
        let content_start = if prefix_chars > 0 {
            indentation + prefix_chars
        } else {
            0
        };
        if prefix_chars > 0 {
            body.extend(line_chars[..indentation].iter());
        }
        body.push_str(&prefix);
        body.push_str(&serialize_inline_range(
            buffer,
            &line_chars[content_start..],
            char_offset + content_start,
        ));
        if line_with_ending.ends_with('\n') {
            body.push('\n');
        }
        char_offset += line_with_ending.chars().count();
    }

    let body = markdown_from_editor_text(&body);
    memonotes::memo::editor_content(title, &body)
}

fn table_markdown_at(buffer: &gtk::TextBuffer, offset: usize) -> Option<String> {
    buffer
        .iter_at_offset(offset as i32)
        .child_anchor()?
        .widgets()
        .into_iter()
        .find_map(|widget| {
            if editor_table::is_rule_widget(&widget) {
                Some("---".to_owned())
            } else {
                editor_table::table_markdown_from_widget(&widget)
            }
        })
}

// Leaves one blank line between the previous Markdown block and the next one.
fn start_markdown_block(body: &mut String) {
    if body.is_empty() {
        return;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    // The title line precedes the body, so a lone newline already is a blank line.
    if !body.ends_with("\n\n") && body != "\n" {
        body.push('\n');
    }
}

fn serialize_inline_range(
    buffer: &gtk::TextBuffer,
    characters: &[char],
    start_offset: usize,
) -> String {
    let marks = [
        Mark::Bold,
        Mark::Italic,
        Mark::Underline,
        Mark::Strikethrough,
        Mark::Code,
    ];
    let tags: Vec<_> = marks
        .into_iter()
        .map(|mark| (mark, inline_style_tag(buffer, mark)))
        .collect();
    let mut runs: Vec<(Option<String>, Vec<Mark>, String, Option<String>)> = Vec::new();

    // has_tag and iter_at_offset are linear in the line length, so walk one iterator and only
    // look at the tags where they change.
    let mut position = buffer.iter_at_offset(start_offset as i32);
    let mut style: Option<(Vec<Mark>, Option<String>, gtk::TextIter)> = None;
    for character in characters {
        let iter = position;
        position.forward_char();
        if *character == '\u{fffc}'
            && let Some(image) = iter.child_anchor().and_then(|anchor| {
                anchor
                    .widgets()
                    .into_iter()
                    .find_map(|widget| editor_image_markdown(&widget))
            })
        {
            runs.push((None, Vec::new(), String::new(), Some(image)));
            continue;
        }
        // An anchor without an image widget must never reach the saved Markdown.
        if *character == '\u{fffc}' {
            continue;
        }
        if style.as_ref().is_none_or(|(_, _, until)| iter >= *until) {
            let mut until = iter;
            until.forward_to_tag_toggle(None::<&gtk::TextTag>);
            let marks: Vec<_> = tags
                .iter()
                .filter(|(_, tag)| iter.has_tag(tag))
                .map(|(mark, _)| *mark)
                .collect();
            style = Some((marks, link_url_at(&iter), until));
        }
        let Some((marks, url, _)) = style.as_ref() else {
            continue;
        };
        match runs.last_mut() {
            Some((last_url, last_marks, text, None))
                if last_url == url && last_marks == marks =>
            {
                text.push(*character)
            }
            _ => runs.push((url.clone(), marks.clone(), character.to_string(), None)),
        }
    }

    let mut output = String::new();
    let mut index = 0;
    while index < runs.len() {
        if let Some(image) = &runs[index].3 {
            output.push_str(image);
            index += 1;
            continue;
        }
        let url = runs[index].0.clone();
        let mut label = String::new();
        while index < runs.len() && runs[index].3.is_none() && runs[index].0 == url {
            label.push_str(&serialize_marked_text(&runs[index].2, &runs[index].1));
            index += 1;
        }
        match url {
            Some(url) => output.push_str(&markdown_link(&label, &url)),
            None => output.push_str(&label),
        }
    }
    output
}

fn serialize_marked_text(text: &str, marks: &[Mark]) -> String {
    if marks.is_empty() {
        text.to_owned()
    } else {
        markdown_inline_text(text, marks)
    }
}

fn editor_text_tag(
    buffer: &gtk::TextBuffer,
    name: &str,
    build: impl FnOnce() -> gtk::TextTag,
) -> gtk::TextTag {
    let table = buffer.tag_table();
    table.lookup(name).unwrap_or_else(|| {
        let tag = build();
        table.add(&tag);
        tag
    })
}

fn set_editor_text(state: &WindowState, content: &str) {
    state.suppress_editor_events.set(true);
    set_editor_view_content(&state.content, content);
    style_note_heading(&state.content.buffer());
    state.suppress_editor_events.set(false);
}

fn clear_note_selection(state: &WindowState) {
    *state.note_id.borrow_mut() = None;
    state.note_list.unselect_all();
    state.pin_button.set_sensitive(false);
    state.archive_button.set_sensitive(false);
    state.delete_button.set_visible(false);
    state.folder_button.set_sensitive(false);
    state.folder_picker.set_sensitive(false);
    set_editor_text(state, "");
}

fn archive_current_note(state: &Rc<WindowState>) {
    let note = state
        .note_id
        .borrow()
        .as_deref()
        .and_then(|note_id| state.store.borrow().note(note_id).ok().flatten());
    if note.is_some_and(|note| note.content.trim().is_empty()) {
        delete_current_note(state);
    } else {
        set_current_note_archived(state, true);
    }
}

fn delete_current_note(state: &Rc<WindowState>) {
    let Some(note_id) = state.note_id.borrow().clone() else {
        return;
    };
    let note = state.store.borrow().note(&note_id).ok().flatten();
    let Some(note) = note else {
        return;
    };
    if !note.archived && !note.content.trim().is_empty() {
        return;
    }
    let Some(parent) = state.note_list.view.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let alert = adw::AlertDialog::builder()
        .heading(tr("note.delete.confirm_title"))
        .body(if note.account_id.is_some() {
            tr("note.delete.confirm_synced_message")
        } else {
            tr("note.delete.confirm_local_message")
        })
        .build();
    alert.add_responses(&[
        ("cancel", tr("action.cancel")),
        ("delete", tr("action.delete")),
    ]);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");
    alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

    let state = Rc::clone(state);
    alert.choose(
        Some(&parent),
        None::<&adw::gio::Cancellable>,
        move |response| {
            if response != "delete" {
                return;
            }
            let current = state.store.borrow().note(&note_id).ok().flatten();
            let Some(current) =
                current.filter(|note| note.archived || note.content.trim().is_empty())
            else {
                return;
            };
            let result = state.store.borrow().delete_note(&note_id);
            if let Err(error) = result {
                state
                    .toast_overlay
                    .add_toast(adw::Toast::new(&memonotes::tr_format!(
                        "note.error.delete_failed",
                        error = format!("{error:#}")
                    )));
                return;
            }
            state
                .undo_stack
                .borrow_mut()
                .retain(|operation| match operation {
                    UndoOperation::Archive(note) => note.id != note_id,
                });
            let note_window = state.note_windows.borrow().get(&note_id).cloned();
            if let Some(window) = note_window {
                window.close();
            }
            debounce_backend_save(&state, current.account_id.as_deref());
            if state.note_id.borrow().as_deref() == Some(note_id.as_str()) {
                clear_note_selection(&state);
            }
            let _ = refresh_notes(&state);
        },
    );
}

fn archive_selected_notes(state: &Rc<WindowState>) {
    let ids = state.note_list.selected_ids();
    for id in ids {
        let note = state.store.borrow().note(&id).ok().flatten();
        let Some(note) = note else {
            continue;
        };
        if state.store.borrow().set_archived(&id, true).is_ok() {
            state
                .undo_stack
                .borrow_mut()
                .push(UndoOperation::Archive(note));
        }
    }
    clear_note_selection(state);
    let _ = refresh_notes(state);
}

fn delete_selected_notes(state: &Rc<WindowState>) {
    let notes: Vec<Note> = state
        .note_list
        .selected_ids()
        .iter()
        .filter_map(|id| state.store.borrow().note(id).ok().flatten())
        .filter(|note| note.archived)
        .collect();
    if notes.is_empty() {
        return;
    }
    let Some(parent) = state.note_list.view.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let alert = adw::AlertDialog::builder()
        .heading(tr("note.delete_many.confirm_title"))
        .body(memonotes::tr_format!(
            "note.delete_many.confirm_message",
            count = notes.len()
        ))
        .build();
    alert.add_responses(&[
        ("cancel", tr("action.cancel")),
        ("delete", tr("action.delete")),
    ]);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");
    alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

    let state = Rc::clone(state);
    alert.choose(
        Some(&parent),
        None::<&adw::gio::Cancellable>,
        move |response| {
            if response != "delete" {
                return;
            }
            let mut accounts: Vec<Option<String>> = Vec::new();
            for note in &notes {
                let result = state.store.borrow().delete_note(&note.id);
                if let Err(error) = result {
                    state
                        .toast_overlay
                        .add_toast(adw::Toast::new(&memonotes::tr_format!(
                            "note.error.delete_failed",
                            error = format!("{error:#}")
                        )));
                    break;
                }
                state
                    .undo_stack
                    .borrow_mut()
                    .retain(|operation| match operation {
                        UndoOperation::Archive(archived) => archived.id != note.id,
                    });
                let note_window = state.note_windows.borrow().get(&note.id).cloned();
                if let Some(window) = note_window {
                    window.close();
                }
                if !accounts.contains(&note.account_id) {
                    accounts.push(note.account_id.clone());
                }
            }
            for account_id in accounts {
                debounce_backend_save(&state, account_id.as_deref());
            }
            clear_note_selection(&state);
            let _ = refresh_notes(&state);
        },
    );
}

fn set_current_note_archived(state: &Rc<WindowState>, archived: bool) {
    let Some(note_id) = state.note_id.borrow().clone() else {
        return;
    };
    let note = state.store.borrow().note(&note_id).ok().flatten();
    let Some(note) = note else {
        return;
    };
    if state
        .store
        .borrow()
        .set_archived(&note_id, archived)
        .is_ok()
    {
        state
            .undo_stack
            .borrow_mut()
            .push(UndoOperation::Archive(note));
        clear_note_selection(state);
        let _ = refresh_notes(state);
    }
}

fn undo_last_operation(state: &Rc<WindowState>) {
    let Some(operation) = state.undo_stack.borrow().last().cloned() else {
        return;
    };
    let undo_result = {
        let store = state.store.borrow();
        match &operation {
            UndoOperation::Archive(note) => store.set_archived(&note.id, note.archived),
        }
    };
    if undo_result.is_err() {
        return;
    }
    state.undo_stack.borrow_mut().pop();

    let note_id = match operation {
        UndoOperation::Archive(note) => note.id,
    };
    let note = state.store.borrow().note(&note_id).ok().flatten();
    let Some(note) = note else {
        clear_note_selection(state);
        let _ = refresh_notes(state);
        return;
    };
    let filter = state
        .active_filter
        .borrow()
        .to_note_filter(state.search.text().to_string());
    let is_visible = state
        .store
        .borrow()
        .notes(&filter)
        .is_ok_and(|notes| notes.iter().any(|visible_note| visible_note.id == note.id));
    if is_visible {
        show_note(state, &note);
        *state.selection_anchor.borrow_mut() = Some(note.id.clone());
    } else {
        clear_note_selection(state);
    }
    let _ = refresh_notes(state);
}

fn connect_account_adapter(account: &BackendAccount) -> Result<Box<dyn BackendAdapter>> {
    let token = memonotes::credentials::lookup_token(&account.id)?
        .context(tr("api.memos.error.token_missing"))?;
    memonotes::adapters::connect(&account.provider, &account.server_url, &token)
}

fn database_path() -> Result<PathBuf> {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("XDG_DATA_HOME of HOME is niet ingesteld")?
        .join("memonotes");
    std::fs::create_dir_all(&data_dir).context("Appgegevensmap aanmaken")?;
    Ok(data_dir.join("notes.sqlite3"))
}

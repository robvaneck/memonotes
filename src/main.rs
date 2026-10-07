use adw::gtk;
use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, ToolbarView};
use anyhow::{Context, Result};
use libadwaita as adw;
use memonotes::{BackendAccount, Note, NoteFilter, Store};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const NOTE_COLOR_PALETTE: [(&str, &str, &str, &str); 32] = [
    ("default", "Standaard", "#ffffff", "#242424"),
    ("gray", "Grijs", "#f1f1f0", "#2a2a2a"),
    ("mauve", "Mauve", "#f0eff1", "#2e2b31"),
    ("slate", "Leisteen", "#eff0f3", "#292d32"),
    ("sage", "Salie", "#eff1f0", "#292d2b"),
    ("olive", "Olijf", "#f1f1ed", "#2d2e26"),
    ("sand", "Zand", "#f1f0ed", "#2e2d2b"),
    ("tomato", "Tomaat", "#feebe7", "#3b1813"),
    ("red", "Rood", "#feebec", "#3b1219"),
    ("ruby", "Robijn", "#ffeaef", "#3a141e"),
    ("crimson", "Karmijn", "#ffe9f0", "#381525"),
    ("pink", "Roze", "#fee9f5", "#3b152f"),
    ("plum", "Pruim", "#fce9f8", "#351a36"),
    ("purple", "Paars", "#f2eafe", "#301c3b"),
    ("violet", "Violet", "#f4f0fe", "#2a1c40"),
    ("iris", "Iris", "#efedfe", "#24203f"),
    ("indigo", "Indigo", "#e8f0fe", "#1c2340"),
    ("blue", "Blauw", "#e6f4fe", "#172b46"),
    ("cyan", "Cyaan", "#e1f8fa", "#12343b"),
    ("teal", "Turkoois", "#e1f8f4", "#12372f"),
    ("jade", "Jade", "#e6f7ed", "#123a31"),
    ("green", "Groen", "#e6f6eb", "#17392c"),
    ("grass", "Gras", "#e9f6e9", "#25391d"),
    ("bronze", "Brons", "#f5f0e7", "#352c25"),
    ("gold", "Goud", "#f9f1dd", "#382f1e"),
    ("brown", "Bruin", "#f6eee7", "#38291f"),
    ("orange", "Oranje", "#feefe7", "#3c271b"),
    ("amber", "Amber", "#fff1c6", "#3c3015"),
    ("yellow", "Geel", "#fff7c2", "#3a3214"),
    ("lime", "Limoen", "#f2f9c7", "#303716"),
    ("mint", "Munt", "#e1f9f2", "#15382e"),
    ("sky", "Hemelblauw", "#e1f6fd", "#183747"),
];

const LEGACY_NOTE_COLORS: [&str; 4] = ["rose", "peach", "lemon", "lilac"];

#[derive(Clone, PartialEq, Eq)]
enum ActiveNoteFilter {
    All,
    Unfiled,
    Folder(String),
    Archive,
}

#[derive(Clone)]
enum UndoOperation {
    Archive(Note),
    Delete(Note),
}

impl ActiveNoteFilter {
    fn is_archived(&self) -> bool {
        matches!(self, Self::Archive)
    }

    fn folder_id(&self) -> Option<String> {
        match self {
            Self::Folder(folder_id) => Some(folder_id.clone()),
            Self::All | Self::Unfiled | Self::Archive => None,
        }
    }

    fn for_folder_picker(folder_id: Option<String>) -> Self {
        folder_id.map_or(Self::Unfiled, Self::Folder)
    }

    fn to_note_filter(&self, search: String) -> NoteFilter {
        let (folder_id, unfiled, archived) = match self {
            Self::All => (None, false, false),
            Self::Unfiled => (None, true, false),
            Self::Folder(folder_id) => (Some(folder_id.clone()), false, false),
            Self::Archive => (None, false, true),
        };
        NoteFilter {
            folder_id,
            unfiled,
            archived,
            search,
        }
    }
}

struct WindowState {
    store: RefCell<Store>,
    note_color: RefCell<String>,
    undo_stack: RefCell<Vec<UndoOperation>>,
    active_filter: RefCell<ActiveNoteFilter>,
    note_id: RefCell<Option<String>>,
    note_windows: RefCell<HashMap<String, ApplicationWindow>>,
    note_list: gtk::ListBox,
    note_list_focus: gtk::ScrolledWindow,
    pinned_separator_row: gtk::ListBoxRow,
    search: gtk::SearchEntry,
    title: gtk::Entry,
    content: gtk::TextView,
    pin_button: gtk::Button,
    archive_button: gtk::Button,
    folder_button: gtk::Button,
    folder_popover: gtk::Popover,
    folder_picker: gtk::DropDown,
    folder_options: RefCell<Vec<Option<String>>>,
    folder_count_labels: RefCell<Vec<(String, gtk::Label)>>,
    note_rows: RefCell<HashMap<String, gtk::ListBoxRow>>,
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
        .application_id("app.memonotes.Desktop")
        .flags(flags)
        .build();

    application.connect_activate(|application| {
        if let Err(error) = build_window(application) {
            eprintln!("Memonotes kon niet starten: {error:#}");
        }
    });

    application.run();
    Ok(())
}

fn build_window(application: &Application) -> Result<()> {
    install_pinned_separator_style();
    install_note_detail_style();
    install_account_error_style();
    let store = Store::open(database_path()?).context("Lokale notities openen")?;
    let folders = store.folders()?;
    let initial_notes = store.notes(&NoteFilter::default())?;
    let initial_note = initial_notes.first().cloned();
    let note_color = store.note_color()?;
    let folder_model = gtk::StringList::new(&["Zonder map"]);
    let mut folder_options = vec![None];
    for folder in &folders {
        folder_model.append(&folder.name);
        folder_options.push(Some(folder.id.clone()));
    }

    let note_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Multiple)
        .css_classes(["navigation-sidebar"])
        .build();
    note_list.set_activate_on_single_click(false);
    let list_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&note_list)
        .build();
    list_scroll.set_focusable(true);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Zoek in notities")
        .build();
    search.set_width_chars(12);
    search.set_hexpand(true);
    let title = gtk::Entry::builder()
        .placeholder_text("Nieuwe notities")
        .css_classes(["title-1"])
        .build();
    title.add_css_class("note-detail-surface");
    let content = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(24)
        .left_margin(12)
        .right_margin(12)
        .build();
    content.add_css_class("note-detail-surface");
    let pin_button = icon_button("view-pin-symbolic", "Vastpinnen");
    let archive_button = icon_button("user-trash-symbolic", "Archiveer");
    let folder_picker = gtk::DropDown::new(Some(folder_model.clone()), None::<&gtk::Expression>);
    folder_picker.set_tooltip_text(Some("Verplaats naar map"));
    let folder_button = icon_button("folder-symbolic", "Verplaats naar map");
    let folder_popover = gtk::Popover::new();
    folder_popover.set_has_arrow(false);
    folder_popover.set_child(Some(&folder_picker));
    folder_popover.set_parent(&folder_button);
    let folder_popover_for_button = folder_popover.clone();
    folder_button.connect_clicked(move |_| {
        if folder_popover_for_button.is_visible() {
            folder_popover_for_button.popdown();
        } else {
            folder_popover_for_button.popup();
        }
    });
    let pinned_separator_row = build_pinned_separator_row();
    pin_button.set_sensitive(false);
    archive_button.set_sensitive(false);
    folder_button.set_sensitive(false);
    folder_picker.set_sensitive(false);

    let state = Rc::new(WindowState {
        store: RefCell::new(store),
        note_color: RefCell::new(note_color),
        undo_stack: RefCell::new(Vec::new()),
        active_filter: RefCell::new(ActiveNoteFilter::All),
        note_id: RefCell::new(initial_note.as_ref().map(|note| note.id.clone())),
        note_windows: RefCell::new(HashMap::new()),
        note_list: note_list.clone(),
        note_list_focus: list_scroll.clone(),
        pinned_separator_row,
        search: search.clone(),
        title: title.clone(),
        content: content.clone(),
        pin_button: pin_button.clone(),
        archive_button: archive_button.clone(),
        folder_button: folder_button.clone(),
        folder_popover,
        folder_picker: folder_picker.clone(),
        folder_options: RefCell::new(folder_options),
        folder_count_labels: RefCell::new(Vec::new()),
        note_rows: RefCell::new(HashMap::new()),
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
    let add_folder = icon_button("folder-new-symbolic", "Map toevoegen");
    let sidebar = build_sidebar(&state, &window, &add_folder)?;
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    list_column.set_margin_top(12);
    list_column.set_margin_bottom(12);
    list_column.set_margin_start(12);
    list_column.set_margin_end(12);
    let new_note = icon_button("document-new-symbolic", "Nieuwe notitie");
    list_column.append(&search);
    list_column.append(&list_scroll);

    let editor = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor.add_css_class("note-detail-surface");
    editor.set_margin_bottom(24);
    editor.set_margin_start(32);
    editor.set_margin_end(32);
    let title_row = gtk::Overlay::new();
    title.set_hexpand(true);
    title.set_margin_end(124);
    title_row.set_child(Some(&title));
    let editor_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    editor_actions.append(&pin_button);
    editor_actions.append(&archive_button);
    editor_actions.append(&folder_button);
    editor_actions.set_halign(gtk::Align::End);
    editor_actions.set_valign(gtk::Align::Center);
    title_row.add_overlay(&editor_actions);
    editor.append(&title_row);
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
    sidebar_toggle.set_tooltip_text(Some("Zijbalk tonen of verbergen"));
    let layout_for_toggle = navigation_layout.clone();
    let animation_generation = Rc::new(Cell::new(0_u64));
    let generation_for_toggle = Rc::clone(&animation_generation);
    let sidebar_width_for_toggle = Rc::clone(&sidebar_width);
    sidebar_toggle.connect_toggled(move |button| {
        let generation = generation_for_toggle.get().wrapping_add(1);
        generation_for_toggle.set(generation);
        let target = if button.is_active() {
            sidebar_width_for_toggle.get()
        } else {
            let current_width = layout_for_toggle.position();
            if current_width > 0 {
                sidebar_width_for_toggle.set(current_width);
            }
            0
        };
        animate_sidebar_position(
            &layout_for_toggle,
            target,
            generation,
            Rc::clone(&generation_for_toggle),
        );
    });

    navigation_header.pack_start(&sidebar_toggle);
    navigation_header.pack_start(&add_folder);
    let more_button = gtk::MenuButton::new();
    more_button.set_icon_name("view-more-symbolic");
    more_button.set_tooltip_text(Some("Meer"));
    let titlebar_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    titlebar_actions.append(&new_note);
    titlebar_actions.append(&more_button);
    navigation_header.pack_end(&titlebar_actions);
    install_app_menu(&window, &more_button, &state);
    let toolbar = ToolbarView::new();
    toolbar.set_content(Some(&layout));
    window.set_content(Some(&toolbar));
    let layout_for_map = navigation_layout.clone();
    window.connect_map(move |_| {
        layout_for_map.set_position(225);
    });

    connect_events(&state, &new_note);
    install_shortcuts(
        application,
        &window,
        &state,
        &new_note,
        &add_folder,
    );
    refresh_notes(&state)?;
    if let Some(note) = initial_note {
        show_note(&state, &note);
    }
    window.present();
    Ok(())
}

fn install_app_menu(
    window: &ApplicationWindow,
    more_button: &gtk::MenuButton,
    state: &Rc<WindowState>,
) {
    let menu = adw::gio::Menu::new();
    menu.append(Some("Instellingen"), Some("win.settings"));
    menu.append(Some("Over Memonotes"), Some("win.about"));
    more_button.set_menu_model(Some(&menu));

    let settings_action = adw::gio::SimpleAction::new("settings", None);
    let window_for_settings = window.clone();
    let state_for_settings = Rc::clone(state);
    settings_action.connect_activate(move |_, _| {
        let settings_dialog = adw::PreferencesDialog::builder()
            .title("Instellingen")
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
        for (index, &(color, label, _, _)) in NOTE_COLOR_PALETTE.iter().enumerate() {
            let button = gtk::ToggleButton::new();
            button.set_size_request(68, 64);
            button.set_hexpand(true);
            button.set_vexpand(true);
            button.add_css_class("note-color-option");
            button.set_tooltip_text(Some(label));
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
            let color_label = gtk::Label::new(Some(label));
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
            .title("Weergave")
            .icon_name("applications-graphics-symbolic")
            .build();
        let appearance_group = adw::PreferencesGroup::builder()
            .title("Notitiekleur")
            .build();
        appearance_group.add(&palette);
        appearance_page.add(&appearance_group);
        settings_dialog.add(&appearance_page);
        settings_dialog.present(Some(&window_for_settings));
    });
    window.add_action(&settings_action);

    let about_action = adw::gio::SimpleAction::new("about", None);
    let window_for_about = window.clone();
    about_action.connect_activate(move |_, _| {
        let dialog = adw::AboutDialog::builder()
            .application_name("Memonotes")
            .developer_name("Rob van Eck")
            .version(env!("CARGO_PKG_VERSION"))
            .support_url("https://github.com/sponsors/robvaneck")
            .issue_url("https://github.com/robvaneck/memonotes")
            .license_type(gtk::License::Gpl30)
            .build();
        dialog.add_credit_section(
            Some("Credits"),
            &[
                "Coded by Rob van Eck",
                "Bitcoin wallet: bc1sfsdfsdfsdfsdfsdfx4",
            ],
        );
        dialog.add_legal_section(
            "Legal",
            None,
            gtk::License::Gpl30,
            Some(include_str!("../LICENSE")),
        );
        dialog.present(Some(&window_for_about));
    });
    window.add_action(&about_action);
}

fn build_accounts_page(
    parent: &ApplicationWindow,
    state: &Rc<WindowState>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("network-server-symbolic")
        .build();
    let add_row = adw::ButtonRow::builder()
        .title("Account toevoegen")
        .start_icon_name("list-add-symbolic")
        .build();
    let accounts_group = adw::PreferencesGroup::builder()
        .title("Accounts")
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
        .title("Beveiliging")
        .build();
    let security_info = gtk::Label::new(Some(
        "Toegangstokens worden bewaard in GNOME Keyring, niet in de notitiedatabase.",
    ));
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
    for row in rows.rows.borrow_mut().drain(..).filter_map(|row| row.upgrade()) {
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
            status.set_text(&format!("Accounts laden mislukt: {error:#}"));
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
    let provider_name = match account.provider.as_str() {
        "memos" => "Memos",
        "imap" => "IMAP",
        "joplin" => "Joplin",
        _ => "Backend",
    };
    let row = adw::SwitchRow::builder()
        .title(format!("{} · {}", provider_name, account.label))
        .subtitle(&account.server_url)
        .subtitle_lines(1)
        .active(account.enabled)
        .build();

    let state_for_toggle = Rc::clone(state);
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
            status_for_toggle.set_text(&format!("Accountstatus wijzigen mislukt: {error:#}"));
            status_for_toggle.set_visible(true);
            restoring_for_toggle.set(true);
            row.set_active(!enabled);
            restoring_for_toggle.set(false);
        } else {
            status_for_toggle.set_visible(false);
        }
    });

    let details_button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Accountdetails bewerken")
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
        .tooltip_text("Account verwijderen")
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
        let alert = adw::AlertDialog::builder()
            .heading("Account verwijderen?")
            .body(format!(
                "{} en de opgeslagen PAT uit GNOME Keyring verwijderen? Notities op de server blijven staan.",
                account.label
            ))
            .build();
        alert.add_responses(&[("cancel", "Annuleren"), ("delete", "Verwijderen")]);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

        let state = Rc::clone(&state_for_remove);
        let rows = rows_for_remove.clone();
        let status = status_for_remove.clone();
        let account_for_confirmation = account.clone();
        alert.choose(Some(&parent), None::<&adw::gio::Cancellable>, move |response| {
            if response == "delete" {
                remove_backend_account(&state, &rows, &status, account_for_confirmation);
            }
        });
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
    let heading = if creating { "Account toevoegen" } else { "Accountdetails" };
    let subtitle = account.as_ref().map_or("Verbind jouw server", |account| {
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
        if creating { "Annuleren" } else { "Sluiten" },
        gtk::ResponseType::Cancel,
    );
    dialog.add_button(
        if creating { "Verbinden en toevoegen" } else { "Bewerken" },
        gtk::ResponseType::Accept,
    );
    dialog.set_default_response(gtk::ResponseType::Accept);

    let account_state = Rc::new(RefCell::new(account.clone()));
    let editing = Rc::new(Cell::new(creating));
    let providers = gtk::StringList::new(&["Memos", "IMAP", "Joplin"]);
    let provider_index = account.as_ref().map_or(0, |account| match account.provider.as_str() {
        "imap" => 1,
        "joplin" => 2,
        _ => 0,
    });
    let provider_row = adw::ComboRow::builder()
        .title("Account type")
        .model(&providers)
        .selected(provider_index)
        .sensitive(creating)
        .build();
    let account_name_row = adw::EntryRow::builder()
        .title("Accountnaam")
        .text(account.as_ref().map_or("", |account| account.label.as_str()))
        .build();
    let server_row = adw::EntryRow::builder()
        .title("Serveradres")
        .text(account.as_ref().map_or("", |account| account.server_url.as_str()))
        .build();
    let token_row = adw::PasswordEntryRow::builder()
        .title(if creating { "Personal Access Token" } else { "Vervang PAT (optioneel)" })
        .build();
    let token_info = adw::ActionRow::builder()
        .title("Toegangstoken")
        .subtitle("Veilig opgeslagen in GNOME Keyring")
        .build();
    token_info.set_visible(!creating);
    token_row.set_visible(creating);
    let advanced_row = adw::ExpanderRow::builder()
        .title("Geavanceerde verbinding")
        .expanded(false)
        .build();
    let allow_http_row = adw::SwitchRow::builder()
        .title("Onversleuteld HTTP toestaan")
        .subtitle("Alleen voor een vertrouwd lokaal netwerk.")
        .active(account.as_ref().is_some_and(|account| account.server_url.starts_with("http://")))
        .build();
    advanced_row.add_row(&allow_http_row);
    let error_banner = adw::Banner::builder().revealed(false).build();
    error_banner.add_css_class("account-error-banner");

    let form_group = adw::PreferencesGroup::builder().title("Verbinding").build();
    form_group.set_margin_top(24);
    form_group.set_margin_bottom(24);
    form_group.set_margin_start(24);
    form_group.set_margin_end(24);
    form_group.add(&provider_row);
    form_group.add(&account_name_row);
    form_group.add(&server_row);
    form_group.add(&token_info);
    form_group.add(&token_row);
    form_group.add(&advanced_row);
    dialog.content_area().append(&error_banner);
    dialog.content_area().append(&form_group);

    set_account_editor_editable(
        &dialog,
        &account_state.borrow(),
        editing.get(),
        &provider_row,
        &account_name_row,
        &server_row,
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
                &token_row,
                &token_info,
                &allow_http_row,
            );
            set_dialog_response_label(dialog, "Opslaan");
            dialog.set_title(Some("Account bewerken"));
            title_for_dialog.set_title("Account bewerken");
            if let Some(account) = state_for_dialog.borrow().as_ref() {
                title_for_dialog.set_subtitle(&account.server_url);
            }
            return;
        }

        if provider_row.selected() != 0 {
            error_banner.set_title("Deze backend is nog niet beschikbaar.");
            error_banner.set_revealed(true);
            return;
        }
        let label = account_name_row.text().trim().to_owned();
        let server_url = server_row.text().trim().to_owned();
        let new_token = token_row.text().to_string();
        if label.is_empty() || server_url.is_empty() || (state_for_dialog.borrow().is_none() && new_token.trim().is_empty()) {
            error_banner.set_title("Vul accountnaam, serveradres en PAT in.");
            error_banner.set_revealed(true);
            return;
        }

        let current_account = state_for_dialog.borrow().clone();
        let account_id = current_account.as_ref().map_or_else(|| Uuid::new_v4().to_string(), |account| account.id.clone());
        let enabled = current_account.as_ref().is_none_or(|account| account.enabled);
        let allow_http = allow_http_row.is_active();
        error_banner.set_revealed(false);
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
                let previous_token = if existing_for_worker.is_some() && !new_token_for_worker.is_empty() {
                    Some(memonotes::credentials::lookup_token(&account_id_for_worker)?)
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
                let client = memonotes::memos::MemosClient::new(
                    &server_for_worker,
                    effective_token.as_deref().unwrap_or("offline-account-without-token"),
                )?;
                if client.uses_plain_http() && !allow_http {
                    anyhow::bail!("HTTP is onversleuteld; bevestig dit expliciet in Geavanceerde verbinding");
                }
                if enabled {
                    let token = effective_token
                        .as_deref()
                        .context("Er is geen PAT in GNOME Keyring; vul een nieuwe PAT in")?;
                    memonotes::memos::MemosClient::new(&server_for_worker, token)?.check_connection()?;
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
        let account_id_for_save = account_id.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            match receiver.try_recv() {
                Ok(worker_result) => {
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
                                state_for_save.store.borrow().add_backend_account_with_state(
                                    &account_id_for_save,
                                    &label,
                                    "memos",
                                    &server_url,
                                    true,
                                )
                            };
                            match save_result {
                                Ok(()) => {
                                    refresh_backend_accounts(&rows_for_save, &state_for_save, &list_status_for_save);
                                    dialog_for_save.close();
                                }
                                Err(error) => {
                                    rollback_account_token(
                                        &account_id_for_save,
                                        current_account.is_none(),
                                        previous_token,
                                        &label,
                                    );
                                    error_banner_for_save.set_title(&format!("Account opslaan mislukt: {error:#}"));
                                    error_banner_for_save.set_revealed(true);
                                }
                            }
                        }
                        Err(error) => {
                            error_banner_for_save.set_title(&format!("Accountverbinding mislukt: {error:#}"));
                            error_banner_for_save.set_revealed(true);
                        }
                    }
                    if let Some(button) = dialog_for_save.widget_for_response(gtk::ResponseType::Accept) {
                        button.set_sensitive(true);
                    }
                    if let Some(button) = dialog_for_save.widget_for_response(gtk::ResponseType::Cancel) {
                        button.set_sensitive(true);
                    }
                    gtk::glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    error_banner_for_save.set_title("Account opslaan is onverwacht gestopt.");
                    error_banner_for_save.set_revealed(true);
                    gtk::glib::ControlFlow::Break
                }
            }
        });
    });
    dialog.present();
}

fn set_account_editor_editable(
    dialog: &gtk::Dialog,
    account: &Option<BackendAccount>,
    editable: bool,
    provider_row: &adw::ComboRow,
    account_name_row: &adw::EntryRow,
    server_row: &adw::EntryRow,
    token_row: &adw::PasswordEntryRow,
    token_info: &adw::ActionRow,
    allow_http_row: &adw::SwitchRow,
) {
    provider_row.set_sensitive(editable && account.is_none());
    account_name_row.set_editable(editable);
    server_row.set_editable(editable);
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
    status.set_text("Account en sleutel uit de keyring verwijderen…");
    let (sender, receiver) = mpsc::channel();
    let account_id = account.id.clone();
    thread::spawn(move || {
        let _ = sender.send(memonotes::credentials::delete_token(&account_id));
    });

    let state = Rc::clone(state);
    let rows = rows.clone();
    let status = status.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        match receiver.try_recv() {
            Ok(Ok(())) => match state.store.borrow().remove_backend_account(&account.id) {
                Ok(()) => {
                    refresh_backend_accounts(&rows, &state, &status);
                    gtk::glib::ControlFlow::Break
                }
                Err(error) => {
                    status.set_text(&format!("Account verwijderen mislukt: {error:#}"));
                    gtk::glib::ControlFlow::Break
                }
            },
            Ok(Err(error)) => {
                status.set_text(&format!("Key uit de GNOME-keyring verwijderen mislukt: {error:#}"));
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                status.set_text("Account verwijderen is onverwacht gestopt.");
                gtk::glib::ControlFlow::Break
            }
        }
    });
}

fn animate_sidebar_position(
    layout: &gtk::Paned,
    target: i32,
    generation: u64,
    current_generation: Rc<Cell<u64>>,
) {
    let start = layout.position();
    let started = Instant::now();
    let duration = Duration::from_millis(240);
    let layout = layout.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(16), move || {
        if current_generation.get() != generation {
            return gtk::glib::ControlFlow::Break;
        }
        let progress = (started.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let position = start as f64 + (target - start) as f64 * eased;
        layout.set_position(position.round() as i32);
        if progress >= 1.0 {
            layout.set_position(target);
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
    let items = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    items.set_activate_on_single_click(true);
    append_sidebar_item(&items, state, "Alle notities", "all");
    append_sidebar_item(&items, state, "Zonder map", "unfiled");
    append_sidebar_item(&items, state, "Archief", "archive");
    for (index, folder) in state.store.borrow().folders()?.into_iter().enumerate() {
        append_folder_row(
            &items,
            state,
            window,
            &folder.id,
            &folder.name,
            folder.count,
            index as i32 + 3,
        );
    }

    let state_for_create = Rc::clone(state);
    let items_for_create = items.clone();
    let parent_window = window.clone();
    add_folder.connect_clicked(move |_| {
        let dialog = gtk::Dialog::builder()
            .title("Nieuwe folder")
            .modal(true)
            .transient_for(&parent_window)
            .build();
        dialog.add_button("Annuleren", gtk::ResponseType::Cancel);
        dialog.add_button("Aanmaken", gtk::ResponseType::Accept);

        let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
        form.set_vexpand(true);
        form.set_margin_top(16);
        form.set_margin_bottom(16);
        form.set_margin_start(16);
        form.set_margin_end(16);
        let name = gtk::Entry::builder()
            .placeholder_text("Foldernaam")
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
        let items_for_response = items_for_create.clone();
        let parent_window_for_response = parent_window.clone();
        let name_for_response = name.clone();
        let error_for_response = error.clone();
        dialog.connect_response(move |dialog, response| {
            if response != gtk::ResponseType::Accept {
                dialog.close();
                return;
            }

            let folder_name = name_for_response.text();
            let create_result = {
                let store = state_for_response.store.borrow();
                store.create_folder(folder_name.as_str()).and_then(|folder_id| {
                    store.folders().map(|folders| (folder_id, folders))
                })
            };
            match create_result {
                Ok((folder_id, folders)) => {
                    let folder_index = folders
                        .iter()
                        .position(|folder| folder.id == folder_id)
                        .unwrap_or(folders.len());
                    let folder_row = append_folder_row(
                        &items_for_response,
                        &state_for_response,
                        &parent_window_for_response,
                        &folder_id,
                        folder_name.trim(),
                        0,
                        folder_index as i32 + 3,
                    );
                    items_for_response.select_row(Some(&folder_row));
                    state_for_response
                        .folder_picker
                        .model()
                        .and_downcast::<gtk::StringList>()
                        .expect("folder dropdown uses a GtkStringList")
                        .splice(folder_index as u32 + 1, 0, &[folder_name.trim()]);
                    state_for_response.folder_options.borrow_mut().insert(
                        folder_index + 1,
                        Some(folder_id.clone()),
                    );
                    *state_for_response.active_filter.borrow_mut() =
                        ActiveNoteFilter::Folder(folder_id);
                    *state_for_response.note_id.borrow_mut() = None;
                    set_editor_text(&state_for_response, "", "");
                    let _ = refresh_notes(&state_for_response);
                    dialog.close();
                }
                Err(_) => {
                    error_for_response.set_text("Voer een foldernaam in.");
                    error_for_response.set_visible(true);
                }
            }
        });
        dialog.present();
    });

    let state_for_selection = Rc::clone(state);
    items.connect_row_activated(move |_, row| {
        let item = row.widget_name();
        match item.as_str() {
            "all" => {
                *state_for_selection.active_filter.borrow_mut() = ActiveNoteFilter::All;
            }
            "unfiled" => {
                *state_for_selection.active_filter.borrow_mut() = ActiveNoteFilter::Unfiled;
            }
            "archive" => {
                *state_for_selection.active_filter.borrow_mut() = ActiveNoteFilter::Archive;
            }
            "heading" => return,
            _ => {
                if let Some(folder_id) = item.strip_prefix("folder:") {
                    *state_for_selection.active_filter.borrow_mut() =
                        ActiveNoteFilter::Folder(folder_id.to_owned());
                }
            }
        }
        *state_for_selection.note_id.borrow_mut() = None;
        state_for_selection.pin_button.set_sensitive(false);
        state_for_selection.archive_button.set_sensitive(false);
        state_for_selection.folder_button.set_sensitive(false);
        state_for_selection.folder_picker.set_sensitive(false);
        update_archive_button(
            &state_for_selection.archive_button,
            state_for_selection.active_filter.borrow().is_archived(),
        );
        set_editor_text(&state_for_selection, "", "");
        let _ = refresh_notes(&state_for_selection);
    });

    let sidebar = gtk::ScrolledWindow::builder().child(&items).build();
    sidebar.add_css_class("memonotes-sidebar");
    items.add_css_class("memonotes-sidebar");
    Ok(sidebar)
}

fn append_sidebar_item(list: &gtk::ListBox, state: &Rc<WindowState>, label: &str, id: &str) {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(id);
    let count_label = folder_count_label(0);
    state
        .folder_count_labels
        .borrow_mut()
        .push((id.to_owned(), count_label.clone()));
    row.set_child(Some(&folder_row_content(label, &count_label)));
    list.append(&row);
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
    id: &str,
    name: &str,
    count: i64,
    position: i32,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&format!("folder:{id}"));
    let count_label = folder_count_label(count);
    state
        .folder_count_labels
        .borrow_mut()
        .push((id.to_owned(), count_label.clone()));
    row.set_child(Some(&folder_row_content(name, &count_label)));
    let drop_target = gtk::DropTarget::new(
        String::static_type(),
        gtk::gdk::DragAction::MOVE,
    );
    let state_for_drop = Rc::clone(state);
    let folder_id_for_drop = id.to_owned();
    drop_target.connect_drop(move |_, value, _, _| {
        let Ok(note_ids) = value.get::<String>() else {
            return false;
        };
        let note_ids: Vec<String> = note_ids.lines().map(str::to_owned).collect();
        move_notes_to_folder(&state_for_drop, &note_ids, &folder_id_for_drop)
    });
    row.add_controller(drop_target);
    list.insert(&row, position);

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 3);
    menu.add_css_class("folder-context-menu");

    let rename_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    rename_list.set_activate_on_single_click(true);
    let rename_item = folder_menu_item("document-edit-symbolic", "Hernoem map", "rename", false);
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
    let delete_item = folder_menu_item("user-trash-symbolic", "Verwijder map", "delete", true);
    delete_list.append(&delete_item);
    menu.append(&delete_list);

    popover.set_child(Some(&menu));
    popover.set_parent(&row);

    let state_for_rename = Rc::clone(state);
    let row_for_rename = row.clone();
    let list_for_menu = list.clone();
    let window_for_menu = window.clone();
    let folder_id = id.to_owned();
    let old_name = name.to_owned();
    let count_label = count_label.clone();
    let state_for_delete = Rc::clone(&state_for_rename);
    let row_for_delete = row_for_rename.clone();
    let list_for_delete = list_for_menu.clone();
    let window_for_delete = window_for_menu.clone();
    let folder_id_for_delete = folder_id.clone();
    let folder_name_for_delete = old_name.clone();
    let popover_for_rename = popover.clone();
    rename_list.connect_row_activated(move |_, _| {
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

    let popover_for_delete = popover.clone();
    delete_list.connect_row_activated(move |_, _| {
        popover_for_delete.popdown();
        confirm_delete_folder(
            &state_for_delete,
            &list_for_delete,
            &window_for_delete,
            &row_for_delete,
            &folder_id_for_delete,
            &folder_name_for_delete,
        );
    });

    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    let popover_for_click = popover.clone();
    right_click.connect_pressed(move |_, _, x, y| {
        popover_for_click
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover_for_click.popup();
    });
    row.add_controller(right_click);
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

fn install_account_error_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "window.account-editor .dialog-action-box { \
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
}

fn install_note_detail_style() {
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
    style_manager.connect_dark_notify(move |style_manager| {
        provider_for_theme.load_from_data(&note_detail_css(style_manager.is_dark()));
    });
}

fn note_detail_css(dark: bool) -> String {
    let default = note_color_value("default", dark);
    let mut css = format!(
        ".note-detail-surface {{ background-color: {default}; background-image: none; }} \
         .note-detail-pane {{ background-image: linear-gradient(to right, alpha(black, 0.05), transparent 8px); }} \
         .note-color-option {{ padding: 5px; }} \
         .note-color-swatch {{ border-radius: 50px; border: 1px solid alpha(@theme_fg_color, 0.18); }} "
    );
    for &(color, _, _, _) in &NOTE_COLOR_PALETTE {
        let value = note_color_value(color, dark);
        if color != "default" {
            css.push_str(&format!(
                ".note-detail-surface.note-color-{color} {{ background-color: {value}; }} "
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
    list: &gtk::ListBox,
    window: &ApplicationWindow,
    row: &gtk::ListBoxRow,
    folder_id: &str,
    folder_name: &str,
) {
    let dialog = gtk::Dialog::builder()
        .title("Verwijder map")
        .modal(true)
        .transient_for(window)
        .build();
    dialog.add_button("Annuleren", gtk::ResponseType::Cancel);
    let delete_button = dialog.add_button("Verwijderen", gtk::ResponseType::Accept);
    delete_button.add_css_class("destructive-action");

    let message = gtk::Label::builder()
        .label(format!(
            "Map ‘{folder_name}’ verwijderen? De notities blijven behouden en worden verplaatst naar Zonder map."
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    message.set_margin_top(16);
    message.set_margin_bottom(16);
    message.set_margin_start(16);
    message.set_margin_end(16);
    dialog.content_area().append(&message);

    let state_for_response = Rc::clone(state);
    let list_for_response = list.clone();
    let row_for_response = row.clone();
    let folder_id = folder_id.to_owned();
    dialog.connect_response(move |dialog, response| {
        if response != gtk::ResponseType::Accept {
            dialog.close();
            return;
        }

        let selected_note_id = state_for_response.note_id.borrow().clone();
        let deleted = state_for_response
            .store
            .borrow_mut()
            .delete_folder(&folder_id);
        if deleted.is_ok() {
            state_for_response
                .suppress_folder_picker_events
                .set(true);
            list_for_response.remove(&row_for_response);
            state_for_response
                .folder_count_labels
                .borrow_mut()
                .retain(|(id, _)| id != &folder_id);

            let folder_option_index = {
                state_for_response
                    .folder_options
                    .borrow()
                    .iter()
                    .position(|id| id.as_deref() == Some(folder_id.as_str()))
            };
            if let Some(index) = folder_option_index {
                state_for_response.folder_options.borrow_mut().remove(index);
                state_for_response
                    .folder_picker
                    .model()
                    .and_downcast::<gtk::StringList>()
                    .expect("folder dropdown uses a GtkStringList")
                    .splice(index as u32, 1, &[]);
            }

            let deleted_folder_is_active = {
                *state_for_response.active_filter.borrow()
                    == ActiveNoteFilter::Folder(folder_id.clone())
            };
            if deleted_folder_is_active {
                *state_for_response.active_filter.borrow_mut() = ActiveNoteFilter::Unfiled;
            }
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
            state_for_response
                .suppress_folder_picker_events
                .set(false);
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
            if let Some(index) = state_for_save
                .folder_options
                .borrow()
                .iter()
                .position(|id| id.as_deref() == Some(&folder_id_for_save))
            {
                state_for_save
                    .folder_picker
                    .model()
                    .and_downcast::<gtk::StringList>()
                    .expect("folder dropdown uses a GtkStringList")
                    .splice(index as u32, 1, &[new_name.trim()]);
            }
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
    undo_action.connect_activate(move |_, _| undo_last_operation(&state_for_undo));
    window.add_action(&undo_action);
    application.set_accels_for_action("win.undo", &["<Control>z"]);

    let search_keys = gtk::EventControllerKey::new();
    let search_for_escape = state.search.clone();
    search_keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            search_for_escape.set_text("");
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

fn connect_events(state: &Rc<WindowState>, new_note: &gtk::Button) {
    let state_for_activation = Rc::clone(state);
    state.note_list.connect_row_activated(move |_, row| {
        let Some(application) = row
            .root()
            .and_downcast::<ApplicationWindow>()
            .and_then(|window| window.application())
        else {
            return;
        };
        let note_id = row.widget_name();
        let note = { state_for_activation.store.borrow().note(&note_id).ok().flatten() };
        if let Some(note) = note {
            open_note_window(&application, &state_for_activation, &note);
            clear_note_selection(&state_for_activation);
            let state_for_clear = Rc::clone(&state_for_activation);
            gtk::glib::idle_add_local_once(move || {
                clear_note_selection(&state_for_clear);
            });
        }
    });

    let state_for_search = Rc::clone(state);
    state.search.connect_search_changed(move |_| {
        let _ = refresh_notes(&state_for_search);
    });

    let delete_keys = gtk::EventControllerKey::new();
    delete_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state_for_delete = Rc::clone(state);
    delete_keys.connect_key_pressed(move |_, key, _, modifiers| match key {
        gtk::gdk::Key::Delete => {
            if state_for_delete.active_filter.borrow().is_archived() {
                delete_current_note(&state_for_delete);
            } else {
                archive_current_note(&state_for_delete);
            }
            gtk::glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => move_note_selection(&state_for_delete, -1),
        gtk::gdk::Key::Down => move_note_selection(&state_for_delete, 1),
        gtk::gdk::Key::Tab if !modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) => {
            state_for_delete.title.grab_focus();
            gtk::glib::Propagation::Stop
        }
        _ => gtk::glib::Propagation::Proceed,
    });
    state.note_list_focus.add_controller(delete_keys);
    connect_tab_navigation(
        &state.title,
        state.content.clone().upcast(),
        state.note_list_focus.clone().upcast(),
    );
    connect_tab_navigation(
        &state.content,
        state.pin_button.clone().upcast(),
        state.title.clone().upcast(),
    );
    connect_tab_navigation(
        &state.pin_button,
        state.archive_button.clone().upcast(),
        state.content.clone().upcast(),
    );
    connect_tab_navigation(
        &state.archive_button,
        state.folder_button.clone().upcast(),
        state.pin_button.clone().upcast(),
    );
    connect_tab_navigation(
        &state.folder_button,
        state.search.clone().upcast(),
        state.archive_button.clone().upcast(),
    );

    let state_for_new = Rc::clone(state);
    new_note.connect_clicked(move |_| {
        if state_for_new.active_filter.borrow().is_archived() {
            return;
        }
        *state_for_new.note_id.borrow_mut() = None;
        state_for_new.note_list.unselect_all();
        set_editor_text(&state_for_new, "", "");
        set_note_surfaces_color(
            &state_for_new.note_surfaces.borrow(),
            &state_for_new.note_color.borrow(),
        );
        state_for_new.pin_button.set_sensitive(false);
        state_for_new.archive_button.set_sensitive(false);
        state_for_new.folder_button.set_sensitive(true);
        state_for_new.folder_picker.set_sensitive(true);

        let target_folder = state_for_new.active_filter.borrow().folder_id();
        let target_index = state_for_new
            .folder_options
            .borrow()
            .iter()
            .position(|option| option.as_deref() == target_folder.as_deref());
        if let Some(index) = target_index {
            state_for_new.folder_picker.set_selected(index as u32);
        }
        state_for_new.title.grab_focus();
        state_for_new.title.set_position(0);
    });

    let state_for_title = Rc::clone(state);
    state.title.connect_changed(move |entry| {
        save_current(&state_for_title, entry.text().as_str(), None);
    });

    let state_for_content = Rc::clone(state);
    state.content.buffer().connect_changed(move |buffer| {
        let (start, end) = buffer.bounds();
        let body = buffer.text(&start, &end, true);
        let title = state_for_content.title.text();
        save_current(&state_for_content, title.as_str(), Some(body.as_str()));
    });

    let state_for_pin = Rc::clone(state);
    state.pin_button.connect_clicked(move |_| {
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

    let state_for_archive = Rc::clone(state);
    state.archive_button.connect_clicked(move |_| {
        let archived = !state_for_archive.active_filter.borrow().is_archived();
        set_current_note_archived(&state_for_archive, archived);
    });

    let state_for_move = Rc::clone(state);
    state.folder_picker.connect_selected_notify(move |picker| {
        if state_for_move.suppress_folder_picker_events.get() {
            return;
        }
        state_for_move.folder_popover.popdown();
        if state_for_move.active_filter.borrow().is_archived() {
            return;
        }
        let folder_id = state_for_move
            .folder_options
            .borrow()
            .get(picker.selected() as usize)
            .cloned();
        let Some(folder_id) = folder_id else {
            return;
        };
        let Some(note_id) = state_for_move.note_id.borrow().clone() else {
            *state_for_move.active_filter.borrow_mut() =
                ActiveNoteFilter::for_folder_picker(folder_id);
            return;
        };
        let current_folder = state_for_move
            .store
            .borrow()
            .note(&note_id)
            .ok()
            .flatten()
            .and_then(|note| note.folder_id);
        if current_folder.as_deref() == folder_id.as_deref() {
            return;
        }
        if state_for_move
            .store
            .borrow()
            .move_note(&note_id, folder_id.as_deref())
            .is_ok()
        {
            let _ = refresh_notes(&state_for_move);
        }
    });
}

fn connect_tab_navigation(
    widget: &impl gtk::prelude::IsA<gtk::Widget>,
    next: gtk::Widget,
    previous: gtk::Widget,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if key != gtk::gdk::Key::Tab {
            return gtk::glib::Propagation::Proceed;
        }
        let target = if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
            &previous
        } else {
            &next
        };
        target.grab_focus();
        gtk::glib::Propagation::Stop
    });
    widget.add_controller(keys);
}

fn refresh_notes(state: &Rc<WindowState>) -> Result<()> {
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

    let desired_ids: HashSet<_> = notes.iter().map(|note| note.id.as_str()).collect();
    let mut rows = state.note_rows.borrow_mut();
    let last_pinned_position = notes.iter().rposition(|note| note.pinned);
    let divider_position = last_pinned_position.map(|position| position + 1);
    let current_divider_position = state
        .pinned_separator_row
        .parent()
        .map(|_| state.pinned_separator_row.index() as usize);
    if current_divider_position != divider_position && state.pinned_separator_row.parent().is_some()
    {
        state.note_list.remove(&state.pinned_separator_row);
    }
    let stale_ids: Vec<_> = rows
        .keys()
        .filter(|id| !desired_ids.contains(id.as_str()))
        .cloned()
        .collect();
    for id in stale_ids {
        if let Some(row) = rows.remove(&id) {
            state.note_list.remove(&row);
        }
    }

    for (position, note) in notes.iter().enumerate() {
        let row = if let Some(row) = rows.get(&note.id) {
            row.clone()
        } else {
            let row = build_note_row(state, note);
            rows.insert(note.id.clone(), row.clone());
            row
        };
        let folder_name = note
            .folder_id
            .as_ref()
            .and_then(|folder_id| folder_names.get(folder_id))
            .map(String::as_str)
            .unwrap_or("");
        update_note_row_widgets(&row, note, folder_name);
        let target_position =
            position + usize::from(divider_position.is_some_and(|divider| position >= divider));
        place_note_list_row(&state.note_list, &row, target_position);
    }
    if let Some(position) = divider_position {
        place_note_list_row(&state.note_list, &state.pinned_separator_row, position);
    }
    drop(rows);

    let selected_id = state.note_id.borrow().clone();
    let selected_row = selected_id
        .as_ref()
        .and_then(|id| state.note_rows.borrow().get(id).cloned());
    if let Some(row) = selected_row {
        state.note_list.select_row(Some(&row));
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

fn install_pinned_separator_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".pinned-divider-row { padding: 0; min-height: 0; } \
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
}

fn place_note_list_row(list: &gtk::ListBox, row: &gtk::ListBoxRow, position: usize) {
    if row.parent().is_some() && row.index() != position as i32 {
        list.remove(row);
    }
    if row.parent().is_none() {
        list.insert(row, position as i32);
    }
}

fn build_note_row(state: &Rc<WindowState>, note: &Note) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
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
        .label(note.content.lines().next().unwrap_or(""))
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
    row.set_child(Some(&item));

    let state_for_click = Rc::clone(state);
    let row_for_click = row.clone();
    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _, _, _| {
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
            .selected_rows()
            .iter()
            .any(|selected_row| selected_row == &row_for_click)
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
    let state_for_drag = Rc::clone(state);
    drag_source.connect_prepare(move |_, _, _| {
        let selected_ids: Vec<String> = state_for_drag
            .note_list
            .selected_rows()
            .into_iter()
            .map(|row| row.widget_name().to_string())
            .filter(|id| state_for_drag.note_rows.borrow().contains_key(id))
            .collect();
        let dragged_ids = if selected_ids.iter().any(|id| id == &note_id) {
            selected_ids
        } else {
            vec![note_id.clone()]
        };
        let value = dragged_ids.join("\n").to_value();
        Some(gtk::gdk::ContentProvider::for_value(&value))
    });
    let drag_icon_note_id = note.id.clone();
    let state_for_drag_icon = Rc::clone(state);
    drag_source.connect_drag_begin(move |_, drag| {
        let selected_ids: Vec<String> = state_for_drag_icon
            .note_list
            .selected_rows()
            .into_iter()
            .map(|row| row.widget_name().to_string())
            .filter(|id| state_for_drag_icon.note_rows.borrow().contains_key(id))
            .collect();
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

fn show_note_from_row(state: &Rc<WindowState>, row: &gtk::ListBoxRow) {
    state.note_list_focus.grab_focus();
    let note_id = row.widget_name();
    let note = { state.store.borrow().note(&note_id).ok().flatten() };
    if let Some(note) = note {
        show_note(state, &note);
    }
}

fn select_note_row(state: &Rc<WindowState>, row: &gtk::ListBoxRow) {
    state.note_list.unselect_all();
    state.note_list.select_row(Some(row));
    *state.selection_anchor.borrow_mut() = Some(row.widget_name().to_string());
    show_note_from_row(state, row);
}

fn select_note_range(state: &Rc<WindowState>, anchor_id: &str, target_id: &str) {
    let mut note_ids = Vec::new();
    let mut child = state.note_list.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk::ListBoxRow>() {
            let id = row.widget_name().to_string();
            if state.note_rows.borrow().contains_key(&id) {
                note_ids.push(id);
            }
        }
    }

    let Some(anchor_index) = note_ids.iter().position(|id| id == anchor_id) else {
        return;
    };
    let Some(target_index) = note_ids.iter().position(|id| id == target_id) else {
        return;
    };
    let start = anchor_index.min(target_index);
    let end = anchor_index.max(target_index);

    state.note_list.unselect_all();
    for id in &note_ids[start..=end] {
        if let Some(row) = state.note_rows.borrow().get(id) {
            state.note_list.select_row(Some(row));
        }
    }
}

fn move_note_selection(state: &Rc<WindowState>, direction: i32) -> gtk::glib::Propagation {
    let mut rows = Vec::new();
    let mut child = state.note_list.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk::ListBoxRow>() {
            if state
                .note_rows
                .borrow()
                .contains_key(row.widget_name().as_str())
            {
                rows.push(row);
            }
        }
    }
    if rows.is_empty() {
        return gtk::glib::Propagation::Stop;
    }

    let current_id = state.note_id.borrow().clone();
    let current_index = current_id
        .as_ref()
        .and_then(|id| rows.iter().position(|row| row.widget_name() == *id));
    let next_index = match current_index {
        Some(index) => (index as i32 + direction).clamp(0, rows.len() as i32 - 1) as usize,
        None if direction < 0 => rows.len() - 1,
        None => 0,
    };
    select_note_row(state, &rows[next_index]);
    gtk::glib::Propagation::Stop
}

fn refresh_folder_counts(state: &Rc<WindowState>) -> Result<()> {
    let store = state.store.borrow();
    let all_count = store.count_notes(&NoteFilter::default())?;
    let unfiled_count = store.count_notes(&NoteFilter {
        unfiled: true,
        ..Default::default()
    })?;
    let archive_count = store.count_notes(&NoteFilter {
        archived: true,
        ..Default::default()
    })?;
    let folders = store.folders()?;
    for (folder_id, label) in state.folder_count_labels.borrow().iter() {
        let count = match folder_id.as_str() {
            "all" => all_count,
            "unfiled" => unfiled_count,
            "archive" => archive_count,
            _ => folders
                .iter()
                .find(|folder| folder.id == *folder_id)
                .map(|folder| folder.count)
                .unwrap_or(0),
        };
        label.set_text(&count.to_string());
    }
    Ok(())
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

fn update_note_row_widgets(row: &gtk::ListBoxRow, note: &Note, folder_name: &str) {
    let Some(item) = row.child().and_downcast::<gtk::Box>() else {
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
            preview.set_text(note.content.lines().next().unwrap_or(""));
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
    update_archive_button(&state.archive_button, note.archived);
    let folder_index = {
        state
            .folder_options
            .borrow()
            .iter()
            .position(|folder_id| folder_id == &note.folder_id)
    };
    if let Some(index) = folder_index {
        state.folder_picker.set_selected(index as u32);
    }
    set_editor_text(state, &note.title, &note.content);
    set_note_surfaces_color(&state.note_surfaces.borrow(), &state.note_color.borrow());
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

    let title = gtk::Entry::builder()
        .text(&note.title)
        .placeholder_text("Nieuwe notities")
        .css_classes(["title-1", "note-detail-surface"])
        .build();
    let content = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(24)
        .left_margin(12)
        .right_margin(12)
        .build();
    content.add_css_class("note-detail-surface");
    content.buffer().set_text(&note.content);

    let editor = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor.add_css_class("note-detail-surface");
    editor.set_margin_top(24);
    editor.set_margin_bottom(24);
    editor.set_margin_start(32);
    editor.set_margin_end(32);
    editor.append(&title);
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
        }
        gtk::glib::Propagation::Proceed
    });

    let note_id = note.id.clone();
    let state_for_title = Rc::clone(state);
    let buffer_for_title = content.buffer();
    title.connect_changed(move |entry| {
        save_detached_note(
            &state_for_title,
            &note_id,
            entry.text().as_str(),
            &buffer_for_title,
        );
    });

    let note_id = note.id.clone();
    let state_for_content = Rc::clone(state);
    let title_for_content = title.clone();
    content.buffer().connect_changed(move |buffer| {
        save_detached_note(
            &state_for_content,
            &note_id,
            title_for_content.text().as_str(),
            buffer,
        );
    });

    state
        .note_windows
        .borrow_mut()
        .insert(note.id.clone(), window.clone());
    window.present();
    title.grab_focus();
}

fn save_detached_note(
    state: &Rc<WindowState>,
    note_id: &str,
    title: &str,
    buffer: &gtk::TextBuffer,
) {
    let (start, end) = buffer.bounds();
    let content = buffer.text(&start, &end, true);
    if state
        .store
        .borrow()
        .save_note(note_id, content.as_str(), title)
        .is_ok()
    {
        let _ = refresh_notes(state);
    }
}

fn save_current(state: &Rc<WindowState>, title: &str, content: Option<&str>) {
    if state.suppress_editor_events.get() {
        return;
    }
    let body = content.map(str::to_owned).unwrap_or_else(|| {
        let buffer = state.content.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, true).to_string()
    });

    let (note_id, created_now) = if let Some(note_id) = state.note_id.borrow().clone() {
        (note_id, false)
    } else {
        if state.active_filter.borrow().is_archived()
            || (title.trim().is_empty() && body.trim().is_empty())
        {
            return;
        }
        let folder_id = state.active_filter.borrow().folder_id();
        let create_result = { state.store.borrow().create_note(folder_id.as_deref()) };
        let Ok(note_id) = create_result else {
            return;
        };
        *state.note_id.borrow_mut() = Some(note_id.clone());
        (note_id, true)
    };

    let current = state.store.borrow().note(&note_id).ok().flatten();
    if let Some(current) = current {
        if current.title == title.trim() && current.content == body {
            return;
        }
        if state
            .store
            .borrow()
            .save_note(&note_id, &body, title)
            .is_ok()
        {
            if created_now {
                state.pin_button.set_sensitive(true);
                set_icon_button(&state.pin_button, "view-pin-symbolic", "Vastpinnen");
                state.archive_button.set_sensitive(true);
                update_archive_button(&state.archive_button, false);
                state.folder_button.set_sensitive(true);
                state.folder_picker.set_sensitive(true);
                let note_folder = state
                    .store
                    .borrow()
                    .note(&note_id)
                    .ok()
                    .flatten()
                    .and_then(|note| note.folder_id);
                if let Some(index) = state
                    .folder_options
                    .borrow()
                    .iter()
                    .position(|folder_id| folder_id == &note_folder)
                {
                    state.folder_picker.set_selected(index as u32);
                }
            }
            let _ = refresh_notes(state);
        }
    }
}

fn set_editor_text(state: &WindowState, title: &str, content: &str) {
    state.suppress_editor_events.set(true);
    state.title.set_text(title);
    state.content.buffer().set_text(content);
    state.suppress_editor_events.set(false);
}

fn clear_note_selection(state: &WindowState) {
    *state.note_id.borrow_mut() = None;
    state.note_list.unselect_all();
    state.pin_button.set_sensitive(false);
    state.archive_button.set_sensitive(false);
    state.folder_button.set_sensitive(false);
    state.folder_picker.set_sensitive(false);
    set_editor_text(state, "", "");
}

fn archive_current_note(state: &Rc<WindowState>) {
    set_current_note_archived(state, true);
}

fn delete_current_note(state: &Rc<WindowState>) {
    let Some(note_id) = state.note_id.borrow().clone() else {
        return;
    };
    let note = state.store.borrow().note(&note_id).ok().flatten();
    let Some(note) = note else {
        return;
    };
    if state.store.borrow().delete_note(&note_id).is_ok() {
        state
            .undo_stack
            .borrow_mut()
            .push(UndoOperation::Delete(note));
        clear_note_selection(state);
        let _ = refresh_notes(state);
    }
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
            UndoOperation::Delete(note) => store.restore_note(note),
        }
    };
    if undo_result.is_err() {
        return;
    }
    state.undo_stack.borrow_mut().pop();

    let note_id = match operation {
        UndoOperation::Archive(note) | UndoOperation::Delete(note) => note.id,
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

fn database_path() -> Result<PathBuf> {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("XDG_DATA_HOME of HOME is niet ingesteld")?
        .join("memonotes");
    std::fs::create_dir_all(&data_dir).context("Appgegevensmap aanmaken")?;
    Ok(data_dir.join("notes.sqlite3"))
}

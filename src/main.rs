use adw::gtk;
use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, ToolbarView};
use anyhow::{Context, Result};
use libadwaita as adw;
use memonotes::{Note, NoteFilter, Store};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

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
    undo_stack: RefCell<Vec<UndoOperation>>,
    active_filter: RefCell<ActiveNoteFilter>,
    note_id: RefCell<Option<String>>,
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
    selection_anchor: RefCell<Option<String>>,
    suppress_folder_picker_events: Cell<bool>,
    suppress_editor_events: Cell<bool>,
}

fn main() -> Result<()> {
    let application = Application::builder()
        .application_id("app.memonotes.Desktop")
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
    let store = Store::open(database_path()?).context("Lokale notities openen")?;
    let folders = store.folders()?;
    let initial_notes = store.notes(&NoteFilter::default())?;
    let initial_note = initial_notes.first().cloned();
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
    let list_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&note_list)
        .build();
    list_scroll.set_focusable(true);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Zoek in notities")
        .build();
    let title = gtk::Entry::builder()
        .placeholder_text("Nieuwe notities")
        .css_classes(["title-1"])
        .build();
    let content = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(24)
        .left_margin(12)
        .right_margin(12)
        .build();
    let pin_button = icon_button("view-pin-symbolic", "Vastpinnen");
    let archive_button = icon_button("mail-archive-symbolic", "Archiveer");
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
        undo_stack: RefCell::new(Vec::new()),
        active_filter: RefCell::new(ActiveNoteFilter::All),
        note_id: RefCell::new(initial_note.as_ref().map(|note| note.id.clone())),
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
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    search_row.append(&search);
    search.set_hexpand(true);
    let new_note = icon_button("document-new-symbolic", "Nieuwe notitie");
    search_row.append(&new_note);
    list_column.append(&search_row);
    list_column.append(&list_scroll);

    let editor = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor.set_margin_top(24);
    editor.set_margin_bottom(24);
    editor.set_margin_start(32);
    editor.set_margin_end(32);
    editor.append(&title);
    let editor_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&content)
        .build();
    editor.append(&editor_scroll);

    let content_pane = gtk::Paned::new(gtk::Orientation::Horizontal);
    content_pane.set_wide_handle(true);
    content_pane.set_hexpand(true);
    content_pane.set_start_child(Some(&list_column));
    content_pane.set_end_child(Some(&editor));
    content_pane.set_position(310);

    let layout = gtk::Paned::new(gtk::Orientation::Horizontal);
    layout.add_css_class("sidebar-layout");
    layout.set_wide_handle(true);
    layout.set_hexpand(true);
    layout.set_start_child(Some(&sidebar));
    layout.set_end_child(Some(&content_pane));
    let sidebar_width = Rc::new(Cell::new(225));
    layout.set_position(sidebar_width.get());

    let sidebar_icon = gtk::Image::from_icon_name("sidebar-show-symbolic");
    let sidebar_toggle = gtk::ToggleButton::new();
    sidebar_toggle.set_child(Some(&sidebar_icon));
    sidebar_toggle.set_active(true);
    sidebar_toggle.set_tooltip_text(Some("Zijbalk tonen of verbergen"));
    let layout_for_toggle = layout.clone();
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

    let toolbar = ToolbarView::new();
    let header = HeaderBar::new();
    header.pack_start(&sidebar_toggle);
    header.pack_start(&add_folder);
    header.set_title_widget(Some(&gtk::Label::new(Some("Memonotes"))));
    header.pack_end(&pin_button);
    header.pack_end(&archive_button);
    header.pack_end(&folder_button);
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&layout));
    window.set_content(Some(&toolbar));
    let layout_for_map = layout.clone();
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
    Ok(())
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
            "mail-archive-symbolic"
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
    application.set_accels_for_action("app.quit", &["<Control>q", "<Control>w"]);

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

fn connect_events(state: &Rc<WindowState>, new_note: &gtk::Button) {
    let state_for_search = Rc::clone(state);
    state.search.connect_search_changed(move |_| {
        let _ = refresh_notes(&state_for_search);
    });

    let delete_keys = gtk::EventControllerKey::new();
    delete_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state_for_delete = Rc::clone(state);
    delete_keys.connect_key_pressed(move |_, key, _, _| match key {
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
        _ => gtk::glib::Propagation::Proceed,
    });
    state.note_list_focus.add_controller(delete_keys);

    let state_for_new = Rc::clone(state);
    new_note.connect_clicked(move |_| {
        if state_for_new.active_filter.borrow().is_archived() {
            return;
        }
        *state_for_new.note_id.borrow_mut() = None;
        state_for_new.note_list.unselect_all();
        set_editor_text(&state_for_new, "", "");
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

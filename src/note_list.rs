use adw::gtk;
use adw::prelude::*;
use libadwaita as adw;
use memonotes::Note;

#[derive(Clone)]
pub struct NoteListEntry {
    pub note: Note,
    pub folder_name: String,
    pub divider: bool,
}

impl NoteListEntry {
    fn matches(&self, other: &Self) -> bool {
        self.note.id == other.note.id
            && self.note.account_id == other.note.account_id
            && self.note.folder_id == other.note.folder_id
            && self.note.content == other.note.content
            && self.note.title == other.note.title
            && self.note.updated_at == other.note.updated_at
            && self.note.pinned == other.note.pinned
            && self.note.archived == other.note.archived
            && self.folder_name == other.folder_name
            && self.divider == other.divider
    }
}

pub struct NoteList {
    pub view: gtk::ListView,
    pub factory: gtk::SignalListItemFactory,
    model: adw::gio::ListStore,
    selection: gtk::MultiSelection,
}

impl NoteList {
    pub fn new() -> Self {
        let model = adw::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
        let selection = gtk::MultiSelection::new(Some(model.clone()));
        let factory = gtk::SignalListItemFactory::new();
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        view.add_css_class("navigation-sidebar");
        view.add_css_class("native-note-list");
        view.set_single_click_activate(false);
        Self {
            view,
            factory,
            model,
            selection,
        }
    }

    pub fn entry(&self, position: u32) -> Option<NoteListEntry> {
        let object = self
            .model
            .item(position)?
            .downcast::<gtk::glib::BoxedAnyObject>()
            .ok()?;
        Some(object.borrow::<NoteListEntry>().clone())
    }

    pub fn position(&self, id: &str) -> Option<u32> {
        (0..self.model.n_items()).find(|position| {
            let object = self
                .model
                .item(*position)
                .unwrap()
                .downcast::<gtk::glib::BoxedAnyObject>()
                .unwrap();
            object.borrow::<NoteListEntry>().note.id == id
        })
    }

    pub fn selected_ids(&self) -> Vec<String> {
        (0..self.model.n_items())
            .filter(|position| self.selection.is_selected(*position))
            .filter_map(|position| self.entry(position).map(|entry| entry.note.id))
            .collect()
    }

    pub fn select(&self, id: &str, exclusive: bool) {
        if let Some(position) = self.position(id) {
            self.selection.select_item(position, exclusive);
        }
    }

    pub fn unselect_all(&self) {
        self.selection.unselect_all();
    }

    pub fn scroll_to(&self, id: &str) {
        if let Some(position) = self.position(id) {
            let _ = self
                .view
                .activate_action("list.scroll-to-item", Some(&position.to_variant()));
        }
    }

    pub fn select_range(&self, anchor: &str, target: &str) {
        if let (Some(anchor), Some(target)) = (self.position(anchor), self.position(target)) {
            self.selection.unselect_all();
            self.selection
                .select_range(anchor.min(target), anchor.abs_diff(target) + 1, false);
        }
    }

    pub fn next_id(&self, current: Option<&str>, direction: i32) -> Option<String> {
        let count = self.model.n_items();
        if count == 0 {
            return None;
        }
        let position = match current.and_then(|id| self.position(id)) {
            Some(position) => {
                (position as i64 + direction as i64).clamp(0, count as i64 - 1) as u32
            }
            None if direction < 0 => count - 1,
            None => 0,
        };
        self.entry(position).map(|entry| entry.note.id)
    }

    pub fn replace(&self, entries: Vec<NoteListEntry>) {
        let old_count = self.model.n_items() as usize;
        let mut prefix = 0;
        while prefix < old_count.min(entries.len())
            && self.entry(prefix as u32).unwrap().matches(&entries[prefix])
        {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < (old_count - prefix).min(entries.len() - prefix)
            && self
                .entry((old_count - suffix - 1) as u32)
                .unwrap()
                .matches(&entries[entries.len() - suffix - 1])
        {
            suffix += 1;
        }
        let removed = old_count - prefix - suffix;
        let objects: Vec<_> = entries[prefix..entries.len() - suffix]
            .iter()
            .cloned()
            .map(gtk::glib::BoxedAnyObject::new)
            .collect();
        if removed != 0 || !objects.is_empty() {
            let selected_ids = self.selected_ids();
            self.model.splice(prefix as u32, removed as u32, &objects);
            for id in selected_ids {
                self.select(&id, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::time::Duration;

    fn entries() -> Vec<NoteListEntry> {
        (0..2700)
            .map(|position| NoteListEntry {
                note: Note {
                    id: format!("note-{position}"),
                    account_id: None,
                    folder_id: None,
                    title: format!("Notitie {position}"),
                    content: format!("# Notitie {position}\nPreview"),
                    updated_at: 0,
                    pinned: position < 2,
                    archived: false,
                },
                folder_name: String::new(),
                divider: position == 1,
            })
            .collect()
    }

    fn frames(window: &gtk::Window) {
        let main_loop = gtk::glib::MainLoop::new(None, false);
        let loop_for_tick = main_loop.clone();
        let count = Cell::new(0);
        let completed = Rc::new(Cell::new(false));
        let completed_for_tick = Rc::clone(&completed);
        let callback = window.add_tick_callback(move |_, _| {
            count.set(count.get() + 1);
            if count.get() >= 5 {
                completed_for_tick.set(true);
                loop_for_tick.quit();
                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });
        let loop_for_timeout = main_loop.clone();
        let timeout = gtk::glib::timeout_add_local_once(Duration::from_secs(5), move || {
            loop_for_timeout.quit();
        });
        main_loop.run();
        if completed.get() {
            timeout.remove();
        } else {
            callback.remove();
            panic!("GTK did not produce frames for the native list test");
        }
    }

    #[test]
    fn native_list_virtualizes_resizes_and_selects_2700_notes() {
        adw::init().expect("GTK display is required for the native list test");
        let list = NoteList::new();
        let bound = Rc::new(RefCell::new(HashSet::new()));
        let total_binds = Rc::new(Cell::new(0));
        let first_label = Rc::new(RefCell::new(None));
        let first_label_for_bind = Rc::clone(&first_label);
        let bound_for_bind = Rc::clone(&bound);
        let binds_for_bind = Rc::clone(&total_binds);
        list.factory.connect_bind(move |_, object| {
            let item = object.downcast_ref::<gtk::ListItem>().unwrap();
            bound_for_bind.borrow_mut().insert(item.position());
            binds_for_bind.set(binds_for_bind.get() + 1);
            let label = gtk::Label::builder()
                .label(format!("Notitie {}", item.position()))
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .height_request(64)
                .hexpand(true)
                .build();
            if item.position() == 0 {
                *first_label_for_bind.borrow_mut() = Some(label.downgrade());
            }
            item.set_child(Some(&label));
        });
        let bound_for_unbind = Rc::clone(&bound);
        list.factory.connect_unbind(move |_, object| {
            let item = object.downcast_ref::<gtk::ListItem>().unwrap();
            bound_for_unbind.borrow_mut().remove(&item.position());
            item.set_child(None::<&gtk::Widget>);
        });
        list.replace(entries());
        let scroll = gtk::ScrolledWindow::builder().child(&list.view).build();
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&gtk::Label::new(Some("Folders"))));
        paned.set_end_child(Some(&scroll));
        paned.set_position(80);
        let window = gtk::Window::builder()
            .title("Memonotes native list test")
            .default_width(320)
            .default_height(480)
            .child(&paned)
            .build();
        window.present();
        frames(&window);
        assert!(!bound.borrow().is_empty());
        assert!(bound.borrow().len() < 2700);
        let label = first_label.borrow().as_ref().unwrap().upgrade().unwrap();
        let text_size = label.layout().pixel_size();
        let narrow_label_width = label.width();
        let narrow_width = list.view.width();
        let binds_before_resize = total_binds.get();
        window.set_default_size(640, 480);
        frames(&window);
        assert!(list.view.width() > narrow_width);
        assert!(label.width() > narrow_label_width);
        assert_eq!(label.layout().pixel_size(), text_size);
        assert_eq!(total_binds.get(), binds_before_resize);

        let generation = Rc::new(Cell::new(1));
        let running = Rc::new(Cell::new(false));
        crate::animate_sidebar_position(&paned, 0, 1, Rc::clone(&generation), Rc::clone(&running));
        for _ in 0..10 {
            frames(&window);
            if !running.get() {
                break;
            }
        }
        assert!(!running.get());
        assert_eq!(paned.position(), 0);
        assert_eq!(label.layout().pixel_size(), text_size);
        assert_eq!(total_binds.get(), binds_before_resize);

        generation.set(2);
        crate::animate_sidebar_position(
            &paned,
            225,
            2,
            Rc::clone(&generation),
            Rc::clone(&running),
        );
        frames(&window);
        generation.set(3);
        crate::animate_sidebar_position(&paned, 0, 3, Rc::clone(&generation), Rc::clone(&running));
        for _ in 0..10 {
            frames(&window);
            if !running.get() {
                break;
            }
        }
        assert!(!running.get());
        assert_eq!(paned.position(), 0);
        assert_eq!(label.layout().pixel_size(), text_size);
        assert_eq!(total_binds.get(), binds_before_resize);

        list.select("note-2699", true);
        assert_eq!(list.selected_ids(), ["note-2699"]);
        list.select("note-0", false);
        assert_eq!(list.selected_ids(), ["note-0", "note-2699"]);
        list.select_range("note-200", "note-20");
        assert_eq!(list.selected_ids().len(), 181);
        assert_eq!(
            list.next_id(Some("note-2699"), 1).as_deref(),
            Some("note-2699")
        );
        assert_eq!(list.next_id(Some("note-0"), -1).as_deref(), Some("note-0"));
        assert_eq!(list.next_id(None, -1).as_deref(), Some("note-2699"));
        assert!(list.entry(1).unwrap().divider);

        let binds_before_refresh = total_binds.get();
        list.replace(entries());
        frames(&window);
        assert_eq!(total_binds.get(), binds_before_refresh);
        assert_eq!(list.selected_ids().len(), 181);
        list.scroll_to("note-2699");
        frames(&window);
        assert!(bound.borrow().contains(&2699));
        assert!(bound.borrow().len() < 2700);

        let mut updated = entries();
        updated[2699].note.title = "Updated title".to_owned();
        list.replace(updated);
        assert_eq!(list.entry(2699).unwrap().note.title, "Updated title");
        list.select("note-2699", true);
        let mut removed = entries();
        removed.remove(0);
        list.replace(removed);
        assert_eq!(list.position("note-2699"), Some(2698));
        assert_eq!(list.selected_ids(), ["note-2699"]);

        list.replace(Vec::new());
        assert!(list.selected_ids().is_empty());
        assert!(list.next_id(None, 1).is_none());
        window.destroy();
    }
}

use adw::gtk;
use adw::prelude::*;
use gtk::subclass::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct NoteListSnapshot {
        pub image: RefCell<Option<gtk::gdk::Paintable>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoteListSnapshot {
        const NAME: &'static str = "MemonotesNoteListSnapshot";
        type Type = super::NoteListSnapshot;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for NoteListSnapshot {}

    impl WidgetImpl for NoteListSnapshot {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().queue_draw();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(image) = self.image.borrow().as_ref() {
                let widget = self.obj();
                image.snapshot(snapshot, widget.width() as f64, widget.height() as f64);
            }
        }
    }
}

use gtk::glib;

glib::wrapper! {
    pub struct NoteListSnapshot(ObjectSubclass<imp::NoteListSnapshot>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl NoteListSnapshot {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .property("overflow", gtk::Overflow::Hidden)
            .build()
    }

    pub fn freeze(&self, widget: &impl IsA<gtk::Widget>) {
        let widget = widget.as_ref();
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let imp = self.imp();
        *imp.image.borrow_mut() = Some(paintable.current_image());
        self.queue_draw();
    }

    pub fn clear(&self) {
        self.imp().image.borrow_mut().take();
        self.queue_draw();
    }
}

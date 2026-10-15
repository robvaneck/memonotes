use adw::gtk;
use adw::prelude::*;
use libadwaita as adw;
use memonotes::editor_document::{TableAlignment, table_markdown};
use memonotes::i18n::tr;
use std::cell::Cell;
use std::rc::Rc;

pub const TABLE_WIDGET_NAME: &str = "memonotes-table";
pub const RULE_WIDGET_NAME: &str = "memonotes-rule";

pub fn is_table_widget(widget: &gtk::Widget) -> bool {
    widget.widget_name() == TABLE_WIDGET_NAME
}

pub fn is_rule_widget(widget: &gtk::Widget) -> bool {
    widget.widget_name() == RULE_WIDGET_NAME
}

pub fn build_rule_widget(editor: &gtk::TextView) -> gtk::Widget {
    let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
    rule.set_widget_name(RULE_WIDGET_NAME);
    rule.set_valign(gtk::Align::Center);
    rule.set_margin_top(8);
    rule.set_margin_bottom(8);
    fit_table_to_editor(editor, &rule);
    rule.upcast()
}

/// Cell widgets hold the content; the Markdown is read from them when the note is saved.
pub fn table_markdown_from_widget(widget: &gtk::Widget) -> Option<String> {
    if !is_table_widget(widget) {
        return None;
    }
    let grid = find_grid(widget)?;
    let (rows, columns) = dimensions(&grid);
    let alignments: Vec<_> = (0..columns).map(|column| alignment_of(&grid, column)).collect();
    let cells: Vec<Vec<String>> = (0..rows)
        .map(|row| {
            (0..columns)
                .map(|column| cell_text(&grid, column, row))
                .collect()
        })
        .collect();
    Some(table_markdown(&alignments, &cells))
}

/// Selected cells as `(first row, first column, last row, last column)`.
type CellRange = (i32, i32, i32, i32);

#[derive(Clone)]
struct TableContext {
    editor: gtk::glib::WeakRef<gtk::TextView>,
    grid: gtk::glib::WeakRef<gtk::Grid>,
    last_focus: Rc<Cell<(i32, i32)>>,
    selection: Rc<Cell<Option<CellRange>>>,
}

#[derive(Clone)]
struct TableState {
    alignments: Vec<TableAlignment>,
    rows: Vec<Vec<String>>,
}

fn read_state(grid: &gtk::Grid) -> TableState {
    let (rows, columns) = dimensions(grid);
    TableState {
        alignments: (0..columns).map(|column| alignment_of(grid, column)).collect(),
        rows: (0..rows)
            .map(|row| {
                (0..columns)
                    .map(|column| cell_text(grid, column, row))
                    .collect()
            })
            .collect(),
    }
}

fn fill_grid(context: &TableContext, grid: &gtk::Grid, state: &TableState) {
    for (row, cells) in state.rows.iter().enumerate() {
        for (column, text) in cells.iter().enumerate() {
            let alignment = state.alignments.get(column).copied().unwrap_or_default();
            let cell = make_cell(context, text, alignment, row == 0);
            grid.attach(&cell, column as i32, row as i32, 1, 1);
        }
    }
}

/// An in-place table edit that can be undone without rebuilding the whole note.
#[derive(Clone)]
pub struct TablePatch {
    context: TableContext,
    before: TableState,
    after: TableState,
}

impl TablePatch {
    /// Returns false when the table no longer exists, so the caller must rebuild instead.
    pub fn apply(&self, undo: bool) -> bool {
        let Some(grid) = self.context.grid.upgrade() else {
            return false;
        };
        if grid.root().is_none() {
            return false;
        }
        let had_focus = grid.focus_child().is_some();
        while let Some(child) = grid.first_child() {
            grid.remove(&child);
        }
        self.context.selection.set(None);
        grid.remove_css_class("table-selecting");
        fill_grid(
            &self.context,
            &grid,
            if undo { &self.before } else { &self.after },
        );
        if had_focus {
            let (rows, columns) = dimensions(&grid);
            let (row, column) = self.context.last_focus.get();
            focus_cell(&grid, row.min(rows - 1), column.min(columns - 1));
        }
        true
    }
}

fn find_grid(widget: &gtk::Widget) -> Option<gtk::Grid> {
    if let Some(grid) = widget.downcast_ref::<gtk::Grid>() {
        return Some(grid.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(grid) = find_grid(&current) {
            return Some(grid);
        }
        child = current.next_sibling();
    }
    None
}

fn table_root(grid: &gtk::Grid) -> Option<gtk::Widget> {
    let mut parent = grid.parent();
    while let Some(widget) = parent {
        if is_table_widget(&widget) {
            return Some(widget);
        }
        parent = widget.parent();
    }
    None
}

/// Returns `(rows, columns)`.
fn dimensions(grid: &gtk::Grid) -> (i32, i32) {
    let mut rows = 0;
    while grid.child_at(0, rows).is_some() {
        rows += 1;
    }
    let mut columns = 0;
    while grid.child_at(columns, 0).is_some() {
        columns += 1;
    }
    (rows, columns)
}

fn cell_text(grid: &gtk::Grid, column: i32, row: i32) -> String {
    grid.child_at(column, row)
        .and_downcast::<gtk::Entry>()
        .map(|entry| entry.text().to_string())
        .unwrap_or_default()
}

fn alignment_class(alignment: TableAlignment) -> &'static str {
    match alignment {
        TableAlignment::None => "table-align-none",
        TableAlignment::Left => "table-align-left",
        TableAlignment::Center => "table-align-center",
        TableAlignment::Right => "table-align-right",
    }
}

fn alignment_of(grid: &gtk::Grid, column: i32) -> TableAlignment {
    let Some(entry) = grid.child_at(column, 0) else {
        return TableAlignment::None;
    };
    [
        TableAlignment::Left,
        TableAlignment::Center,
        TableAlignment::Right,
    ]
    .into_iter()
    .find(|alignment| entry.has_css_class(alignment_class(*alignment)))
    .unwrap_or_default()
}

fn notify_editor_changed(context: &TableContext) {
    if let Some(editor) = context.editor.upgrade() {
        editor.buffer().emit_by_name::<()>("changed", &[]);
    }
}

fn set_selection(context: &TableContext, range: Option<CellRange>) {
    if context.selection.get() == range {
        return;
    }
    context.selection.set(range);
    let Some(grid) = context.grid.upgrade() else {
        return;
    };
    if range.is_some() {
        grid.add_css_class("table-selecting");
    } else {
        grid.remove_css_class("table-selecting");
    }
    let (rows, columns) = dimensions(&grid);
    for row in 0..rows {
        for column in 0..columns {
            let Some(cell) = grid.child_at(column, row) else {
                continue;
            };
            let selected = range.is_some_and(|(row_a, column_a, row_b, column_b)| {
                (row_a..=row_b).contains(&row) && (column_a..=column_b).contains(&column)
            });
            if selected {
                cell.add_css_class("table-cell-selected");
            } else {
                cell.remove_css_class("table-cell-selected");
            }
        }
    }
}

/// Returns the `(row, column)` of the cell under a point in grid coordinates.
fn cell_at(grid: &gtk::Grid, x: f64, y: f64) -> Option<(i32, i32)> {
    let mut widget = grid.pick(x, y, gtk::PickFlags::DEFAULT)?;
    while widget.parent().as_ref() != Some(grid.upcast_ref::<gtk::Widget>()) {
        widget = widget.parent()?;
    }
    let (column, row, _, _) = grid.query_child(&widget);
    Some((row, column))
}

fn selected_cells_text(grid: &gtk::Grid, range: CellRange) -> String {
    let (row_a, column_a, row_b, column_b) = range;
    (row_a..=row_b)
        .map(|row| {
            (column_a..=column_b)
                .map(|column| cell_text(grid, column, row))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clear_selected_cells(context: &TableContext, range: CellRange) {
    structural_change(context, |grid| {
        let (row_a, column_a, row_b, column_b) = range;
        for row in row_a..=row_b {
            for column in column_a..=column_b {
                if let Some(entry) = grid.child_at(column, row).and_downcast::<gtk::Entry>() {
                    entry.set_text("");
                }
            }
        }
    });
}

// A cell undoes its own typing first; otherwise the table-level history (paste, clear, rows) applies.
fn undo_in_cell(context: &TableContext, cell: &gtk::Widget, redo: bool) {
    let natively = cell.downcast_ref::<gtk::Entry>().is_some_and(|entry| {
        let mut child = entry.first_child();
        while let Some(current) = child {
            if let Some(text) = current.downcast_ref::<gtk::Text>() {
                let before = entry.text();
                let action = if redo { "text.redo" } else { "text.undo" };
                return text.activate_action(action, None).is_ok() && entry.text() != before;
            }
            child = current.next_sibling();
        }
        false
    });
    if natively {
        return;
    }
    let Some(editor) = context.editor.upgrade() else {
        return;
    };
    let Some(history) = crate::editor_image_history(&editor) else {
        return;
    };
    let restored = if redo {
        crate::redo_editor_image_change(&editor, &history)
    } else {
        crate::undo_editor_image_change(&editor, &history)
    };
    if restored && !crate::editor_cell_has_focus(&editor) {
        editor.grab_focus();
    }
}

/// Tab-separated rows fill cells from the selection's top-left (or the focused cell); one
/// value fills the whole selection. Plain text without a selection goes into the focused cell.
fn paste_text(context: &TableContext, text: &str, entry: Option<&gtk::Entry>) {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let block = memonotes::editor_document::parse_tsv(text);
    let single = block.len() == 1 && block[0].len() == 1;
    let selection = context.selection.get();
    if single && selection.is_none() {
        if let Some(entry) = entry {
            entry.delete_selection();
            let mut position = entry.position();
            entry.insert_text(text, &mut position);
            entry.set_position(position);
        }
        return;
    }
    let Some(grid) = context.grid.upgrade() else {
        return;
    };
    let (rows, columns) = dimensions(&grid);
    let (first_row, first_column, last_row, last_column) = selection.unwrap_or_else(|| {
        let (row, column) = context.last_focus.get();
        (row, column, row, column)
    });
    let filled = if single {
        (first_row, first_column, last_row, last_column)
    } else {
        let height = block.len() as i32;
        let width = block.iter().map(Vec::len).max().unwrap_or(1) as i32;
        (
            first_row,
            first_column,
            (first_row + height - 1).min(rows - 1),
            (first_column + width - 1).min(columns - 1),
        )
    };
    structural_change(context, |grid| {
        for row in filled.0..=filled.2 {
            for column in filled.1..=filled.3 {
                let value = if single {
                    block[0][0].as_str()
                } else {
                    block
                        .get((row - first_row) as usize)
                        .and_then(|line| line.get((column - first_column) as usize))
                        .map_or("", String::as_str)
                };
                if let Some(cell) = grid.child_at(column, row).and_downcast::<gtk::Entry>() {
                    cell.set_text(value);
                }
            }
        }
    });
    set_selection(context, Some(filled));
}

// Cells keep the pointer grab while dragging, so plain motion events are tracked instead of
// a gesture: a cell's own text gesture would claim the sequence and cancel ours.
fn install_cell_selection(context: &TableContext, grid: &gtk::Grid) {
    let start_cell: Rc<Cell<Option<(i32, i32)>>> = Rc::new(Cell::new(None));

    let press = gtk::GestureClick::new();
    press.set_button(gtk::gdk::BUTTON_PRIMARY);
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    let context_for_press = context.clone();
    let start_for_press = Rc::clone(&start_cell);
    press.connect_pressed(move |_, _, x, y| {
        set_selection(&context_for_press, None);
        let start = context_for_press
            .grid
            .upgrade()
            .and_then(|grid| cell_at(&grid, x, y));
        start_for_press.set(start);
    });
    grid.add_controller(press);

    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    let context_for_motion = context.clone();
    motion.connect_motion(move |controller, x, y| {
        if !controller
            .current_event_state()
            .contains(gtk::gdk::ModifierType::BUTTON1_MASK)
        {
            start_cell.set(None);
            return;
        }
        let (Some(start), Some(grid)) = (start_cell.get(), context_for_motion.grid.upgrade())
        else {
            return;
        };
        let Some(current) = cell_at(&grid, x, y) else {
            return;
        };
        if current == start && context_for_motion.selection.get().is_none() {
            return;
        }
        set_selection(
            &context_for_motion,
            Some((
                start.0.min(current.0),
                start.1.min(current.1),
                start.0.max(current.0),
                start.1.max(current.1),
            )),
        );
    });
    grid.add_controller(motion);
}

fn focus_cell(grid: &gtk::Grid, row: i32, column: i32) -> bool {
    grid.child_at(column, row)
        .is_some_and(|widget| widget.grab_focus())
}

fn make_cell(
    context: &TableContext,
    text: &str,
    alignment: TableAlignment,
    header: bool,
) -> gtk::Entry {
    let entry = gtk::Entry::builder()
        .text(text)
        .width_chars(10)
        .hexpand(true)
        .xalign(match alignment {
            TableAlignment::Center => 0.5,
            TableAlignment::Right => 1.0,
            TableAlignment::None | TableAlignment::Left => 0.0,
        })
        .build();
    entry.add_css_class("table-cell");
    entry.add_css_class(alignment_class(alignment));
    if header {
        entry.add_css_class("table-header");
    }

    let context_for_change = context.clone();
    entry.connect_changed(move |_| notify_editor_changed(&context_for_change));

    let context_for_activate = context.clone();
    entry.connect_activate(move |entry| {
        let Some(grid) = context_for_activate.grid.upgrade() else {
            return;
        };
        let (column, row, _, _) = grid.query_child(entry);
        let (rows, _) = dimensions(&grid);
        if row + 1 >= rows {
            add_row(&context_for_activate);
        }
        focus_cell(&grid, row + 1, column);
    });

    let context_for_focus = context.clone();
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter(move |controller| {
        let (Some(grid), Some(widget)) = (context_for_focus.grid.upgrade(), controller.widget())
        else {
            return;
        };
        let (column, row, _, _) = grid.query_child(&widget);
        context_for_focus.last_focus.set((row, column));
        set_selection(&context_for_focus, None);
    });
    entry.add_controller(focus);

    let context_for_keys = context.clone();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |controller, key, _, modifiers| {
        let (Some(grid), Some(widget)) = (context_for_keys.grid.upgrade(), controller.widget())
        else {
            return gtk::glib::Propagation::Proceed;
        };
        let (column, row, _, _) = grid.query_child(&widget);
        let (rows, columns) = dimensions(&grid);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        if control
            && matches!(
                key,
                gtk::gdk::Key::z | gtk::gdk::Key::Z | gtk::gdk::Key::y | gtk::gdk::Key::Y
            )
        {
            let redo = matches!(key, gtk::gdk::Key::y | gtk::gdk::Key::Y)
                || (shift && matches!(key, gtk::gdk::Key::z | gtk::gdk::Key::Z));
            undo_in_cell(&context_for_keys, &widget, redo);
            return gtk::glib::Propagation::Stop;
        }
        if control && !shift && matches!(key, gtk::gdk::Key::v | gtk::gdk::Key::V) {
            let context = context_for_keys.clone();
            let entry = widget.downcast_ref::<gtk::Entry>().cloned();
            let clipboard = widget.clipboard();
            gtk::glib::MainContext::default().spawn_local(async move {
                if let Ok(Some(text)) = clipboard.read_text_future().await {
                    paste_text(&context, &text, entry.as_ref());
                }
            });
            return gtk::glib::Propagation::Stop;
        }
        if let Some(range) = context_for_keys.selection.get() {
            match key {
                gtk::gdk::Key::Escape => {
                    set_selection(&context_for_keys, None);
                    return gtk::glib::Propagation::Stop;
                }
                gtk::gdk::Key::Delete | gtk::gdk::Key::BackSpace if !control => {
                    clear_selected_cells(&context_for_keys, range);
                    return gtk::glib::Propagation::Stop;
                }
                gtk::gdk::Key::c | gtk::gdk::Key::C | gtk::gdk::Key::x | gtk::gdk::Key::X
                    if control =>
                {
                    widget
                        .clipboard()
                        .set_text(&selected_cells_text(&grid, range));
                    if matches!(key, gtk::gdk::Key::x | gtk::gdk::Key::X) {
                        clear_selected_cells(&context_for_keys, range);
                    }
                    return gtk::glib::Propagation::Stop;
                }
                _ if !control && !crate::is_modifier_key(key) => {
                    set_selection(&context_for_keys, None);
                }
                _ => {}
            }
        }
        match key {
            gtk::gdk::Key::Tab if !shift => {
                if column + 1 < columns {
                    focus_cell(&grid, row, column + 1);
                } else if row + 1 < rows {
                    focus_cell(&grid, row + 1, 0);
                } else {
                    add_row(&context_for_keys);
                    focus_cell(&grid, rows, 0);
                }
            }
            gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab => {
                if column > 0 {
                    focus_cell(&grid, row, column - 1);
                } else if row > 0 {
                    focus_cell(&grid, row - 1, columns - 1);
                } else if let Some(editor) = context_for_keys.editor.upgrade() {
                    editor.grab_focus();
                }
            }
            gtk::gdk::Key::Up if row > 0 => {
                focus_cell(&grid, row - 1, column);
            }
            gtk::gdk::Key::Down if row + 1 < rows => {
                focus_cell(&grid, row + 1, column);
            }
            _ => return gtk::glib::Propagation::Proceed,
        }
        gtk::glib::Propagation::Stop
    });
    entry.add_controller(keys);
    entry
}

// Structure edits are not tracked by the buffer, so they go through the Markdown snapshot history.
fn structural_change(context: &TableContext, change: impl FnOnce(&gtk::Grid)) {
    let (Some(editor), Some(grid)) = (context.editor.upgrade(), context.grid.upgrade()) else {
        return;
    };
    match crate::editor_image_history(&editor) {
        Some(history) => {
            let state_before = read_state(&grid);
            let before = crate::begin_editor_image_change(&editor, &history);
            change(&grid);
            set_selection(context, None);
            crate::finish_editor_image_change(&editor, &history, before);
            // A removed table has no root and is recreated from the snapshot on undo.
            if grid.root().is_some()
                && let Some(entry) = history.borrow_mut().undo.last_mut()
            {
                entry.table_patch = Some(TablePatch {
                    context: context.clone(),
                    before: state_before,
                    after: read_state(&grid),
                });
            }
        }
        None => {
            change(&grid);
            set_selection(context, None);
            notify_editor_changed(context);
        }
    }
}

fn add_row(context: &TableContext) {
    structural_change(context, |grid| {
        let (rows, columns) = dimensions(grid);
        for column in 0..columns {
            let alignment = alignment_of(grid, column);
            grid.attach(&make_cell(context, "", alignment, false), column, rows, 1, 1);
        }
    });
}

fn add_column(context: &TableContext) {
    structural_change(context, |grid| {
        let (rows, columns) = dimensions(grid);
        for row in 0..rows {
            grid.attach(
                &make_cell(context, "", TableAlignment::None, row == 0),
                columns,
                row,
                1,
                1,
            );
        }
    });
}

fn delete_row(context: &TableContext) {
    let Some(grid) = context.grid.upgrade() else {
        return;
    };
    let (rows, _) = dimensions(&grid);
    if rows <= 1 {
        return;
    }
    // The header row stays; without a selected or focused body row the last one goes.
    let (focused_row, focused_column) = context.last_focus.get();
    let (first, last) = match context.selection.get() {
        Some((row_a, _, row_b, _)) => (row_a.max(1), row_b.min(rows - 1)),
        None if focused_row == 0 || focused_row >= rows => (rows - 1, rows - 1),
        None => (focused_row, focused_row),
    };
    if first > last {
        return;
    }
    structural_change(context, |grid| {
        for row in (first..=last).rev() {
            grid.remove_row(row);
        }
    });
    let remaining = rows - (last - first + 1);
    context
        .last_focus
        .set((first.min(remaining - 1), focused_column));
}

fn delete_column(context: &TableContext) {
    let Some(grid) = context.grid.upgrade() else {
        return;
    };
    let (_, columns) = dimensions(&grid);
    if columns <= 1 {
        return;
    }
    let (focused_row, focused_column) = context.last_focus.get();
    let (first, last) = match context.selection.get() {
        Some((_, column_a, _, column_b)) => (column_a, column_b.min(columns - 1)),
        None if focused_column >= columns => (columns - 1, columns - 1),
        None => (focused_column, focused_column),
    };
    // At least one column must remain.
    if first > last || last - first + 1 >= columns {
        return;
    }
    structural_change(context, |grid| {
        for column in (first..=last).rev() {
            grid.remove_column(column);
        }
    });
    let remaining = columns - (last - first + 1);
    context
        .last_focus
        .set((focused_row, first.min(remaining - 1)));
}

fn delete_table(context: &TableContext) {
    let context = context.clone();
    // The button lives inside the widget being removed.
    gtk::glib::idle_add_local_once(move || {
        structural_change(&context, |grid| {
            let (Some(root), Some(editor)) = (table_root(grid), context.editor.upgrade()) else {
                return;
            };
            let buffer = editor.buffer();
            if let Some(anchor) = crate::find_image_anchor(&buffer, &root) {
                let mut start = buffer.iter_at_child_anchor(&anchor);
                let mut end = start;
                end.forward_char();
                editor.remove(&root);
                buffer.delete(&mut start, &mut end);
            }
        });
    });
}

fn control_button(label: &str, context: &TableContext, action: fn(&TableContext)) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .css_classes(["flat", "caption"])
        .focus_on_click(false)
        .build();
    let context = context.clone();
    button.connect_clicked(move |_| action(&context));
    button
}

pub fn build_table_widget(
    editor: &gtk::TextView,
    alignments: &[TableAlignment],
    rows: &[Vec<String>],
) -> gtk::Widget {
    let columns = alignments
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    let grid = gtk::Grid::builder()
        .column_homogeneous(true)
        .css_classes(["editor-table-grid"])
        .build();
    let context = TableContext {
        editor: editor.downgrade(),
        grid: grid.downgrade(),
        last_focus: Rc::new(Cell::new((0, 0))),
        selection: Rc::new(Cell::new(None)),
    };
    let mut state_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| (0..columns).map(|column| row.get(column).cloned().unwrap_or_default()).collect())
        .collect();
    if state_rows.is_empty() {
        state_rows.push(vec![String::new(); columns]);
    }
    let mut state_alignments = alignments.to_vec();
    state_alignments.resize(columns, TableAlignment::None);
    fill_grid(
        &context,
        &grid,
        &TableState {
            alignments: state_alignments,
            rows: state_rows,
        },
    );

    install_cell_selection(&context, &grid);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .hexpand(true)
        .child(&grid)
        .build();

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    controls.append(&control_button(tr("editor.table.add_row"), &context, add_row));
    controls.append(&control_button(
        tr("editor.table.add_column"),
        &context,
        add_column,
    ));
    controls.append(&control_button(
        tr("editor.table.delete_row"),
        &context,
        delete_row,
    ));
    controls.append(&control_button(
        tr("editor.table.delete_column"),
        &context,
        delete_column,
    ));
    controls.append(&control_button(
        tr("editor.table.delete"),
        &context,
        delete_table,
    ));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
    root.set_widget_name(TABLE_WIDGET_NAME);
    root.add_css_class("editor-table");
    root.set_halign(gtk::Align::Fill);
    root.set_margin_top(6);
    root.set_margin_bottom(6);
    root.append(&scroller);
    root.append(&controls);
    fit_table_to_editor(editor, &root);
    root.upcast()
}

// Anchored children keep their natural width, so the text width is applied explicitly.
fn fit_table_to_editor<W: IsA<gtk::Widget>>(editor: &gtk::TextView, root: &W) {
    let editor_weak = editor.downgrade();
    let root_weak = root.downgrade();
    let applied = Cell::new(0);
    editor.add_tick_callback(move |_, _| {
        let (Some(editor), Some(root)) = (editor_weak.upgrade(), root_weak.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        let width = editor.width() - editor.left_margin() - editor.right_margin();
        if width > 0 && width != applied.get() {
            applied.set(width);
            root.set_size_request(width, -1);
        }
        gtk::glib::ControlFlow::Continue
    });

}
/// Inserts an empty table on its own line after the cursor line, as one undo step.
pub fn insert_table(editor: &gtk::TextView) {
    let mut rows = vec![
        (1..=2)
            .map(|number| memonotes::tr_format!("editor.table.column_title", n = number))
            .collect::<Vec<_>>(),
    ];
    rows.extend(std::iter::repeat_n(vec![String::new(); 2], 2));
    insert_table_with_rows(editor, &rows);
}

/// Inserts a table for pasted spreadsheet cells, replacing any selected text.
pub fn insert_pasted_table(editor: &gtk::TextView, rows: &[Vec<String>]) {
    editor.buffer().delete_selection(true, editor.is_editable());
    insert_table_with_rows(editor, rows);
}

fn insert_table_with_rows(editor: &gtk::TextView, rows: &[Vec<String>]) {
    let columns = rows.first().map_or(1, Vec::len).max(1);
    let alignments = vec![TableAlignment::None; columns];

    let history = crate::editor_image_history(editor);
    let before = history
        .as_ref()
        .map(|history| crate::begin_editor_image_change(editor, history));

    let buffer = editor.buffer();
    let mut iter = buffer.iter_at_mark(&buffer.get_insert());
    // The first line is the note title.
    if iter.line() == 0 {
        iter.forward_to_line_end();
        buffer.insert(&mut iter, "\n");
    } else if iter.line_offset() != 0 {
        buffer.insert(&mut iter, "\n");
    }
    let anchor = buffer.create_child_anchor(&mut iter);
    if iter.is_end() || iter.char() != '\n' {
        buffer.insert(&mut iter, "\n");
    }
    let widget = build_table_widget(editor, &alignments, rows);
    editor.add_child_at_anchor(&widget, &anchor);

    match (history, before) {
        (Some(history), Some(before)) => {
            crate::finish_editor_image_change(editor, &history, before)
        }
        _ => buffer.emit_by_name::<()>("changed", &[]),
    }
    if let Some(grid) = find_grid(&widget) {
        gtk::glib::idle_add_local_once(move || {
            focus_cell(&grid, 0, 0);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_cells_can_be_copied_cleared_and_deleted() -> anyhow::Result<()> {
        if std::env::var_os("MEMONOTES_TABLE_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_table::tests::selected_cells_can_be_copied_cleared_and_deleted",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_TABLE_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        let rows: Vec<Vec<String>> = (0..4)
            .map(|row| (0..3).map(|column| format!("r{row}c{column}")).collect())
            .collect();
        let widget = build_table_widget(&editor, &[TableAlignment::None; 3], &rows);
        let grid = find_grid(&widget).unwrap();
        let context = TableContext {
            editor: editor.downgrade(),
            grid: grid.downgrade(),
            last_focus: Rc::new(Cell::new((0, 0))),
            selection: Rc::new(Cell::new(None)),
        };

        set_selection(&context, Some((1, 0, 2, 1)));
        let selected = |row, column| {
            grid.child_at(column, row)
                .unwrap()
                .has_css_class("table-cell-selected")
        };
        assert!(selected(1, 0) && selected(2, 1));
        assert!(!selected(0, 0) && !selected(3, 1) && !selected(1, 2));
        assert_eq!(
            selected_cells_text(&grid, (1, 0, 2, 1)),
            "r1c0\tr1c1\nr2c0\tr2c1"
        );

        clear_selected_cells(&context, (1, 0, 2, 1));
        assert_eq!(cell_text(&grid, 0, 1), "");
        assert_eq!(cell_text(&grid, 2, 1), "r1c2");
        assert!(!selected(1, 0));

        set_selection(&context, Some((1, 2, 2, 2)));
        paste_text(&context, "a\nb", None);
        assert_eq!(selected_cells_text(&grid, (1, 2, 2, 2)), "a\nb");
        assert_eq!(context.selection.get(), Some((1, 2, 2, 2)));
        set_selection(&context, Some((1, 0, 1, 0)));
        paste_text(&context, "x\ty\n1\t2", None);
        assert_eq!(selected_cells_text(&grid, (1, 0, 2, 2)), "x\ty\ta\n1\t2\tb");
        set_selection(&context, Some((1, 0, 3, 1)));
        paste_text(&context, "same", None);
        assert_eq!(selected_cells_text(&grid, (1, 0, 3, 1)), "same\tsame\nsame\tsame\nsame\tsame");
        clear_selected_cells(&context, (1, 0, 2, 1));

        set_selection(&context, Some((2, 0, 3, 0)));
        delete_row(&context);
        assert_eq!(dimensions(&grid), (2, 3));
        set_selection(&context, Some((0, 0, 0, 1)));
        delete_column(&context);
        assert_eq!(dimensions(&grid), (2, 1));
        set_selection(&context, Some((0, 0, 0, 0)));
        delete_column(&context);
        assert_eq!(dimensions(&grid), (2, 1));
        Ok(())
    }
}

#[cfg(test)]
mod undo_tests {
    use super::*;

    #[test]
    fn ctrl_z_in_a_cell_undoes_table_level_changes() -> anyhow::Result<()> {
        if std::env::var_os("MEMONOTES_TABLE_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_table::undo_tests::ctrl_z_in_a_cell_undoes_table_level_changes",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_TABLE_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        crate::install_editor_format_shortcuts(&editor, crate::new_editor_image_history());
        let source = "T\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
        crate::set_editor_view_content(&editor, source);
        let buffer = editor.buffer();
        let table = |buffer: &gtk::TextBuffer| {
            let mut iter = buffer.start_iter();
            while !iter.is_end() {
                if let Some(widget) = iter
                    .child_anchor()
                    .and_then(|anchor| anchor.widgets().into_iter().find(is_table_widget))
                {
                    return widget;
                }
                iter.forward_char();
            }
            panic!("no table");
        };
        let key_controller = |widget: &gtk::Widget| {
            let controllers = widget.observe_controllers();
            (0..controllers.n_items())
                .filter_map(|index| controllers.item(index))
                .find_map(|controller| controller.downcast::<gtk::EventControllerKey>().ok())
                .unwrap()
        };
        let grid = find_grid(&table(&buffer)).unwrap();
        let context = TableContext {
            editor: editor.downgrade(),
            grid: grid.downgrade(),
            last_focus: Rc::new(Cell::new((0, 0))),
            selection: Rc::new(Cell::new(None)),
        };
        clear_selected_cells(&context, (1, 0, 1, 1));
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("|  |  |"));

        let cell = table(&buffer);
        let entry = find_grid(&cell).unwrap().child_at(0, 0).unwrap();
        let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
        key_controller(&entry).emit_by_name::<bool>(
            "key-pressed",
            &[&gtk::gdk::Key::z, &0u32, &ctrl],
        );
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("| 1 | 2 |"));

        let entry = find_grid(&table(&buffer)).unwrap().child_at(0, 0).unwrap();
        key_controller(&entry).emit_by_name::<bool>(
            "key-pressed",
            &[&gtk::gdk::Key::y, &0u32, &ctrl],
        );
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("|  |  |"));

        // The window-level shortcut uses this when the editor itself has focus.
        crate::undo_focused_editor(&editor, false);
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("| 1 | 2 |"));
        crate::undo_focused_editor(&editor, true);
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("|  |  |"));
        Ok(())
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn undo_keeps_the_scroll_position() -> anyhow::Result<()> {
        if std::env::var_os("MEMONOTES_TABLE_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_table::scroll_tests::undo_keeps_the_scroll_position",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_TABLE_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        crate::install_editor_format_shortcuts(&editor, crate::new_editor_image_history());
        let filler = "line\n".repeat(80);
        let source = format!("T\n\n{filler}\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nend");
        crate::set_editor_view_content(&editor, &source);
        let scroller = gtk::ScrolledWindow::builder().child(&editor).build();
        let window = gtk::Window::builder()
            .child(&scroller)
            .default_width(500)
            .default_height(300)
            .build();
        window.present();
        let settle = || {
            let context = gtk::glib::MainContext::default();
            for _ in 0..40 {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        settle();
        let adjustment = scroller.vadjustment();
        adjustment.set_value(adjustment.upper());
        settle();
        let scrolled = adjustment.value();
        assert!(scrolled > 100.0, "scrolled {scrolled}");

        let buffer = editor.buffer();
        let widget = {
            let mut iter = buffer.start_iter();
            loop {
                if let Some(widget) = iter
                    .child_anchor()
                    .and_then(|anchor| anchor.widgets().into_iter().find(is_table_widget))
                {
                    break widget;
                }
                iter.forward_char();
            }
        };
        let grid = find_grid(&widget).unwrap();
        let context = TableContext {
            editor: editor.downgrade(),
            grid: grid.downgrade(),
            last_focus: Rc::new(Cell::new((0, 0))),
            selection: Rc::new(Cell::new(None)),
        };
        add_row(&context);
        settle();
        // The view must never visibly jump, not only end up in the right place.
        let jumped = Rc::new(Cell::new(false));
        let jumped_for_signal = Rc::clone(&jumped);
        adjustment.connect_value_changed(move |adjustment| {
            if (adjustment.value() - scrolled).abs() > 40.0 {
                jumped_for_signal.set(true);
            }
        });
        crate::undo_focused_editor(&editor, false);
        for _ in 0..4 {
            settle();
        }
        assert!(!jumped.get(), "the scroll position jumped during undo");
        assert_eq!(dimensions(&grid), (2, 2));
        assert!(
            (adjustment.value() - scrolled).abs() < 40.0,
            "scroll moved from {scrolled} to {}",
            adjustment.value()
        );
        Ok(())
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn pasting_spreadsheet_cells_creates_a_table_and_plain_text_still_pastes() -> anyhow::Result<()> {
        if std::env::var_os("MEMONOTES_TABLE_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "editor_table::paste_tests::pasting_spreadsheet_cells_creates_a_table_and_plain_text_still_pastes",
                    "--test-threads=1",
                ])
                .env("MEMONOTES_TABLE_TEST", "1")
                .status()?;
            assert!(status.success());
            return Ok(());
        }
        gtk::init()?;
        let editor = gtk::TextView::new();
        crate::install_editor_format_shortcuts(&editor, crate::new_editor_image_history());
        crate::set_editor_view_content(&editor, "Title\n\nIntro");
        let buffer = editor.buffer();
        let paste = |text: &str| {
            editor.clipboard().set_text(text);
            editor.emit_by_name::<()>("paste-clipboard", &[]);
            let context = gtk::glib::MainContext::default();
            for _ in 0..50 {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };

        buffer.place_cursor(&buffer.end_iter());
        paste("Item\tQty\nMilk\t2\n");
        assert_eq!(
            crate::markdown_from_editor_buffer(&buffer),
            "# Title\n\nIntro\n\n| Item | Qty |\n| --- | --- |\n| Milk | 2 |\n"
        );

        buffer.place_cursor(&buffer.iter_at_line(2).unwrap());
        paste("plain words");
        assert!(crate::markdown_from_editor_buffer(&buffer).contains("plain wordsIntro"));
        Ok(())
    }
}

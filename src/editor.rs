use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, Corner, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, FontWeight, GlobalElementId, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, actions, anchored, deferred, div, fill, point, prelude::*, px, relative, size,
};
use numnum_core::lexer::{Lexer, TokenKind};
use numnum_core::types::{CurrencyTable, NumberFormat, UnitTable};
use unicode_segmentation::*;

use std::collections::HashMap;

use crate::theme::Theme;

/// Check if a name is likely a plural form by seeing if a shorter singular exists in the same table.
fn is_likely_plural<V>(name: &str, table: &HashMap<String, V>) -> bool {
    if name.ends_with('s') && name.len() > 2 {
        let singular = &name[..name.len() - 1];
        if table.contains_key(singular) {
            return true;
        }
    }
    // "feet" → "foot" (special case)
    if name.ends_with("feet") {
        let singular = format!("{}foot", &name[..name.len() - 4]);
        if table.contains_key(&singular) {
            return true;
        }
    }
    false
}

actions!(
    editor,
    [
        Enter,
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocumentStart,
        DocumentEnd,
        SelectDocumentStart,
        SelectDocumentEnd,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Tab,
        Escape,
    ]
);

// ── Autocomplete ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum CompletionCategory {
    Function,
    Unit,
    Currency,
    Keyword,
    Scale,
    Constant,
    Variable,
    Aggregation,
}

impl CompletionCategory {
    fn label(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Unit => "unit",
            Self::Currency => "currency",
            Self::Keyword => "keyword",
            Self::Scale => "scale",
            Self::Constant => "const",
            Self::Variable => "variable",
            Self::Aggregation => "aggregate",
        }
    }
}

#[derive(Debug, Clone)]
struct CompletionItem {
    label: String,
    category: CompletionCategory,
}

struct CompletionState {
    visible: bool,
    anchor_offset: usize,
    prefix: String,
    all_candidates: Vec<CompletionItem>,
    filtered: Vec<CompletionItem>,
    selected_index: usize,
}

#[derive(Clone)]
struct UndoEntry {
    content: String,
    selected_range: Range<usize>,
}

type OnChangeFn = Box<dyn Fn(&str, &mut Window, &mut App)>;

/// Gutter width as a multiple of font size (2.5x font_size ≈ room for 3 digits).
const GUTTER_WIDTH_FACTOR: f32 = 2.5;
/// Gutter padding as a fraction of font size.
const GUTTER_PADDING_FACTOR: f32 = 0.5;

enum SelectMode {
    Character,
    Word { anchor_start: usize, anchor_end: usize },
    Line { anchor_start: usize, anchor_end: usize },
}

fn previous_word_boundary(text: &str, offset: usize) -> usize {
    text.unicode_word_indices()
        .take_while(|(start, _)| *start < offset)
        .map(|(start, _)| start)
        .last()
        .unwrap_or(0)
}

fn next_word_boundary(text: &str, offset: usize) -> usize {
    text.unicode_word_indices()
        .find_map(|(start, word)| {
            let end = start + word.len();
            (end > offset).then_some(end)
        })
        .unwrap_or(text.len())
}

fn extend_selection(mut range: Range<usize>, mut reversed: bool, offset: usize) -> (Range<usize>, bool) {
    if reversed {
        range.start = offset;
    } else {
        range.end = offset;
    }
    if range.end < range.start {
        reversed = !reversed;
        range = range.end..range.start;
    }
    (range, reversed)
}

pub struct Editor {
    focus_handle: FocusHandle,
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    select_mode: Option<SelectMode>,
    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    on_change: Option<OnChangeFn>,
    pub theme: Theme,
    pub font_family: SharedString,
    pub font_size: Pixels,
    unit_table: UnitTable,
    currency_table: CurrencyTable,
    // Per-line diagnostics (error messages shown as inlay below the line)
    pub diagnostics: Vec<Option<String>>,
    pub show_diagnostics: bool,
    // Per-line layout cache (rebuilt each frame)
    line_layouts: Vec<Option<WrappedLine>>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    // Per-line visual line count (1 for unwrapped, 2+ for wrapped)
    pub line_visual_counts: Vec<usize>,
    // Cursor blink state
    pub cursor_visible: bool,
    blink_epoch: usize,
    // Autocomplete
    completion: CompletionState,
    pub known_variables: Vec<String>,
    pub number_format: NumberFormat,
}

impl Editor {
    pub fn new(
        cx: &mut Context<Self>,
        theme: Theme,
        font_family: String,
        font_size: f32,
        on_change: Option<OnChangeFn>,
        unit_table: UnitTable,
        currency_table: CurrencyTable,
    ) -> Self {
        let mut editor = Editor {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            select_mode: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            on_change,
            theme,
            font_family: SharedString::from(font_family),
            font_size: px(font_size),
            diagnostics: Vec::new(),
            show_diagnostics: true,
            unit_table,
            currency_table,
            line_layouts: Vec::new(),
            last_bounds: None,
            line_height: px(26.0),
            line_visual_counts: Vec::new(),
            cursor_visible: true,
            blink_epoch: 0,
            completion: CompletionState {
                visible: false,
                anchor_offset: 0,
                prefix: String::new(),
                all_candidates: Vec::new(),
                filtered: Vec::new(),
                selected_index: 0,
            },
            known_variables: Vec::new(),
            number_format: NumberFormat::US,
        };
        editor.rebuild_completion_candidates();
        editor.schedule_blink(cx);
        editor
    }

    fn schedule_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_epoch += 1;
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |editor, cx| {
                    if editor.blink_epoch == epoch {
                        editor.cursor_visible = !editor.cursor_visible;
                        cx.notify();
                        editor.schedule_blink(cx);
                    }
                });
            }
        }).detach();
    }

    pub fn pause_blinking(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self.blink_epoch += 1; // cancel current timer
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |editor, cx| {
                    if editor.blink_epoch == epoch {
                        editor.schedule_blink(cx);
                    }
                });
            }
        }).detach();
        cx.notify();
    }

    // ── Autocomplete ──────────────────────────────────────────────────────

    fn rebuild_completion_candidates(&mut self) {
        let mut items: Vec<CompletionItem> = Vec::new();

        // Keywords first — take priority over unit/currency aliases (e.g. "in" is keyword, not inch)
        for name in &["in", "to", "as", "into", "of", "from", "on", "off", "what is"] {
            items.push(CompletionItem {
                label: name.to_string(),
                category: CompletionCategory::Keyword,
            });
        }

        // Functions
        for name in &[
            "sqrt", "cbrt", "abs", "round", "ceil", "floor", "log", "ln", "fact",
            "sin", "cos", "tan", "asin", "acos", "atan", "sinh", "cosh", "tanh",
            "arcsin", "arccos", "arctan",
        ] {
            items.push(CompletionItem {
                label: name.to_string(),
                category: CompletionCategory::Function,
            });
        }

        // Constants
        for name in &["pi", "e"] {
            items.push(CompletionItem {
                label: name.to_string(),
                category: CompletionCategory::Constant,
            });
        }

        // Aggregation
        for name in &["sum", "total", "average", "avg", "prev"] {
            items.push(CompletionItem {
                label: name.to_string(),
                category: CompletionCategory::Aggregation,
            });
        }

        // Scale words
        for name in &[
            "thousand", "million", "billion", "trillion",
            "quadrillion", "quintillion", "sextillion", "septillion",
        ] {
            items.push(CompletionItem {
                label: name.to_string(),
                category: CompletionCategory::Scale,
            });
        }

        // Units — singular forms only (plurals work when typed, just not suggested)
        for name in self.unit_table.name_to_id.keys() {
            if is_likely_plural(name, &self.unit_table.name_to_id) { continue; }
            items.push(CompletionItem {
                label: name.clone(),
                category: CompletionCategory::Unit,
            });
        }

        // Currencies — singular forms only
        for name in self.currency_table.name_to_id.keys() {
            if is_likely_plural(name, &self.currency_table.name_to_id) { continue; }
            items.push(CompletionItem {
                label: name.clone(),
                category: CompletionCategory::Currency,
            });
        }

        // User-defined variables
        for name in &self.known_variables {
            items.push(CompletionItem {
                label: name.clone(),
                category: CompletionCategory::Variable,
            });
        }

        items.sort_by(|a, b| a.label.cmp(&b.label));
        // Deduplicate by label (keep first occurrence — keywords/functions win over unit aliases)
        items.dedup_by(|a, b| a.label == b.label);
        self.completion.all_candidates = items;
    }

    pub fn set_known_variables(&mut self, vars: Vec<String>) {
        if self.known_variables != vars {
            self.known_variables = vars;
            self.rebuild_completion_candidates();
        }
    }

    fn find_word_start(&self, offset: usize) -> usize {
        let bytes = self.content.as_bytes();
        let mut i = offset;
        while i > 0 {
            let prev = i - 1;
            if prev < bytes.len() && (bytes[prev].is_ascii_alphanumeric() || bytes[prev] == b'_') {
                i = prev;
            } else {
                break;
            }
        }
        i
    }

    fn find_word_end(&self, offset: usize) -> usize {
        let bytes = self.content.as_bytes();
        let mut i = offset;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        i
    }

    fn surrounding_word(&self, offset: usize) -> Range<usize> {
        self.find_word_start(offset)..self.find_word_end(offset)
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        let (line, _) = self.line_col_for_offset(offset);
        let start = self.line_start_offset(line);
        let end = if line + 1 < self.line_count() {
            self.line_start_offset(line + 1)
        } else {
            self.content.len()
        };
        start..end
    }

    fn update_completion_state(&mut self) {
        let offset = self.cursor_offset();
        let prefix_start = self.find_word_start(offset);
        let prefix = &self.content[prefix_start..offset];

        // Don't show on comment or header lines
        let (line, _) = self.line_col_for_offset(offset);
        let line_text: &str = self.content.split('\n').nth(line).unwrap_or("");
        let trimmed = line_text.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            self.completion.visible = false;
            return;
        }

        // Must be >= 2 chars, starting with alphabetic
        if prefix.len() >= 2
            && prefix.bytes().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        {
            self.completion.anchor_offset = prefix_start;
            self.completion.prefix = prefix.to_lowercase();
            self.filter_completions();
            self.completion.visible = !self.completion.filtered.is_empty();
            self.completion.selected_index = 0;
        } else {
            self.completion.visible = false;
        }
    }

    fn filter_completions(&mut self) {
        let prefix = &self.completion.prefix;
        self.completion.filtered = self
            .completion
            .all_candidates
            .iter()
            .filter(|item| item.label.to_lowercase().starts_with(prefix))
            .take(50) // cap filtered list
            .cloned()
            .collect();
    }

    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.completion.visible || self.completion.filtered.is_empty() {
            return;
        }
        let item = self.completion.filtered[self.completion.selected_index].clone();
        let insert_text = if item.category == CompletionCategory::Function {
            format!("{}(", item.label)
        } else {
            item.label
        };

        let anchor = self.completion.anchor_offset;
        let cursor = self.cursor_offset();

        self.push_undo();
        self.content = format!(
            "{}{}{}",
            &self.content[..anchor],
            insert_text,
            &self.content[cursor..],
        );
        let new_pos = anchor + insert_text.len();
        self.selected_range = new_pos..new_pos;
        self.completion.visible = false;
        self.pause_blinking(cx);
        self.fire_change(window, cx);
        cx.notify();
    }

    fn completion_move_up(&mut self, cx: &mut Context<Self>) {
        if self.completion.selected_index > 0 {
            self.completion.selected_index -= 1;
        } else {
            self.completion.selected_index = self.completion.filtered.len().saturating_sub(1);
        }
        cx.notify();
    }

    fn completion_move_down(&mut self, cx: &mut Context<Self>) {
        if self.completion.selected_index + 1 < self.completion.filtered.len() {
            self.completion.selected_index += 1;
        } else {
            self.completion.selected_index = 0;
        }
        cx.notify();
    }

    fn dismiss_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.visible {
            self.completion.visible = false;
            cx.notify();
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, content: String, cx: &mut Context<Self>) {
        self.content = content;
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.select_mode = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.line_layouts.clear();
        self.line_visual_counts.clear();
        self.pause_blinking(cx);
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        let offset = self.cursor_offset();
        let before = &self.content[..offset];
        let line = before.matches('\n').count();
        let last_newline = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = offset - last_newline;
        (line, col)
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(UndoEntry {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
        });
        self.redo_stack.clear();
    }

    fn fire_change(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(cb) = &self.on_change {
            let content = self.content.clone();
            cb(&content, window, cx);
        }
    }

    // --- Line helpers ---

    fn line_count(&self) -> usize {
        self.content.split('\n').count()
    }

    /// Given a byte offset, return (line_index, col_byte_offset)
    fn line_col_for_offset(&self, offset: usize) -> (usize, usize) {
        let before = &self.content[..offset.min(self.content.len())];
        let line = before.matches('\n').count();
        let last_newline = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        (line, offset - last_newline)
    }

    /// Given (line, col), return byte offset
    fn offset_for_line_col(&self, line: usize, col: usize) -> usize {
        let mut offset = 0;
        for (i, l) in self.content.split('\n').enumerate() {
            if i == line {
                return offset + col.min(l.len());
            }
            offset += l.len() + 1;
        }
        self.content.len()
    }

    /// Byte offset of the start of a given line
    fn line_start_offset(&self, line: usize) -> usize {
        self.offset_for_line_col(line, 0)
    }

    /// Byte offset of the end of a given line (before the newline)
    fn line_end_offset(&self, line: usize) -> usize {
        let lines: Vec<&str> = self.content.split('\n').collect();
        if line >= lines.len() {
            return self.content.len();
        }
        self.line_start_offset(line) + lines[line].len()
    }

    fn vertical_offset(&self, direction: isize) -> usize {
        let (line, col) = self.line_col_for_offset(self.cursor_offset());
        let target_line = line.saturating_add_signed(direction);
        if target_line >= self.line_count() {
            self.content.len()
        } else {
            self.offset_for_line_col(target_line, col)
        }
    }

    // --- Navigation actions ---

    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion.visible {
            self.accept_completion(window, cx);
            return;
        }
        self.push_undo();
        let range = self.selected_range.clone();
        self.content = format!(
            "{}\n{}",
            &self.content[..range.start],
            &self.content[range.end..]
        );
        let new_pos = range.start + 1;
        self.selected_range = new_pos..new_pos;
        self.marked_range.take();
        self.pause_blinking(cx);
        self.fire_change(window, cx);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx);
        }
        self.push_undo();
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx);
        }
        self.push_undo();
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion.visible {
            self.completion_move_up(cx);
            return;
        }
        self.move_to(self.vertical_offset(-1), cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion.visible {
            self.completion_move_down(cx);
            return;
        }
        self.move_to(self.vertical_offset(1), cx);
    }

    fn move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(previous_word_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(next_word_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion.visible {
            self.completion_move_down(cx);
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_completion(cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(previous_word_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(next_word_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(-1), cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(1), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _col) = self.line_col_for_offset(self.cursor_offset());
        self.move_to(self.line_start_offset(line), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _col) = self.line_col_for_offset(self.cursor_offset());
        self.move_to(self.line_end_offset(line), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor_offset());
        self.select_to(self.line_start_offset(line), cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor_offset());
        self.select_to(self.line_end_offset(line), cx);
    }

    fn document_start(&mut self, _: &DocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_document_start(&mut self, _: &SelectDocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_document_end(&mut self, _: &SelectDocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.push_undo();
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.push_undo();
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.undo_stack.pop() {
            self.redo_stack.push(UndoEntry {
                content: self.content.clone(),
                selected_range: self.selected_range.clone(),
            });
            self.content = entry.content;
            self.selected_range = entry.selected_range;
            self.pause_blinking(cx);
            self.fire_change(window, cx);
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.redo_stack.pop() {
            self.undo_stack.push(UndoEntry {
                content: self.content.clone(),
                selected_range: self.selected_range.clone(),
            });
            self.content = entry.content;
            self.selected_range = entry.selected_range;
            self.pause_blinking(cx);
            self.fire_change(window, cx);
            cx.notify();
        }
    }

    // --- Mouse handling ---

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let idx = self.index_for_mouse_position(event.position);

        match event.click_count {
            2 => {
                // Double-click: select word
                let word = self.surrounding_word(idx);
                self.selected_range = word.clone();
                self.selection_reversed = false;
                self.select_mode = Some(SelectMode::Word {
                    anchor_start: word.start,
                    anchor_end: word.end,
                });
                self.pause_blinking(cx);
                cx.notify();
            }
            3 => {
                // Triple-click: select line
                let line = self.line_range(idx);
                self.selected_range = line.clone();
                self.selection_reversed = false;
                self.select_mode = Some(SelectMode::Line {
                    anchor_start: line.start,
                    anchor_end: line.end,
                });
                self.pause_blinking(cx);
                cx.notify();
            }
            _ => {
                // Single click
                self.select_mode = Some(SelectMode::Character);
                if event.modifiers.shift {
                    self.select_to(idx, cx);
                } else {
                    self.move_to(idx, cx);
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.select_mode = None;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ref mode) = self.select_mode else { return };
        let idx = self.index_for_mouse_position(event.position);

        match mode {
            SelectMode::Character => {
                self.select_to(idx, cx);
            }
            SelectMode::Word { anchor_start, anchor_end } => {
                let drag_word = self.surrounding_word(idx);
                let (anchor_start, anchor_end) = (*anchor_start, *anchor_end);
                // Extend from anchor word to drag word
                if drag_word.start < anchor_start {
                    self.selected_range = drag_word.start..anchor_end;
                    self.selection_reversed = true;
                } else {
                    self.selected_range = anchor_start..drag_word.end;
                    self.selection_reversed = false;
                }
                self.pause_blinking(cx);
                cx.notify();
            }
            SelectMode::Line { anchor_start, anchor_end } => {
                let drag_line = self.line_range(idx);
                let (anchor_start, anchor_end) = (*anchor_start, *anchor_end);
                if drag_line.start < anchor_start {
                    self.selected_range = drag_line.start..anchor_end;
                    self.selection_reversed = true;
                } else {
                    self.selected_range = anchor_start..drag_line.end;
                    self.selection_reversed = false;
                }
                self.pause_blinking(cx);
                cx.notify();
            }
        }
    }

    // --- Core movement and selection helpers ---

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.completion.visible = false;
        self.pause_blinking(cx);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        (self.selected_range, self.selection_reversed) = extend_selection(
            self.selected_range.clone(),
            self.selection_reversed,
            offset,
        );
        self.pause_blinking(cx);
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    // --- Mouse hit-testing for multi-line ---

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds.as_ref() else {
            return 0;
        };

        if self.content.is_empty() {
            return 0;
        }

        // Determine which logical line was clicked, accounting for wrapped lines
        let y_offset = position.y - bounds.top();
        let lines: Vec<&str> = self.content.split('\n').collect();

        let mut line_idx = 0;
        let mut accumulated_y = px(0.);
        for (i, &count) in self.line_visual_counts.iter().enumerate() {
            let line_total_height = self.line_height * count as f32;
            if y_offset < accumulated_y + line_total_height {
                line_idx = i;
                break;
            }
            accumulated_y += line_total_height;
            line_idx = i + 1;
        }
        let line_idx = line_idx.min(lines.len().saturating_sub(1));

        // Find byte offset within line using wrapped line layout
        let col: usize = if let Some(Some(wrapped)) = self.line_layouts.get(line_idx) {
            let local_x = position.x - bounds.left();
            let local_y = y_offset - accumulated_y;
            let local_pos = point(local_x, local_y);
            match wrapped.closest_index_for_position(local_pos, self.line_height) {
                Ok(idx) | Err(idx) => idx,
            }
        } else {
            0
        };

        self.offset_for_line_col(line_idx, col)
    }

    // --- UTF-16 conversion helpers (for IME) ---

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    // --- Syntax highlighting ---

    fn highlight_line(&self, line: &str, style: &TextRun) -> Vec<TextRun> {
        if line.is_empty() {
            return vec![TextRun {
                len: 0,
                ..style.clone()
            }];
        }

        let mut lexer = Lexer::new(line, &self.unit_table, &self.currency_table)
            .with_number_format(self.number_format);
        let tokens = lexer.tokenize();

        let mut runs = Vec::new();
        let mut last_end = 0usize;

        for tok in &tokens {
            let start = tok.span.start;
            let end = tok.span.end;
            if start > last_end {
                // Gap -- whitespace or unknown chars, use default text color
                runs.push(TextRun {
                    len: start - last_end,
                    color: self.theme.text,
                    ..style.clone()
                });
            }
            if end > start {
                let color = self.color_for_token(&tok.kind);
                runs.push(TextRun {
                    len: end - start,
                    color,
                    ..style.clone()
                });
            }
            last_end = end;
        }

        // If no runs were generated, fallback to full-line default
        if runs.is_empty() {
            runs.push(TextRun {
                len: line.len(),
                ..style.clone()
            });
        }

        // Verify total length matches
        let total: usize = runs.iter().map(|r| r.len).sum();
        if total < line.len() {
            runs.push(TextRun {
                len: line.len() - total,
                color: self.theme.text,
                ..style.clone()
            });
        }

        runs
    }

    fn color_for_token(&self, kind: &TokenKind) -> gpui::Hsla {
        match kind {
            TokenKind::Number(_) | TokenKind::NumberRepr(_, _) => self.theme.syn_number,
            TokenKind::Op(_) => self.theme.syn_operator,
            TokenKind::LParen | TokenKind::RParen => self.theme.syn_operator,
            TokenKind::Percent => self.theme.syn_percent,
            TokenKind::Assign | TokenKind::CompoundAssign(_) => self.theme.syn_operator,
            TokenKind::Convert => self.theme.syn_keyword,
            TokenKind::Of | TokenKind::From | TokenKind::On | TokenKind::Off => {
                self.theme.syn_keyword
            }
            TokenKind::AsAPctOf | TokenKind::AsAPctOn | TokenKind::AsAPctOff => {
                self.theme.syn_keyword
            }
            TokenKind::OfWhatIs | TokenKind::OnWhatIs | TokenKind::OffWhatIs => {
                self.theme.syn_keyword
            }
            TokenKind::Func(_) => self.theme.syn_function,
            TokenKind::Unit(_) | TokenKind::CompoundUnitShorthand(_, _) => self.theme.syn_unit,
            TokenKind::Currency(_) => self.theme.syn_currency,
            TokenKind::CurrencySymbol(_) => self.theme.text, // $ € £ etc blend with numbers
            TokenKind::Scale(_) => self.theme.syn_scale,
            TokenKind::Repr(_) => self.theme.syn_keyword,
            TokenKind::Ident(_) => self.theme.syn_variable,
            TokenKind::Agg(_) => self.theme.syn_function,
            TokenKind::Comment => self.theme.syn_comment,
            TokenKind::Header => self.theme.syn_header,
            TokenKind::Label(_) => self.theme.syn_label,
            TokenKind::Eof => self.theme.text,
        }
    }
}

// --- EntityInputHandler (IME support) ---

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        );
        let new_pos = range.start + new_text.len();
        self.selected_range = new_pos..new_pos;
        self.marked_range.take();
        self.pause_blinking(cx);
        self.fire_change(window, cx);
        self.update_completion_state();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        );
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| {
                let pos = range.start + new_text.len();
                pos..pos
            });
        self.fire_change(window, cx);
        self.update_completion_state();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        // Determine which line this range start falls on
        let (line, col_start) = self.line_col_for_offset(range.start);
        let (_, col_end) = self.line_col_for_offset(range.end);
        let wrapped = self.line_layouts.get(line)?.as_ref()?;

        // Compute the y offset accounting for wrapped lines above
        let y_base: Pixels = self.line_visual_counts.iter().take(line).map(|&c| self.line_height * c as f32).sum();

        let start_pos = wrapped.position_for_index(col_start, self.line_height)?;
        let end_pos = wrapped.position_for_index(col_end, self.line_height)?;

        Some(Bounds::from_corners(
            point(bounds.left() + start_pos.x, bounds.top() + y_base + start_pos.y),
            point(
                bounds.left() + end_pos.x,
                bounds.top() + y_base + end_pos.y + self.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds.as_ref()?;
        let y_offset = point.y - bounds.top();
        let lines: Vec<&str> = self.content.split('\n').collect();

        // Find logical line accounting for wrapping
        let mut line_idx = 0;
        let mut accumulated_y = px(0.);
        for (i, &count) in self.line_visual_counts.iter().enumerate() {
            let line_total_height = self.line_height * count as f32;
            if y_offset < accumulated_y + line_total_height {
                line_idx = i;
                break;
            }
            accumulated_y += line_total_height;
            line_idx = i + 1;
        }
        let line_idx = line_idx.min(lines.len().saturating_sub(1));

        let wrapped = self.line_layouts.get(line_idx)?.as_ref()?;
        let local_x = point.x - bounds.left();
        let local_y = y_offset - accumulated_y;
        let local_pos = gpui::point(local_x, local_y);
        let col = match wrapped.closest_index_for_position(local_pos, self.line_height) {
            Ok(idx) | Err(idx) => idx,
        };
        let utf8_offset = self.offset_for_line_col(line_idx, col);
        Some(self.offset_to_utf16(utf8_offset))
    }
}

// --- EditorLineElement: custom Element for each line ---

pub struct EditorLineElement {
    editor: Entity<Editor>,
    line_index: usize,
}

pub struct EditorLinePrepaint {
    wrapped: Option<WrappedLine>,
    visual_lines: usize,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    cursor_visible: bool,
}

impl IntoElement for EditorLineElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorLineElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorLinePrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let editor = self.editor.read(cx);
        let lh = editor.line_height;

        // Use cached visual line count from previous frame (1-frame delay is imperceptible)
        let visual_lines = editor.line_visual_counts
            .get(self.line_index)
            .copied()
            .unwrap_or(1);

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (lh * visual_lines as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Extract everything we need from the editor, then drop the borrow
        let (line_text_owned, theme_text, selected_range, cursor_off, line_start, marked_range, lh, cursor_visible) = {
            let editor = self.editor.read(cx);
            let lines: Vec<&str> = editor.content.split('\n').collect();
            let lt = lines.get(self.line_index).copied().unwrap_or("").to_string();
            let ls = editor.line_start_offset(self.line_index);
            (
                lt,
                editor.theme.text,
                editor.selected_range.clone(),
                editor.cursor_offset(),
                ls,
                editor.marked_range.clone(),
                editor.line_height,
                editor.cursor_visible,
            )
        };

        let ws = window.text_style();
        let is_header = line_text_owned.starts_with('#');
        let font_size = if is_header {
            ws.font_size.to_pixels(window.rem_size()) * 1.05
        } else {
            ws.font_size.to_pixels(window.rem_size())
        };

        let mut base_font = ws.font();
        if is_header {
            base_font.weight = FontWeight::BOLD;
        }

        let base_run = TextRun {
            len: 0,
            font: base_font,
            color: theme_text,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let display_text: SharedString = if line_text_owned.is_empty() {
            " ".into()
        } else {
            SharedString::from(line_text_owned.clone())
        };

        let runs = if line_text_owned.is_empty() {
            vec![TextRun {
                len: 1,
                ..base_run.clone()
            }]
        } else {
            let editor = self.editor.read(cx);
            let mut runs = editor.highlight_line(&line_text_owned, &base_run);
            let _ = editor;

            // Apply marked_range underline if on this line
            if let Some(marked) = &marked_range {
                let line_end = line_start + line_text_owned.len();
                let m_start = marked.start.max(line_start);
                let m_end = marked.end.min(line_end);
                if m_start < m_end {
                    let local_start = m_start - line_start;
                    let local_end = m_end - line_start;
                    let mut new_runs = Vec::new();
                    let mut pos = 0;
                    for run in &runs {
                        let run_end = pos + run.len;
                        let overlap_start = local_start.max(pos);
                        let overlap_end = local_end.min(run_end);
                        if overlap_start < overlap_end {
                            if pos < overlap_start {
                                new_runs.push(TextRun {
                                    len: overlap_start - pos,
                                    ..run.clone()
                                });
                            }
                            new_runs.push(TextRun {
                                len: overlap_end - overlap_start,
                                underline: Some(UnderlineStyle {
                                    color: Some(run.color),
                                    thickness: px(1.0),
                                    wavy: false,
                                }),
                                ..run.clone()
                            });
                            if overlap_end < run_end {
                                new_runs.push(TextRun {
                                    len: run_end - overlap_end,
                                    ..run.clone()
                                });
                            }
                        } else {
                            new_runs.push(run.clone());
                        }
                        pos = run_end;
                    }
                    runs = new_runs;
                }
            }
            runs
        };

        let wrap_width = bounds.size.width;
        let wrapped_lines = window
            .text_system()
            .shape_text(display_text.clone(), font_size, &runs, Some(wrap_width), None)
            .unwrap_or_else(|_| {
                // Fallback: shape without wrapping
                window
                    .text_system()
                    .shape_text(display_text, font_size, &runs, None, None)
                    .unwrap_or_default()
            });
        let wrapped = wrapped_lines.into_iter().next().unwrap_or_default();
        let visual_lines = wrapped.wrap_boundaries().len() + 1;

        // Compute selection and cursor for this line
        let line_end = line_start + line_text_owned.len();
        let cursor_color = {
            let editor = self.editor.read(cx);
            editor.theme.cursor
        };
        let selection_color = {
            let editor = self.editor.read(cx);
            editor.theme.selection
        };

        let (selection, cursor) = if selected_range.is_empty() {
            // Cursor
            let cursor_q = if cursor_off >= line_start && cursor_off <= line_end {
                let local_col = cursor_off - line_start;
                if let Some(pos) = wrapped.position_for_index(local_col, lh) {
                    Some(fill(
                        Bounds::new(
                            point(bounds.left() + pos.x + px(2.), bounds.top() + pos.y),
                            size(px(2.), lh),
                        ),
                        cursor_color,
                    ))
                } else {
                    // Fallback: place cursor at start
                    Some(fill(
                        Bounds::new(
                            point(bounds.left(), bounds.top()),
                            size(px(2.), lh),
                        ),
                        cursor_color,
                    ))
                }
            } else {
                None
            };
            (None, cursor_q)
        } else {
            // Selection — draw a simple rect from start to end position
            let sel_start = selected_range.start.max(line_start);
            let sel_end = selected_range.end.min(line_end + 1);
            if sel_start <= line_end && sel_end > line_start {
                let local_start = sel_start.saturating_sub(line_start);
                let local_end = (sel_end - line_start).min(line_text_owned.len());
                let start_pos = wrapped.position_for_index(local_start, lh);
                let end_pos = wrapped.position_for_index(local_end, lh);

                match (start_pos, end_pos) {
                    (Some(sp), Some(ep)) => {
                        if sp.y == ep.y {
                            // Selection on the same visual line
                            let x_end = if sel_end > line_end {
                                bounds.size.width
                            } else {
                                ep.x
                            };
                            (
                                Some(fill(
                                    Bounds::from_corners(
                                        point(bounds.left() + sp.x, bounds.top() + sp.y),
                                        point(bounds.left() + x_end, bounds.top() + sp.y + lh),
                                    ),
                                    selection_color,
                                )),
                                None,
                            )
                        } else {
                            // Selection spans multiple visual lines — draw from start to
                            // end of the wrapped area as a simple rectangle for now
                            (
                                Some(fill(
                                    Bounds::from_corners(
                                        point(bounds.left(), bounds.top() + sp.y),
                                        point(
                                            bounds.left() + bounds.size.width,
                                            bounds.top() + ep.y + lh,
                                        ),
                                    ),
                                    selection_color,
                                )),
                                None,
                            )
                        }
                    }
                    _ => (None, None),
                }
            } else {
                (None, None)
            }
        };

        EditorLinePrepaint {
            wrapped: Some(wrapped),
            visual_lines,
            cursor,
            selection,
            cursor_visible,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Read out data we need before mutable borrows
        let (focus_handle, editor_bounds, lh, is_focused) = {
            let editor = self.editor.read(cx);
            (
                editor.focus_handle.clone(),
                editor.last_bounds.unwrap_or(bounds),
                editor.line_height,
                editor.focus_handle.is_focused(window),
            )
        };

        // Register input handler on the first line element
        if self.line_index == 0 {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(editor_bounds, self.editor.clone()),
                cx,
            );
        }

        // Active line highlight
        // Active line highlight is now drawn at the row level in render()
        // to include the gutter area

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let wrapped = prepaint.wrapped.take().unwrap();
        let visual_lines = prepaint.visual_lines;
        wrapped.paint(
            bounds.origin,
            lh,
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .unwrap();

        if is_focused
            && prepaint.cursor_visible
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        // Store the wrapped line layout, visual line count, and update editor bounds
        self.editor.update(cx, |editor, _| {
            while editor.line_layouts.len() <= self.line_index {
                editor.line_layouts.push(None);
            }
            editor.line_layouts[self.line_index] = Some(wrapped);

            while editor.line_visual_counts.len() <= self.line_index {
                editor.line_visual_counts.push(1);
            }
            editor.line_visual_counts[self.line_index] = visual_lines;

            // First line sets the editor origin for mouse hit testing
            if self.line_index == 0 {
                editor.last_bounds = Some(bounds);
            }
        });
    }
}

// --- Render implementation ---

impl Editor {
    /// Compute the popup position for the completion menu (window coordinates).
    /// Uses previous-frame cached layout data (same pattern as line_visual_counts).
    fn completion_popup_position(&self) -> Option<Point<Pixels>> {
        let bounds = self.last_bounds.as_ref()?;
        let (line, col) = self.line_col_for_offset(self.completion.anchor_offset);
        let wrapped = self.line_layouts.get(line)?.as_ref()?;

        let y_base: Pixels = self
            .line_visual_counts
            .iter()
            .take(line)
            .map(|&c| self.line_height * c as f32)
            .sum();

        let pos = wrapped.position_for_index(col, self.line_height)?;

        Some(point(
            bounds.left() + pos.x,
            bounds.top() + y_base + pos.y + self.line_height,
        ))
    }

    fn render_completion_list(&self, entity: &Entity<Self>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let max_visible: usize = 8;
        let items = &self.completion.filtered;
        let selected = self.completion.selected_index;
        let bg = self.theme.editor_background;
        let border_color = self.theme.text_dimmed;
        let text_color = self.theme.text;
        let sel_bg = self.theme.divider;

        let mut list = div()
            .id("completion-popup")
            .flex()
            .flex_col()
            .w(px(260.))
            .bg(bg)
            .border_1()
            .border_color(border_color)
            .rounded(px(6.))
            .py(px(2.))
            .occlude()
            .on_mouse_down_out(cx.listener(|editor, _: &MouseDownEvent, _window, cx| {
                editor.completion.visible = false;
                cx.notify();
            }));

        for (i, item) in items.iter().take(max_visible).enumerate() {
            let is_selected = i == selected;
            let category_color = match item.category {
                CompletionCategory::Function => self.theme.syn_function,
                CompletionCategory::Unit => self.theme.syn_unit,
                CompletionCategory::Currency => self.theme.syn_currency,
                CompletionCategory::Keyword => self.theme.syn_keyword,
                CompletionCategory::Scale => self.theme.syn_scale,
                CompletionCategory::Constant => self.theme.syn_number,
                CompletionCategory::Variable => self.theme.syn_variable,
                CompletionCategory::Aggregation => self.theme.syn_function,
            };
            let label = item.label.clone();
            let cat_label = item.category.label().to_string();
            let entity_clone = entity.clone();

            list = list.child(
                div()
                    .id(ElementId::Name(format!("c-{i}").into()))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px(px(8.))
                    .py(px(3.))
                    .text_size(self.font_size * 0.85)
                    .font_family(self.font_family.clone())
                    .when(is_selected, |el| el.bg(sel_bg))
                    .cursor(CursorStyle::PointingHand)
                    .hover(|s| s.bg(sel_bg))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                            entity_clone.update(cx, |editor, cx| {
                                editor.completion.selected_index = i;
                                editor.accept_completion(window, cx);
                            });
                        },
                    )
                    .child(div().text_color(text_color).child(label))
                    .child(
                        div()
                            .text_color(category_color)
                            .text_size(self.font_size * 0.7)
                            .child(cat_label),
                    ),
            );
        }

        list.into_any_element()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line_count = self.line_count();
        self.line_height = window.line_height();

        // Pre-compute completion popup position BEFORE clearing line_layouts
        // (uses previous-frame cached data, same pattern as line_visual_counts)
        let show_completion =
            self.completion.visible && !self.completion.filtered.is_empty();
        let completion_position = if show_completion {
            self.completion_popup_position()
        } else {
            None
        };

        self.line_layouts.clear();
        // Preserve line_visual_counts from previous frame (used by request_layout for height hints)
        // They will be overwritten during paint of each line

        let entity = cx.entity().clone();
        let completion_element = if show_completion {
            Some(self.render_completion_list(&entity, cx))
        } else {
            None
        };

        div()
            .flex()
            .flex_col()
            .key_context("Editor")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::document_start))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_document_start))
            .on_action(cx.listener(Self::select_document_end))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .size_full()
            .bg(self.theme.editor_background)
            .p(self.font_size * 0.75)
            .font_family(self.font_family.clone())
            .text_size(self.font_size)
            .text_color(self.theme.text)
            .children({
                let gutter_w = self.font_size * GUTTER_WIDTH_FACTOR;
                let gutter_pad = self.font_size * GUTTER_PADDING_FACTOR;
                let dimmed = self.theme.text_dimmed;
                let error_color = self.theme.error;
                let lh = self.line_height;
                let visual_counts = &self.line_visual_counts;
                let mut children: Vec<gpui::AnyElement> = Vec::new();
                for i in 0..line_count {
                    // Use cached visual line count from previous frame
                    let visual_lines = visual_counts.get(i).copied().unwrap_or(1);
                    let row_height = lh * visual_lines as f32;
                    // Row: line number gutter + editor line
                    let is_cursor_line = {
                        let offset = self.cursor_offset();
                        let line_start = self.line_start_offset(i);
                        let lines: Vec<&str> = self.content.split('\n').collect();
                        let line_end = line_start + lines.get(i).map(|l| l.len()).unwrap_or(0);
                        offset >= line_start && offset <= line_end
                    };
                    let row_bg = if is_cursor_line && self.focus_handle.is_focused(window) {
                        Some(gpui::hsla(0.0, 0.0, 1.0, 0.03))
                    } else {
                        None
                    };
                    children.push(
                        div()
                            .flex()
                            .flex_row()
                            .w_full()
                            .when_some(row_bg, |el, bg| el.bg(bg))
                            .child(
                                // Line number gutter — first visual line, baseline-aligned
                                div()
                                    .w(gutter_w)
                                    .h(row_height)
                                    .flex_shrink_0()
                                    .flex()
                                    .items_start()
                                    .justify_end()
                                    .pr(gutter_pad)
                                    .child(
                                        div()
                                            .h(lh)
                                            .flex()
                                            .items_center()
                                            .text_size(self.font_size * 0.7)
                                            .line_height(lh)
                                            .text_color(dimmed)
                                            .child(format!("{}", i + 1))
                                    ),
                            )
                            .child(
                                // The editor line itself
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(EditorLineElement {
                                        editor: entity.clone(),
                                        line_index: i,
                                    }),
                            )
                            .into_any_element(),
                    );
                    // Inlay diagnostic below error lines
                    if self.show_diagnostics {
                        if let Some(Some(diag)) = self.diagnostics.get(i) {
                            children.push(
                                div()
                                    .w_full()
                                    .pl(gutter_w + gutter_pad)
                                    .py(px(2.))
                                    .text_size(self.font_size * 0.75)
                                    .text_color(error_color)
                                    .child(diag.clone())
                                    .into_any_element(),
                            );
                        }
                    }
                }
                children
            })
            // Completion popup (floating, rendered above content)
            .when_some(completion_element, |el, popup| {
                let pos = completion_position.unwrap_or(point(px(0.), px(0.)));
                el.child(
                    deferred(
                        anchored()
                            .anchor(Corner::TopLeft)
                            .position(pos)
                            .snap_to_window_with_margin(px(8.))
                            .child(popup),
                    )
                    .with_priority(10),
                )
            })
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{extend_selection, next_word_boundary, previous_word_boundary};

    #[test]
    fn word_boundaries_are_unicode_aware() {
        let text = "café 世界 résumé";

        assert_eq!(next_word_boundary(text, 0), "café".len());
        assert_eq!(next_word_boundary(text, "café".len()), "café 世".len());
        assert_eq!(previous_word_boundary(text, text.len()), "café 世界 ".len());
        assert_eq!(previous_word_boundary(text, "café 世界".len()), "café 世".len());
    }

    #[test]
    fn word_boundaries_skip_separators() {
        let text = "one,  two";

        assert_eq!(next_word_boundary(text, 0), 3);
        assert_eq!(next_word_boundary(text, 3), text.len());
        assert_eq!(previous_word_boundary(text, text.len()), 6);
        assert_eq!(previous_word_boundary(text, 6), 0);
    }

    #[test]
    fn extending_selection_keeps_anchor_when_crossing_it() {
        let (range, reversed) = extend_selection(4..4, false, 8);
        assert_eq!(range, 4..8);
        assert!(!reversed);

        let (range, reversed) = extend_selection(range, reversed, 2);
        assert_eq!(range, 2..4);
        assert!(reversed);

        let (range, reversed) = extend_selection(range, reversed, 7);
        assert_eq!(range, 4..7);
        assert!(!reversed);
    }
}

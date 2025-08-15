use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::popup_consts::MAX_POPUP_ROWS;
use crate::bottom_pane::scroll_state::ScrollState;
use crate::bottom_pane::selection_popup_common::GenericDisplayRow;
use crate::bottom_pane::selection_popup_common::render_rows;
use codex_common::fuzzy_match::fuzzy_match;
use codex_core::config::Config;
use codex_core::models::ResponseItem;
use serde_json::Value;

/// Popup for selecting a previous session (rollout file) to resume.
pub(crate) struct SessionResumePopup {
    /// All entries for the current cwd.
    entries: Vec<SessionEntry>,
    /// Filtered view: each item is (index into entries, optional match indices, score).
    filtered: Vec<(usize, Option<Vec<usize>>, i32)>,
    /// Current fuzzy filter text.
    filter: String,
    state: ScrollState,
    done: bool,
    app_event_tx: AppEventSender,
}

struct SessionEntry {
    path: PathBuf,
    label: String,
}

impl SessionResumePopup {
    pub fn new(config: &Config, app_event_tx: AppEventSender) -> Self {
        let entries = collect_sessions(config, 500);
        let mut popup = Self {
            entries,
            filtered: Vec::new(),
            filter: String::new(),
            state: ScrollState::new(),
            done: false,
            app_event_tx,
        };
        popup.recompute_filtered();
        if !popup.filtered.is_empty() {
            popup.state.selected_idx = Some(0);
            popup
                .state
                .ensure_visible(popup.filtered.len(), MAX_POPUP_ROWS.min(popup.filtered.len()));
        }
        popup
    }

    fn recompute_filtered(&mut self) {
        let filter = self.filter.trim();
        let mut out: Vec<(usize, Option<Vec<usize>>, i32)> = Vec::new();
        if filter.is_empty() {
            for (idx, _) in self.entries.iter().enumerate() {
                out.push((idx, None, 0));
            }
        } else {
            for (idx, e) in self.entries.iter().enumerate() {
                if let Some((indices, score)) = fuzzy_match(&e.label, filter) {
                    out.push((idx, Some(indices), score));
                }
            }
            // Sort by score then stable by label
            out.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| self.entries[a.0].label.cmp(&self.entries[b.0].label)));
        }
        self.filtered = out;
        // Clamp selection to new filtered length
        let len = self.filtered.len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    fn move_selection_by(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 { return; }
        let cur = self.state.selected_idx.unwrap_or(0) as isize;
        let mut next = cur + delta;
        if next < 0 { next = 0; }
        if next as usize >= len { next = (len - 1) as isize; }
        self.state.selected_idx = Some(next as usize);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    #[inline]
    fn page_size(&self) -> usize {
        let len = self.filtered.len();
        let page = MAX_POPUP_ROWS.min(len);
        page.max(1)
    }

    #[inline]
    fn half_page_size(&self) -> usize {
        let p = self.page_size();
        std::cmp::max(1, p / 2)
    }

    fn selected_entry_path(&self) -> Option<PathBuf> {
        let sel = self.state.selected_idx?;
        let (idx, _, _) = *self.filtered.get(sel)?;
        self.entries.get(idx).map(|e| e.path.clone())
    }
}

impl<'a> BottomPaneView<'a> for SessionResumePopup {
    fn handle_key_event(&mut self, _pane: &mut BottomPane<'a>, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Up => self.move_selection_by(-1),
            KeyCode::Down => self.move_selection_by(1),
            KeyCode::PageUp => {
                let amt = self.page_size() as isize;
                self.move_selection_by(-amt);
            }
            KeyCode::PageDown => {
                let amt = self.page_size() as isize;
                self.move_selection_by(amt);
            }
            KeyCode::Home => {
                self.state.selected_idx = Some(0);
                let len = self.filtered.len();
                self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
            }
            KeyCode::End => {
                let len = self.filtered.len();
                if len > 0 {
                    self.state.selected_idx = Some(len - 1);
                    self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
                }
            }
            KeyCode::Enter => {
                if let Some(path) = self.selected_entry_path() {
                    self.app_event_tx.send(AppEvent::ResumeSession(path));
                    self.done = true;
                }
            }
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    self.done = true;
                } else {
                    self.filter.clear();
                    self.recompute_filtered();
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.recompute_filtered();
            }
            KeyCode::Char(ch) => {
                // Vim-style half-page: Ctrl-D (down), Ctrl-U (up)
                if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                    match ch {
                        'd' | 'D' => {
                            let amt = self.half_page_size() as isize;
                            self.move_selection_by(amt);
                        }
                        'u' | 'U' => {
                            let amt = self.half_page_size() as isize;
                            self.move_selection_by(-amt);
                        }
                        'f' | 'F' => {
                            let amt = self.page_size() as isize;
                            self.move_selection_by(amt);
                        }
                        'b' | 'B' => {
                            let amt = self.page_size() as isize;
                            self.move_selection_by(-amt);
                        }
                        _ => {}
                    }
                } else {
                    self.filter.push(ch);
                    self.recompute_filtered();
                }
            }
            _ => {}
        }
    }

    fn on_ctrl_c(&mut self, _pane: &mut BottomPane<'a>) -> CancellationEvent {
        self.done = true;
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.done
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // +1 for the filter header line
        (self.filtered.len().clamp(1, MAX_POPUP_ROWS) as u16).saturating_add(1)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        // Header line: show current filter text
        if area.height > 0 {
            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::styled(" filter: ", Style::default().add_modifier(Modifier::DIM)));
            if self.filter.is_empty() {
                spans.push(Span::styled("(type to search)", Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)));
            } else {
                spans.push(Span::raw(self.filter.clone()));
            }
            let header = Line::from(spans);
            // Draw header into the first row
            let mut x = area.x;
            for span in header.spans {
                for ch in span.content.chars() {
                    if x >= area.right() { break; }
                    if let Some(cell) = buf.cell_mut((x, area.y)) {
                        let s = ch.to_string();
                        cell.set_symbol(&s);
                        cell.set_style(span.style);
                    }
                    x += 1;
                }
                if x >= area.right() { break; }
            }
        }

        // Content area begins after header
        let content_area = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(1),
        };

        let rows: Vec<GenericDisplayRow> = if self.filtered.is_empty() {
            Vec::new()
        } else {
            self
                .filtered
                .iter()
                .enumerate()
                .map(|(row_idx, (orig_idx, indices, _))| {
                    let e = &self.entries[*orig_idx];
                    GenericDisplayRow {
                        name: e.label.clone(),
                        match_indices: indices.clone(),
                        is_current: self.state.selected_idx == Some(row_idx),
                        description: None,
                    }
                })
                .collect()
        };
        render_rows(content_area, buf, &rows, &self.state, MAX_POPUP_ROWS);
    }
}

fn collect_sessions(config: &Config, max_entries: usize) -> Vec<SessionEntry> {
    let mut root = config.codex_home.clone();
    root.push("sessions");
    let mut items: Vec<(SystemTime, PathBuf)> = Vec::new();

    let Ok(years) = fs::read_dir(&root) else {
        return Vec::new();
    };
    for y in years.flatten() {
        if let Ok(ft) = y.file_type() {
            if !ft.is_dir() {
                continue;
            }
        }
        let Ok(mons) = fs::read_dir(y.path()) else {
            continue;
        };
        for m in mons.flatten() {
            if let Ok(ft) = m.file_type() {
                if !ft.is_dir() {
                    continue;
                }
            }
            let Ok(days) = fs::read_dir(m.path()) else {
                continue;
            };
            for d in days.flatten() {
                if let Ok(files) = fs::read_dir(d.path()) {
                    for f in files.flatten() {
                        let fpath = f.path();
                        let is_rollout = fpath
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
                            .unwrap_or(false);
                        if !is_rollout {
                            continue;
                        }
                        let mt = f
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        items.push((mt, fpath));
                    }
                }
            }
        }
    }

    items.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let mut out: Vec<SessionEntry> = Vec::new();
    for (_t, path) in items.into_iter() {
        let meta = extract_meta(&path);
        let cwd = extract_cwd(&path);
        // only include sessions with the same cwd
        if let Some(cwd_str) = cwd.as_deref() {
            if cwd_str != config.cwd.to_string_lossy() {
                continue;
            }
        } else {
            continue;
        }
        let title = extract_title(&path);
        let label = build_label(&path, meta.as_ref(), cwd.as_deref(), title.as_deref());
        out.push(SessionEntry { label, path });
        if out.len() >= max_entries {
            break;
        }
    }
    out
}

fn label_for_path(path: &PathBuf) -> String {
    // Prefer file name (includes timestamp + uuid). Show parent date folder as context.
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rollout.jsonl");
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{name}  ({parent})")
    }
}

fn build_label(
    path: &PathBuf,
    meta: Option<&(String, Option<String>)>,
    cwd: Option<&str>,
    title: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((ts, branch)) = meta {
        let ts_short = ts.chars().take(19).collect::<String>();
        parts.push(ts_short);
        if let Some(b) = branch {
            if !b.is_empty() {
                parts.push(format!("branch:{b}"));
            }
        }
    }
    if let Some(t) = title {
        parts.push(truncate_title(t, 80));
    }
    if let Some(c) = cwd {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        label_for_path(path)
    } else {
        parts.join("  •  ")
    }
}

fn truncate_title(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::with_capacity(max + 1);
    for (i, ch) in s.chars().enumerate() {
        if i >= max.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn extract_title(path: &PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let _ = lines.next(); // skip meta
    // Collect candidates, preferring user messages that look like prompts.
    let mut user_candidates: Vec<String> = Vec::new();
    let mut assistant_candidates: Vec<String> = Vec::new();
    for line in lines {
        if line.trim().is_empty() { continue; }
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        let item: ResponseItem = match serde_json::from_value(v) { Ok(i) => i, Err(_) => continue };
        if let ResponseItem::Message { role, content, .. } = item {
            let mut acc = String::new();
            for c in content {
                match c {
                    codex_core::models::ContentItem::OutputText { text }
                    | codex_core::models::ContentItem::InputText { text } => {
                        // Skip environment context messages
                        if text.contains("<environment_context>") { continue; }
                        if !acc.is_empty() { acc.push(' '); }
                        acc.push_str(&text);
                    }
                    _ => {}
                }
            }
            let acc = acc.trim();
            if acc.is_empty() { continue; }
            // Ignore slash-commands and boilerplate "ok"/"thanks" titles
            let looks_like_command = acc.starts_with('/');
            let too_short = acc.split_whitespace().take(3).count() < 3;
            if role == "user" && !looks_like_command && !too_short {
                user_candidates.push(acc.to_string());
            } else if role == "assistant" && !too_short {
                assistant_candidates.push(acc.to_string());
            }
        }
    }
    // Choose best user candidate; fallback to assistant.
    if let Some(t) = user_candidates.into_iter().next() {
        return Some(summarize_title(&t));
    }
    if let Some(t) = assistant_candidates.into_iter().next() {
        return Some(summarize_title(&t));
    }
    None
}

fn summarize_title(s: &str) -> String {
    // Prefer first line
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.is_empty() { return String::new(); }
    // Prefer up to the first sentence end punctuation.
    for sep in ['.', '!', '?', ':'] {
        if let Some(idx) = first_line.find(sep) {
            let snippet = &first_line[..=idx];
            return snippet.trim().to_string();
        }
    }
    // Otherwise, take first ~12 words
    let mut out = String::new();
    for (i, w) in first_line.split_whitespace().enumerate() {
        if i >= 12 { break; }
        if !out.is_empty() { out.push(' '); }
        out.push_str(w);
    }
    out
}

fn extract_meta(path: &PathBuf) -> Option<(String, Option<String>)> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let meta_line = lines.next()?;
    let v: Value = serde_json::from_str(meta_line).ok()?;
    // The header is flattened: { "id":..., "timestamp":..., "instructions":..., "git": {...} }
    let ts = v.get("timestamp")?.as_str()?.to_string();
    let branch = v.get("git")
        .and_then(|g| g.get("branch"))
        .and_then(|b| b.as_str())
        .map(|s| s.to_string());
    Some((ts, branch))
}

fn extract_cwd(path: &PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().skip(1) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let item: ResponseItem = match serde_json::from_value(v) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if let ResponseItem::Message { content, .. } = item {
            for c in content {
                match c {
                    codex_core::models::ContentItem::InputText { text }
                    | codex_core::models::ContentItem::OutputText { text } => {
                        if let Some(idx) = text.find("Current working directory:") {
                            let after = &text[idx..];
                            if let Some(colon) = after.find(':') {
                                let rest = after[colon + 1..].trim();
                                let line_end = rest.find('\n').unwrap_or(rest.len());
                                let cwd_str = rest[..line_end].trim();
                                return Some(cwd_str.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

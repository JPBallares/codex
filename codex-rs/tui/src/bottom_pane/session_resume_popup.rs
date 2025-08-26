use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
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
use codex_protocol::models::ResponseItem;
use serde_json::Value;

// LRU cache for extracted titles to avoid re-reading unchanged files and prevent unbounded growth
struct TitleCache {
    map: HashMap<PathBuf, (SystemTime, String)>,
    order: VecDeque<PathBuf>,
    capacity: usize,
}

impl TitleCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn touch(&mut self, key: &PathBuf) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_front(key.clone());
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > self.capacity {
            if let Some(lru) = self.order.pop_back() {
                self.map.remove(&lru);
            } else {
                break;
            }
        }
    }

    fn get_if_fresh(&mut self, key: &PathBuf, modified: SystemTime) -> Option<String> {
        if let Some((seen_mtime, title)) = self.map.get(key)
            && *seen_mtime >= modified
        {
            // Clone first to end the immutable borrow before mutating
            let out = title.clone();
            self.touch(key);
            return Some(out);
        }
        None
    }

    fn put(&mut self, key: PathBuf, modified: SystemTime, title: String) {
        self.map.insert(key.clone(), (modified, title));
        self.touch(&key);
        self.evict_if_needed();
    }
}

// Global title cache with LRU eviction
static TITLE_CACHE: OnceLock<Mutex<TitleCache>> = OnceLock::new();

fn get_title_cache() -> &'static Mutex<TitleCache> {
    // Default to a conservative cap to avoid UI stalls and memory growth.
    // This can be tuned based on usage patterns.
    TITLE_CACHE.get_or_init(|| Mutex::new(TitleCache::new(512)))
}

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
            popup.state.ensure_visible(
                popup.filtered.len(),
                MAX_POPUP_ROWS.min(popup.filtered.len()),
            );
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
            out.sort_by(|a, b| {
                a.2.cmp(&b.2)
                    .then_with(|| self.entries[a.0].label.cmp(&self.entries[b.0].label))
            });
        }
        self.filtered = out;
        // Clamp selection to new filtered length
        let len = self.filtered.len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    fn move_selection_by(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let cur = self.state.selected_idx.unwrap_or(0) as isize;
        let mut next = cur + delta;
        if next < 0 {
            next = 0;
        }
        if next as usize >= len {
            next = (len - 1) as isize;
        }
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

impl BottomPaneView for SessionResumePopup {
    fn handle_key_event(&mut self, _pane: &mut BottomPane, key_event: KeyEvent) {
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

    fn on_ctrl_c(&mut self, _pane: &mut BottomPane) -> CancellationEvent {
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
            spans.push(Span::styled(
                " filter: ",
                Style::default().add_modifier(Modifier::DIM),
            ));
            if self.filter.is_empty() {
                spans.push(Span::styled(
                    "(type to search)",
                    Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
                ));
            } else {
                spans.push(Span::raw(self.filter.clone()));
            }
            let header = Line::from(spans);
            // Draw header into the first row
            let mut x = area.x;
            for span in header.spans {
                for ch in span.content.chars() {
                    if x >= area.right() {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((x, area.y)) {
                        let s = ch.to_string();
                        cell.set_symbol(&s);
                        cell.set_style(span.style);
                    }
                    x += 1;
                }
                if x >= area.right() {
                    break;
                }
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
            self.filtered
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
        render_rows(content_area, buf, &rows, &self.state, MAX_POPUP_ROWS, false);
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
        if let Ok(ft) = y.file_type()
            && !ft.is_dir()
        {
            continue;
        }
        let Ok(mons) = fs::read_dir(y.path()) else {
            continue;
        };
        for m in mons.flatten() {
            if let Ok(ft) = m.file_type()
                && !ft.is_dir()
            {
                continue;
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
        // Fast path: read header for ts/branch/cwd/instructions and any prompt prefix meta update
        let header = extract_header_fields(&path);

        // Determine cwd using header first, fallback to scanning old logs
        let cwd = header
            .as_ref()
            .and_then(|(_, _, cwd, _, _)| cwd.clone())
            .or_else(|| extract_cwd(&path));

        // only include sessions with the same cwd
        if let Some(cwd_str) = cwd.as_deref() {
            if cwd_str != config.cwd.to_string_lossy() {
                continue;
            }
        } else {
            continue;
        }

        // Prefer a meta-update title from the last assistant reply; then prompt prefix (older fallback);
        // then a cached/streaming title; finally header instructions.
        let title = extract_last_meta_title(&path)
            .or_else(|| {
                header
                    .as_ref()
                    .and_then(|(_, _, _, _, prefix)| prefix.clone())
            })
            .or_else(|| extract_title_cached(&path))
            .or_else(|| {
                header
                    .as_ref()
                    .and_then(|(_, _, _, instr, _)| instr.as_ref().map(|s| summarize_title(s)))
                    .filter(|s| !s.is_empty())
            });

        // Meta data for label: timestamp and branch
        let meta = header
            .as_ref()
            .map(|(ts, branch, _, _, _)| (ts.clone(), branch.clone()));

        let label = build_label(&path, meta.as_ref(), cwd.as_deref(), title.as_deref());
        out.push(SessionEntry { label, path });
        if out.len() >= max_entries {
            break;
        }
    }
    out
}

use std::path::Path;

fn label_for_path(path: &Path) -> String {
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
    path: &Path,
    meta: Option<&(String, Option<String>)>,
    _cwd: Option<&str>,
    title: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Prioritize the title (most important for identification)
    if let Some(t) = title {
        let truncated = truncate_title(t, 60);
        if !truncated.is_empty() {
            parts.push(truncated);
        }
    }

    // Add timestamp (make it shorter and more readable)
    if let Some((ts, branch)) = meta {
        let readable_time = format_readable_timestamp(ts);
        parts.push(readable_time);

        // Add branch info if it's not main/master
        if let Some(b) = branch
            && !b.is_empty()
            && b != "main"
            && b != "master"
        {
            parts.push(format!("Branch: {}", b));
        }
    }

    // If no meaningful title was found, make timestamp more prominent
    if title.is_none()
        && meta.is_some()
        && let Some((ts, _)) = meta
    {
        let readable_time = format_readable_timestamp(ts);
        parts.clear(); // Remove the plain timestamp we added earlier
        parts.push(format!("Session from {}", readable_time));
        // Re-add branch info if it's not main/master
        if let Some((_, b_opt)) = meta
            && let Some(b) = b_opt.as_ref()
            && !b.is_empty()
            && b != "main"
            && b != "master"
        {
            parts.push(format!("Branch: {}", b));
        }
    }

    // Fallback to path-based label
    if parts.is_empty() {
        label_for_path(path)
    } else {
        parts.join("  •  ")
    }
}

fn format_readable_timestamp(ts: &str) -> String {
    // Convert from ISO timestamp to more readable format
    // Example: "2024-01-15T10:30:45" -> "Jan 15, 10:30"
    if ts.len() >= 16 {
        let date_part = &ts[5..10]; // MM-DD
        let time_part = &ts[11..16]; // HH:MM

        let month_day = date_part.replace('-', "/");
        format!("{} {}", month_day, time_part)
    } else {
        ts.chars().take(16).collect::<String>()
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

/// Cached version of extract_title that checks file modification time
fn extract_title_cached(path: &PathBuf) -> Option<String> {
    // Get file metadata for cache checking
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return None,
    };

    let modified = match metadata.modified() {
        Ok(m) => m,
        Err(_) => return None,
    };

    // First, attempt to get a fresh cached value without holding the lock during I/O
    if let Ok(mut cache) = get_title_cache().lock()
        && let Some(title) = cache.get_if_fresh(path, modified)
    {
        return Some(title);
    }

    // Extract title without holding the lock
    let title = extract_title_streaming(path)?;

    // Update cache with new value
    if let Ok(mut cache) = get_title_cache().lock() {
        cache.put(path.clone(), modified, title.clone());
    }

    Some(title)
}

/// Streaming version of title extraction that processes files line by line
fn extract_title_streaming(path: &PathBuf) -> Option<String> {
    use std::io::{BufRead, BufReader};

    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Skip the metadata line
    let _ = lines.next()?;

    // Collect candidates with early termination for performance
    let mut user_candidates: Vec<String> = Vec::new();
    let mut assistant_candidates: Vec<String> = Vec::new();
    let mut tool_calls: Vec<String> = Vec::new();
    let mut files_mentioned: Vec<String> = Vec::new();

    // Limit processing to avoid reading huge files entirely
    const MAX_LINES_TO_PROCESS: usize = 200;
    const MAX_CANDIDATES_PER_TYPE: usize = 5;

    let mut lines_processed = 0;

    for line_result in lines {
        if lines_processed >= MAX_LINES_TO_PROCESS {
            break;
        }

        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        lines_processed += 1;

        // Try to parse as ResponseItem directly to avoid double parsing
        let item: ResponseItem = match serde_json::from_str(&line) {
            Ok(item) => item,
            Err(_) => continue,
        };

        match item {
            ResponseItem::Message { role, content, .. } => {
                // Early exit if we have enough candidates
                if user_candidates.len() >= MAX_CANDIDATES_PER_TYPE
                    && assistant_candidates.len() >= MAX_CANDIDATES_PER_TYPE
                {
                    continue;
                }

                let mut acc = String::new();
                for c in content {
                    match c {
                        codex_protocol::models::ContentItem::OutputText { text }
                        | codex_protocol::models::ContentItem::InputText { text } => {
                            // Skip environment context messages
                            if text.contains("<environment_context>") {
                                continue;
                            }
                            if !acc.is_empty() {
                                acc.push(' ');
                            }
                            acc.push_str(&text);
                        }
                        _ => {}
                    }
                }

                let acc = acc.trim();
                if acc.is_empty() {
                    continue;
                }

                // Extract file mentions early
                extract_file_mentions(acc, &mut files_mentioned);

                // Filter candidates
                let looks_like_command = acc.starts_with('/');
                let too_short = acc.split_whitespace().take(3).count() < 3;
                let is_boilerplate = is_boilerplate_message(acc);
                let is_placeholder = is_placeholder_text(acc);

                if role == "user"
                    && !looks_like_command
                    && !too_short
                    && !is_boilerplate
                    && !is_placeholder
                {
                    if user_candidates.len() < MAX_CANDIDATES_PER_TYPE {
                        user_candidates.push(acc.to_string());
                    }
                } else if role == "assistant"
                    && !too_short
                    && !is_boilerplate
                    && !is_placeholder
                    && assistant_candidates.len() < MAX_CANDIDATES_PER_TYPE
                {
                    assistant_candidates.push(acc.to_string());
                }
            }
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => {
                // Limit tool call extraction as well
                if tool_calls.len() >= MAX_CANDIDATES_PER_TYPE {
                    continue;
                }

                match name.as_str() {
                    "edit_file" | "write_file" | "create_file" => {
                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&arguments)
                            && let Some(file_path) = args
                                .get("file_path")
                                .or_else(|| args.get("path"))
                                .and_then(|p| p.as_str())
                        {
                            tool_calls.push(format!("edited {}", extract_filename(file_path)));
                        }
                    }
                    "bash" | "shell" => {
                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&arguments)
                            && let Some(command) = args.get("command").and_then(|c| c.as_str())
                        {
                            let cmd_summary = summarize_command(command);
                            if !cmd_summary.is_empty() {
                                tool_calls.push(cmd_summary);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        // Early termination if we have found good candidates
        if !user_candidates.is_empty() && !tool_calls.is_empty() && !files_mentioned.is_empty() {
            break;
        }
    }

    // Build title using the same logic
    build_session_title(
        user_candidates,
        assistant_candidates,
        tool_calls,
        files_mentioned,
    )
}

fn extract_file_mentions(text: &str, files: &mut Vec<String>) {
    use std::collections::HashSet;

    // Pre-compiled set of file extensions for O(1) lookup
    static COMMON_EXTENSIONS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let extensions = COMMON_EXTENSIONS.get_or_init(|| {
        [
            ".rs", ".py", ".js", ".ts", ".jsx", ".tsx", ".go", ".java", ".cpp", ".c", ".h", ".md",
            ".json", ".toml", ".yaml", ".yml",
        ]
        .into_iter()
        .collect()
    });

    // Single pass through words
    for word in text.split_whitespace() {
        // Check for @filename patterns
        if word.starts_with('@') && word.len() > 1 {
            let filename = &word[1..];
            if filename.contains('.') {
                files.push(extract_filename(filename).to_string());
                continue;
            }
        }

        // Check for common file patterns using extension set
        if word.contains('.') {
            for ext in extensions.iter() {
                if word.ends_with(ext) {
                    files.push(extract_filename(word).to_string());
                    break; // Found match, no need to check other extensions
                }
            }
        }
    }
}

fn extract_filename(path: &str) -> &str {
    path.split('/').next_back().unwrap_or(path)
}

fn is_boilerplate_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    let boilerplate_patterns = [
        "ok",
        "thanks",
        "thank you",
        "got it",
        "understood",
        "sure",
        "yes",
        "no problem",
        "i'll help",
        "let me help",
        "i can help",
        "here's",
        "i've",
        "done",
        "completed",
        "i understand",
        "sounds good",
        "perfect",
        "great",
        "excellent",
    ];

    let words = lower.split_whitespace().collect::<Vec<_>>();
    if words.len() <= 4 {
        return boilerplate_patterns
            .iter()
            .any(|pattern| lower.contains(pattern) || words.join(" ") == *pattern);
    }
    false
}

fn is_placeholder_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    let placeholder_patterns = [
        "<user_instructions>",
        "<instructions>",
        "<system>",
        "<environment_context>",
        "<placeholder>",
        "<context>",
        "<system_message>",
        "user instructions",
        "system instructions",
        "environment context",
    ];

    // Check for placeholder patterns
    for pattern in &placeholder_patterns {
        if lower.contains(pattern) {
            return true;
        }
    }

    // Check if text looks like XML/HTML tags
    if text.trim().starts_with('<') && text.trim().ends_with('>') {
        return true;
    }

    // Check if it's just a tag name without content
    if text.trim().starts_with('<') && text.contains('>') && text.len() < 50 {
        return true;
    }

    false
}

fn summarize_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    let words: Vec<&str> = cmd.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }

    let first_word = words[0];
    match first_word {
        "cargo" => {
            if words.len() > 1 {
                format!("cargo {}", words[1])
            } else {
                "cargo".to_string()
            }
        }
        "npm" | "yarn" | "pnpm" => {
            if words.len() > 1 {
                format!("{} {}", first_word, words[1])
            } else {
                first_word.to_string()
            }
        }
        "git" => {
            if words.len() > 1 {
                format!("git {}", words[1])
            } else {
                "git".to_string()
            }
        }
        "docker" => {
            if words.len() > 1 {
                format!("docker {}", words[1])
            } else {
                "docker".to_string()
            }
        }
        "make" | "just" => {
            if words.len() > 1 {
                format!("{} {}", first_word, words[1])
            } else {
                first_word.to_string()
            }
        }
        _ => {
            if cmd.len() > 30 {
                format!("{}...", &cmd[..27])
            } else {
                cmd.to_string()
            }
        }
    }
}

fn build_session_title(
    user_candidates: Vec<String>,
    assistant_candidates: Vec<String>,
    tool_calls: Vec<String>,
    files_mentioned: Vec<String>,
) -> Option<String> {
    // Priority 1: Meaningful user message
    if let Some(user_msg) = user_candidates.into_iter().next() {
        let summary = summarize_title(&user_msg);
        if !summary.is_empty() {
            return Some(add_context_to_title(summary, &tool_calls, &files_mentioned));
        }
    }

    // Priority 2: Build title from tool calls and files
    if !tool_calls.is_empty() || !files_mentioned.is_empty() {
        let mut parts = Vec::new();

        // Add primary action
        if !tool_calls.is_empty() {
            let main_action = tool_calls[0].clone();
            parts.push(main_action);
        }

        // Add file context
        if !files_mentioned.is_empty() {
            let unique_files: std::collections::HashSet<_> =
                files_mentioned.iter().cloned().collect();
            let files: Vec<_> = unique_files.into_iter().take(2).collect();
            if files.len() == 1 {
                parts.push(format!("in {}", files[0]));
            } else if files.len() == 2 {
                parts.push(format!("in {} & {}", files[0], files[1]));
            } else if files.len() > 2 {
                parts.push(format!("in {} & {} more files", files[0], files.len() - 1));
            }
        }

        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }

    // Priority 3: Assistant message as fallback
    if let Some(assistant_msg) = assistant_candidates.into_iter().next() {
        let summary = summarize_title(&assistant_msg);
        if !summary.is_empty() {
            return Some(summary);
        }
    }

    // Priority 4: Generate title from tool calls only
    if !tool_calls.is_empty() {
        return Some(tool_calls[0].clone());
    }

    // Priority 5: Generate title from files only
    if !files_mentioned.is_empty() {
        let unique_files: std::collections::HashSet<_> = files_mentioned.into_iter().collect();
        let files: Vec<_> = unique_files.into_iter().take(2).collect();
        if files.len() == 1 {
            return Some(format!("Working with {}", files[0]));
        } else if files.len() > 1 {
            return Some(format!("Working with {} files", files.len()));
        }
    }

    None
}

fn add_context_to_title(
    mut title: String,
    _tool_calls: &[String],
    files_mentioned: &[String],
) -> String {
    // If title doesn't mention files but we have file context, add it
    if !files_mentioned.is_empty()
        && !title.to_lowercase().contains(".rs")
        && !title.to_lowercase().contains(".py")
        && !title.to_lowercase().contains(".js")
    {
        let unique_files: std::collections::HashSet<_> = files_mentioned.iter().cloned().collect();
        let files: Vec<_> = unique_files.into_iter().take(1).collect();
        if !files.is_empty() {
            title = format!("{} ({})", title, files[0]);
        }
    }
    title
}

fn summarize_title(s: &str) -> String {
    // Prefer first line
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return String::new();
    }
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
        if i >= 12 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
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
                    codex_protocol::models::ContentItem::InputText { text }
                    | codex_protocol::models::ContentItem::OutputText { text } => {
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

/// Read the JSONL header line to extract timestamp, branch, cwd, and instructions.
/// Also scans a few subsequent lines to find a meta_update containing an initial prompt prefix.
/// Returns (timestamp, branch, cwd, instructions, prompt_prefix).
type HeaderFields = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn extract_header_fields(path: &PathBuf) -> Option<HeaderFields> {
    use std::io::{BufRead, BufReader};

    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    let _ = reader.read_line(&mut first_line).ok()?;
    let first_line = first_line.trim();
    if first_line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(first_line).ok()?;
    // The header is flattened: { "id":..., "timestamp":..., "instructions":..., "git": {...}, "cwd": "..." }
    let ts = v.get("timestamp")?.as_str()?.to_string();
    let branch = v
        .get("git")
        .and_then(|g| g.get("branch"))
        .and_then(|b| b.as_str())
        .map(|s| s.to_string());
    let cwd = v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string());
    let instructions = v
        .get("instructions")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string());

    // Attempt to find a meta_update line with a prompt_prefix by scanning a small number of lines.
    let mut prompt_prefix: Option<String> = None;
    let mut buf = String::new();
    // Scan up to a small number of lines for performance.
    for _ in 0..200 {
        buf.clear();
        if reader.read_line(&mut buf).ok()? == 0 {
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v.get("record_type")
                .and_then(|x| x.as_str())
                .map(|s| s == "meta_update")
                .unwrap_or(false)
            && let Some(pp) = v.get("prompt_prefix").and_then(|x| x.as_str())
            && !pp.is_empty()
        {
            prompt_prefix = Some(pp.to_string());
            break;
        }
    }

    Some((ts, branch, cwd, instructions, prompt_prefix))
}

/// Find the most recent meta_update.title from the rollout file (scan from the end).
fn extract_last_meta_title(path: &PathBuf) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    // Read only the tail of the file to avoid loading large files into memory.
    // 64 KiB should be enough to capture recent meta updates.
    const TAIL_READ_SIZE: u64 = 64 * 1024;

    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_READ_SIZE);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return None;
    }

    // Iterate from the end to find the most recent title meta update.
    for line in buf.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v.get("record_type")
                .and_then(|x| x.as_str())
                .map(|s| s == "meta_update")
                .unwrap_or(false)
            && let Some(title) = v.get("title").and_then(|x| x.as_str())
        {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

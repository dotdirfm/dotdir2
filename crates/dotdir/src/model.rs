use dotdir_core::ops::{EntryInfo, EntryKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub directory: bool,
    pub symlink: bool,
    pub size: u64,
    pub modified: f64,
    pub hidden: bool,
    pub parent: bool,
    pub mode: u32,
}

fn parent_entry(path: &Path) -> Option<Entry> {
    Some(Entry {
        name: "..".into(),
        path: path.parent()?.into(),
        directory: true,
        symlink: false,
        size: 0,
        modified: 0.,
        hidden: false,
        parent: true,
        mode: 0,
    })
}

pub fn read_directory(path: &Path, hidden: bool, sort: SortOrder) -> anyhow::Result<Vec<Entry>> {
    let mut items = dotdir_core::ops::entries(
        path.to_str()
            .ok_or_else(|| anyhow::anyhow!("Directory is not valid UTF-8"))?,
    )?
    .into_iter()
    .filter(|e| hidden || !e.hidden)
    .map(|e: EntryInfo| {
        let child = path.join(&e.name);
        Entry {
            directory: matches!(e.kind, EntryKind::Directory)
                || (matches!(e.kind, EntryKind::Symlink) && child.is_dir()),
            symlink: matches!(e.kind, EntryKind::Symlink),
            path: child,
            name: e.name,
            size: e.size as u64,
            modified: e.mtime_ms,
            hidden: e.hidden,
            parent: false,
            mode: e.mode,
        }
    })
    .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b.directory.cmp(&a.directory).then_with(|| match sort {
            SortOrder::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortOrder::Size => b.size.cmp(&a.size).then(a.name.cmp(&b.name)),
            SortOrder::Modified => b.modified.total_cmp(&a.modified).then(a.name.cmp(&b.name)),
            SortOrder::Extension => a
                .path
                .extension()
                .cmp(&b.path.extension())
                .then(a.name.cmp(&b.name)),
        })
    });
    if let Some(parent) = parent_entry(path) {
        items.insert(0, parent);
    }
    Ok(items)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum SortOrder {
    #[default]
    Name,
    Size,
    Modified,
    Extension,
}

/// Presentation is independent for each pane. Brief retains the original layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Presentation {
    #[default]
    Brief,
    Icons,
    Details,
    Columns,
}
impl Presentation {
    pub const ALL: [Self; 4] = [Self::Brief, Self::Icons, Self::Details, Self::Columns];
    pub fn label(self) -> &'static str {
        match self {
            Self::Brief => "Brief",
            Self::Icons => "Large icons",
            Self::Details => "Details",
            Self::Columns => "Columns",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PanelLayout {
    pub presentation: Presentation,
    pub rows: usize,
    pub columns: usize,
    pub cell_height: f32,
}
impl PanelLayout {
    pub fn new(presentation: Presentation, width: f32, height: f32) -> Self {
        let cell_height = if presentation == Presentation::Icons {
            116.
        } else {
            26.
        };
        let header = if matches!(presentation, Presentation::Details | Presentation::Columns) {
            26.
        } else {
            0.
        };
        let rows = ((height - header) / cell_height).floor().max(1.) as usize;
        let columns = match presentation {
            Presentation::Brief => (width / 240.).floor().max(1.) as usize,
            Presentation::Icons => (width / 112.).floor().max(1.) as usize,
            _ => 1,
        };
        Self {
            presentation,
            rows,
            columns,
            cell_height,
        }
    }
    pub fn delta(self, command: &str) -> isize {
        let row_major = self.presentation == Presentation::Icons;
        match command {
            "up" | "mark-up" => {
                if row_major {
                    -(self.columns as isize)
                } else {
                    -1
                }
            }
            "down" | "mark-down" => {
                if row_major {
                    self.columns as isize
                } else {
                    1
                }
            }
            "left" => {
                if row_major {
                    -1
                } else if self.presentation == Presentation::Brief {
                    -(self.rows as isize)
                } else {
                    0
                }
            }
            "right" => {
                if row_major {
                    1
                } else if self.presentation == Presentation::Brief {
                    self.rows as isize
                } else {
                    0
                }
            }
            "page-up" => -((self.rows * self.columns) as isize),
            "page-down" => (self.rows * self.columns) as isize,
            _ => 0,
        }
    }
    pub fn ensure_visible(self, directory: &mut Directory) {
        if self.presentation != Presentation::Icons {
            directory.ensure_visible(self.rows, self.columns);
            return;
        }
        let cursor_row = directory.cursor / self.columns;
        let mut top_row = directory.top / self.columns;
        if cursor_row < top_row {
            top_row = cursor_row;
        }
        if cursor_row >= top_row + self.rows {
            top_row = cursor_row + 1 - self.rows;
        }
        let total_rows = directory.entries.len().div_ceil(self.columns);
        directory.top = top_row.min(total_rows.saturating_sub(self.rows)) * self.columns;
    }
    pub fn move_cursor(self, directory: &mut Directory, command: &str) {
        // Brief columns are filled vertically. Crossing a viewport edge shifts
        // a whole column so the cursor keeps its screen row and column.
        if self.presentation == Presentation::Brief {
            let capacity = self.rows * self.columns;
            let max_top = directory.entries.len().saturating_sub(capacity);
            if command == "right" && directory.cursor >= directory.top + capacity - self.rows {
                directory.top = directory.top.saturating_add(self.rows).min(max_top);
            } else if command == "left" && directory.cursor < directory.top + self.rows {
                directory.top = directory.top.saturating_sub(self.rows).min(max_top);
            }
        }
        directory.move_cursor(
            self.delta(command),
            command.starts_with("mark-"),
            self.rows,
            self.columns,
        );
        self.ensure_visible(directory);
    }
    pub fn scroll_pixels(self, directory: &mut Directory, pixels: f32) -> bool {
        if !pixels.is_finite() {
            return false;
        }
        directory.wheel_remainder += pixels / self.cell_height;
        let steps = directory.wheel_remainder.round() as isize;
        directory.wheel_remainder -= steps as f32;
        if steps == 0 {
            return false;
        }
        let previous = (directory.cursor, directory.top);
        let stride = if self.presentation == Presentation::Icons {
            self.columns
        } else {
            1
        };
        directory.cursor = directory
            .cursor
            .saturating_add_signed(steps.saturating_mul(stride as isize))
            .min(directory.entries.len().saturating_sub(1));
        let moved_rows = (directory.cursor / stride) as isize - (previous.0 / stride) as isize;
        directory.top = (directory.top / stride).saturating_add_signed(moved_rows) * stride;
        self.ensure_visible(directory);
        previous != (directory.cursor, directory.top)
    }
}

const ICON_SCROLL_DURATION: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy)]
struct IconScroll {
    from: f32,
    target: f32,
    started: Instant,
    rows: usize,
    columns: usize,
}

impl IconScroll {
    fn position(self, now: Instant) -> f32 {
        let progress = (now.duration_since(self.started).as_secs_f32()
            / ICON_SCROLL_DURATION.as_secs_f32())
        .min(1.);
        self.target + (self.from - self.target) * (1. - progress).powi(3)
    }
}

#[derive(Debug, Clone)]
pub struct Directory {
    pub id: u64,
    pub path: PathBuf,
    pub entries: Arc<Vec<Entry>>,
    show_parent: bool,
    pub cursor: usize,
    pub top: usize,
    wheel_remainder: f32,
    icon_scroll: Option<IconScroll>,
    pub marked: HashSet<PathBuf>,
    pub generation: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub history: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
    pub restore_name: Option<String>,
    pub sort: SortOrder,
}
impl Directory {
    pub fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            path,
            entries: Arc::new(vec![]),
            show_parent: true,
            cursor: 0,
            top: 0,
            wheel_remainder: 0.,
            icon_scroll: None,
            marked: HashSet::new(),
            generation: 0,
            loading: false,
            error: None,
            history: vec![],
            forward: vec![],
            restore_name: None,
            sort: SortOrder::Name,
        }
    }
    pub fn focused(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }
    pub fn selected(&self) -> Vec<Entry> {
        if self.marked.is_empty() {
            self.focused()
                .filter(|e| !e.parent)
                .cloned()
                .into_iter()
                .collect()
        } else {
            self.entries
                .iter()
                .filter(|e| self.marked.contains(&e.path))
                .cloned()
                .collect()
        }
    }
    pub fn apply(&mut self, generation: u64, result: anyhow::Result<Vec<Entry>>) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        match result {
            Ok(mut entries) => {
                if !self.show_parent {
                    entries.retain(|entry| !entry.parent);
                }
                let restore = self
                    .restore_name
                    .take()
                    .or_else(|| self.focused().map(|e| e.name.clone()));
                self.cursor = restore
                    .and_then(|name| entries.iter().position(|e| e.name == name))
                    .unwrap_or(self.cursor.min(entries.len().saturating_sub(1)));
                self.marked.retain(|p| entries.iter().any(|e| &e.path == p));
                self.entries = Arc::new(entries);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }
    pub fn navigate(&mut self, path: PathBuf) {
        if self.path == path {
            return;
        }
        let old = self.path.clone();
        self.restore_name = if old.parent() == Some(path.as_path()) {
            old.file_name().map(|n| n.to_string_lossy().into_owned())
        } else {
            None
        };
        self.history.push(old);
        self.forward.clear();
        self.path = path;
        self.cursor = 0;
        self.top = 0;
        self.wheel_remainder = 0.;
        self.icon_scroll = None;
        self.marked.clear();
        self.entries = Arc::new(vec![]);
    }
    pub fn move_cursor(&mut self, delta: isize, mark: bool, rows: usize, columns: usize) {
        self.wheel_remainder = 0.;
        if mark {
            self.toggle_mark();
        }
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        self.ensure_visible(rows, columns);
    }
    pub fn icon_scroll_position(
        &mut self,
        layout: PanelLayout,
        reduce_motion: bool,
        now: Instant,
    ) -> (f32, bool) {
        let target = (self.top / layout.columns) as f32;
        let max = self
            .entries
            .len()
            .div_ceil(layout.columns)
            .saturating_sub(layout.rows) as f32;
        let previous = self.icon_scroll.filter(|scroll| {
            !reduce_motion
                && scroll.rows == layout.rows
                && scroll.columns == layout.columns
                && scroll.target <= max
        });
        let scroll = if let Some(previous) = previous.filter(|scroll| scroll.target == target) {
            previous
        } else {
            IconScroll {
                from: previous.map_or(target, |scroll| scroll.position(now)),
                target,
                started: now,
                rows: layout.rows,
                columns: layout.columns,
            }
        };
        self.icon_scroll = Some(scroll);
        let position = scroll.position(now);
        (position, position != target)
    }
    pub fn reset_icon_scroll(&mut self) {
        self.icon_scroll = None;
    }
    pub fn ensure_visible(&mut self, rows: usize, columns: usize) {
        let capacity = rows.max(1) * columns.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + capacity {
            self.top = self.cursor + 1 - capacity;
        }
        self.top = self.top.min(self.entries.len().saturating_sub(capacity));
    }
    pub fn toggle_mark(&mut self) {
        if let Some(entry) = self.focused().filter(|e| !e.parent) {
            let path = entry.path.clone();
            if !self.marked.remove(&path) {
                self.marked.insert(path);
            }
        }
    }
    pub fn select_all(&mut self) {
        self.marked = self
            .entries
            .iter()
            .filter(|e| !e.parent)
            .map(|e| e.path.clone())
            .collect();
    }
}

/// Every column has its own cursor, selection, generation and watch ID.
/// File operations always use `current`, including when a child is being previewed.
#[derive(Debug, Clone)]
pub struct DirectoryBrowser {
    pub id: u64,
    pub columns: Vec<Directory>,
    pub active: usize,
}
impl DirectoryBrowser {
    pub fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            columns: vec![Directory::new(id, path)],
            active: 0,
        }
    }
    pub fn current(&self) -> &Directory {
        &self.columns[self.active]
    }
    pub fn current_mut(&mut self) -> &mut Directory {
        &mut self.columns[self.active]
    }
    pub fn reconcile(&mut self) {
        for index in 1..self.columns.len() {
            if self.columns[index - 1].focused().map(|entry| &entry.path)
                != Some(&self.columns[index].path)
            {
                self.columns.truncate(index);
                break;
            }
        }
        self.active = self.active.min(self.columns.len() - 1);
    }
    pub fn collapse(&mut self) {
        let mut current = self.columns.remove(self.active);
        if !current.show_parent {
            current.show_parent = true;
            if let Some(parent) = parent_entry(&current.path) {
                // Preserve the focused file and viewport when a child becomes
                // the first column (including when leaving Columns mode).
                if !current.entries.is_empty() {
                    current.cursor += 1;
                    current.top += 1;
                }
                Arc::make_mut(&mut current.entries).insert(0, parent);
            }
        }
        self.columns = vec![current];
        self.active = 0;
    }
    /// Keep an existing child and its cursor when the selected path is unchanged.
    /// Otherwise replace the entire branch to its right, rejecting stale reads by ID.
    pub fn preview(&mut self, child_id: u64) -> Option<u64> {
        let path = self
            .current()
            .focused()
            .filter(|e| e.directory && !e.parent)
            .map(|e| e.path.clone());
        if self.columns.get(self.active + 1).map(|d| &d.path) == path.as_ref() {
            return None;
        }
        self.columns.truncate(self.active + 1);
        path.map(|path| {
            let mut child = Directory::new(child_id, path);
            child.show_parent = false;
            child.sort = self.current().sort;
            self.columns.push(child);
            child_id
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub icon_theme: Option<PathBuf>,
    pub show_hidden: bool,
    pub brief: bool,
    pub presentations: Option<[Presentation; 2]>,
    pub font_size: f32,
    pub left: Vec<PathBuf>,
    pub right: Vec<PathBuf>,
    pub left_active: usize,
    pub right_active: usize,
    pub active_pane: usize,
    pub terminal_height: f32,
}
impl Default for Settings {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            theme: "ayu".into(),
            icon_theme: None,
            show_hidden: true,
            brief: true,
            presentations: None,
            font_size: 13.,
            left: vec![home.clone()],
            right: vec![home],
            left_active: 0,
            right_active: 0,
            active_pane: 0,
            terminal_height: 240.,
        }
    }
}
impl Settings {
    pub fn presentation(&self, pane: usize) -> Presentation {
        self.presentations
            .map(|p| p[pane])
            .unwrap_or(if self.brief {
                Presentation::Brief
            } else {
                Presentation::Details
            })
    }
    pub fn load(path: &Path) -> Self {
        let mut s = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default();
        s.font_size = s.font_size.clamp(10., 24.);
        s.terminal_height = s.terminal_height.clamp(100., 700.);
        s.active_pane = s.active_pane.min(1);
        // Migrate the previous global brief/list toggle without losing its preference.
        s.presentations.get_or_insert(
            [if s.brief {
                Presentation::Brief
            } else {
                Presentation::Details
            }; 2],
        );
        for pane in [&mut s.left, &mut s.right] {
            pane.retain(|p| p.is_dir());
            if pane.is_empty() {
                pane.push(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
            }
        }
        s
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
}

pub fn human_size(size: u64) -> String {
    if size < 1024 {
        return size.to_string();
    }
    let mut n = size as f64;
    let mut unit = "";
    for u in ["K", "M", "G", "T", "P"] {
        n /= 1024.;
        unit = u;
        if n < 1024. {
            break;
        }
    }
    format!("{n:.1} {unit}")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn numbered_directory(count: usize) -> Directory {
        let mut directory = Directory::new(1, "/fixture".into());
        directory.entries = Arc::new(
            (0..count)
                .map(|i| Entry {
                    name: format!("{i:03}.txt"),
                    path: PathBuf::from(format!("/fixture/{i:03}.txt")),
                    directory: false,
                    symlink: false,
                    size: i as u64,
                    modified: 0.,
                    hidden: false,
                    parent: false,
                    mode: 0o644,
                })
                .collect(),
        );
        directory
    }

    #[test]
    fn icon_scroll_retargets_between_rows_and_respects_reduce_motion() {
        let mut directory = numbered_directory(100);
        let layout = PanelLayout::new(Presentation::Icons, 360., 250.);
        let now = Instant::now();
        let halfway = now + ICON_SCROLL_DURATION / 2;
        assert_eq!(
            directory.icon_scroll_position(layout, false, now),
            (0., false)
        );
        assert!(layout.scroll_pixels(&mut directory, layout.cell_height));
        assert_eq!((directory.cursor, directory.top), (3, 3));
        assert_eq!(
            directory.icon_scroll_position(layout, false, now),
            (0., true)
        );
        assert_eq!(
            directory.icon_scroll_position(layout, false, halfway),
            (0.875, true)
        );
        assert!(layout.scroll_pixels(&mut directory, layout.cell_height));
        assert_eq!((directory.cursor, directory.top), (6, 6));
        assert_eq!(
            directory.icon_scroll_position(layout, false, halfway),
            (0.875, true)
        );
        assert_eq!(
            directory.icon_scroll_position(layout, false, halfway + ICON_SCROLL_DURATION),
            (2., false)
        );
        assert!(layout.scroll_pixels(&mut directory, -layout.cell_height));
        assert_eq!(
            directory.icon_scroll_position(layout, true, halfway + ICON_SCROLL_DURATION),
            (1., false)
        );
    }

    #[test]
    fn brief_edge_navigation_scrolls_whole_columns_and_clamps_at_the_end() {
        let layout = PanelLayout {
            presentation: Presentation::Brief,
            rows: 10,
            columns: 3,
            cell_height: 26.,
        };
        let mut directory = numbered_directory(95);
        directory.cursor = 20; // first row of the last visible column
        layout.move_cursor(&mut directory, "right");
        assert_eq!((directory.cursor, directory.top), (30, 10));
        assert_eq!(directory.cursor - directory.top, 20);
        layout.move_cursor(&mut directory, "right");
        assert_eq!((directory.cursor, directory.top), (40, 20));
        directory.cursor = 23; // first visible column, fourth row
        layout.move_cursor(&mut directory, "left");
        assert_eq!((directory.cursor, directory.top), (13, 10));
        assert_eq!(directory.cursor - directory.top, 3);
        directory.top = 60;
        directory.cursor = 80;
        layout.move_cursor(&mut directory, "right");
        assert_eq!((directory.cursor, directory.top), (90, 65));
        layout.move_cursor(&mut directory, "right");
        assert_eq!((directory.cursor, directory.top), (94, 65));
        directory.cursor = 0;
        directory.top = 0;
        layout.move_cursor(&mut directory, "left");
        assert_eq!((directory.cursor, directory.top), (0, 0));
    }

    #[test]
    fn wheel_moves_cursor_and_viewport_together_without_changing_marks() {
        for presentation in [
            Presentation::Brief,
            Presentation::Details,
            Presentation::Columns,
        ] {
            let layout = PanelLayout {
                presentation,
                rows: 4,
                columns: if presentation == Presentation::Brief {
                    3
                } else {
                    1
                },
                cell_height: 26.,
            };
            let mut directory = numbered_directory(100);
            directory.marked.insert(directory.entries[7].path.clone());
            let marks = directory.marked.clone();
            assert!(layout.scroll_pixels(&mut directory, 26.));
            assert_eq!((directory.cursor, directory.top), (1, 1));
            directory.cursor = 7;
            directory.top = 6;
            assert!(layout.scroll_pixels(&mut directory, 52.));
            assert_eq!((directory.cursor, directory.top), (9, 8));
            assert!(layout.scroll_pixels(&mut directory, -26.));
            assert_eq!((directory.cursor, directory.top), (8, 7));
            layout.scroll_pixels(&mut directory, 26_000.);
            assert_eq!(directory.cursor, 99);
            assert_eq!(directory.top, 100 - layout.rows * layout.columns);
            assert!(!layout.scroll_pixels(&mut directory, 26.));
            layout.scroll_pixels(&mut directory, -26_000.);
            assert_eq!((directory.cursor, directory.top), (0, 0));
            assert!(!layout.scroll_pixels(&mut directory, -26.));
            assert_eq!(directory.marked, marks);
            assert!(!layout.scroll_pixels(&mut numbered_directory(0), 26.));
        }
    }

    #[test]
    fn small_wheel_deltas_accumulate_and_keyboard_navigation_resets_them() {
        let layout = PanelLayout {
            presentation: Presentation::Details,
            rows: 4,
            columns: 1,
            cell_height: 26.,
        };
        let mut directory = numbered_directory(100);
        for _ in 0..4 {
            layout.scroll_pixels(&mut directory, 7.);
        }
        assert_eq!((directory.cursor, directory.top), (1, 1));
        for _ in 0..4 {
            layout.scroll_pixels(&mut directory, -7.);
        }
        assert_eq!((directory.cursor, directory.top), (0, 0));
        layout.scroll_pixels(&mut directory, 10.);
        layout.move_cursor(&mut directory, "down");
        assert!(!layout.scroll_pixels(&mut directory, 5.));
        assert_eq!(directory.cursor, 1);
        assert!(!layout.scroll_pixels(&mut directory, f32::NAN));
    }
    #[test]
    fn reload_preserves_cursor_marks_and_ignores_stale_results() {
        let temp = tempfile::tempdir().unwrap();
        for n in ["a", "b", "c"] {
            std::fs::write(temp.path().join(n), "x").unwrap();
        }
        let mut d = Directory::new(1, temp.path().into());
        d.generation = 1;
        d.apply(1, read_directory(temp.path(), true, SortOrder::Name));
        d.cursor = 2;
        d.toggle_mark();
        let path = d.focused().unwrap().path.clone();
        std::fs::write(temp.path().join("aa"), "x").unwrap();
        d.generation = 2;
        d.apply(1, Ok(vec![]));
        assert_eq!(d.entries.len(), 4);
        d.apply(2, read_directory(temp.path(), true, SortOrder::Name));
        assert_eq!(d.focused().unwrap().path, path);
        assert!(d.marked.contains(&path));
    }
    #[test]
    fn parent_cannot_be_selected_and_navigation_restores_child() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let mut d = Directory::new(1, child.clone());
        d.generation = 1;
        d.apply(1, read_directory(&child, true, SortOrder::Name));
        d.toggle_mark();
        assert!(d.selected().is_empty());
        d.navigate(temp.path().into());
        d.apply(1, read_directory(temp.path(), true, SortOrder::Name));
        assert_eq!(d.focused().unwrap().path, child);
    }
    #[test]
    fn pane_presentations_migrate_and_persist_independently() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&serde_json::json!({
                "brief": false, "left": [temp.path()], "right": [temp.path()]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut settings = Settings::load(&settings_path);
        assert_eq!(settings.presentation(0), Presentation::Details);
        assert_eq!(settings.presentation(1), Presentation::Details);
        settings.presentations = Some([Presentation::Icons, Presentation::Columns]);
        settings.save(&settings_path).unwrap();
        let restored = Settings::load(&settings_path);
        assert_eq!(restored.presentation(0), Presentation::Icons);
        assert_eq!(restored.presentation(1), Presentation::Columns);
    }
    #[test]
    fn icon_navigation_and_wheel_scrolling_track_grid_rows() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(temp.path().join(format!("{i:02}.txt")), "x").unwrap();
        }
        let mut directory = Directory::new(1, temp.path().into());
        directory.apply(0, read_directory(temp.path(), true, SortOrder::Name));
        let layout = PanelLayout::new(Presentation::Icons, 360., 250.);
        assert_eq!((layout.rows, layout.columns), (2, 3));
        assert_eq!(layout.delta("down"), 3);
        assert_eq!(layout.delta("right"), 1);
        directory.cursor = 19;
        layout.ensure_visible(&mut directory);
        assert_eq!(directory.top, 15);
        assert!(layout.scroll_pixels(&mut directory, layout.cell_height * 2.));
        assert_eq!((directory.cursor, directory.top), (25, 21));
        layout.move_cursor(&mut directory, "up");
        assert_eq!(directory.cursor, 22);
        assert!(directory.top <= 22 && directory.top + 6 > 22);
        assert_eq!(directory.top % 3, 0);
        let narrow = PanelLayout::new(Presentation::Icons, 0., 0.);
        assert_eq!((narrow.rows, narrow.columns), (1, 1));
    }
    #[test]
    fn columns_preview_without_changing_operation_target_and_discard_replaced_branches() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(a.join("nested")).unwrap();
        std::fs::create_dir(&b).unwrap();
        let mut browser = DirectoryBrowser::new(1, temp.path().into());
        browser
            .current_mut()
            .apply(0, read_directory(temp.path(), true, SortOrder::Name));
        browser.current_mut().cursor = browser
            .current()
            .entries
            .iter()
            .position(|e| e.path == a)
            .unwrap();
        assert_eq!(browser.preview(2), Some(2));
        assert_eq!(browser.active, 0);
        assert_eq!(browser.current().selected()[0].path, a);
        assert_eq!(browser.preview(99), None);
        assert_eq!(browser.columns[1].id, 2);
        browser.active = 1;
        browser
            .current_mut()
            .apply(0, read_directory(&a, true, SortOrder::Name));
        browser.current_mut().cursor = 0;
        assert_eq!(browser.preview(3), Some(3));
        browser.active = 0;
        browser.current_mut().cursor = browser
            .current()
            .entries
            .iter()
            .position(|e| e.path == b)
            .unwrap();
        assert_eq!(browser.preview(4), Some(4));
        assert_eq!(browser.columns.len(), 2);
        assert_eq!(browser.columns[1].path, b);
        assert!(!browser.columns.iter().any(|d| matches!(d.id, 2 | 3)));
    }
    #[test]
    fn column_reconciliation_and_mode_change_keep_a_valid_active_directory() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let mut browser = DirectoryBrowser::new(1, temp.path().into());
        browser
            .current_mut()
            .apply(0, read_directory(temp.path(), true, SortOrder::Name));
        browser.current_mut().cursor = 1;
        browser.preview(2);
        browser.active = 1;
        browser
            .current_mut()
            .apply(0, read_directory(&a, true, SortOrder::Name));
        let snapshot = browser.clone();
        browser.columns[0].cursor = 0;
        browser.reconcile();
        assert_eq!(browser.active, 0);
        assert_eq!(browser.columns.len(), 1);
        browser = snapshot;
        browser.collapse();
        assert_eq!(browser.id, 1);
        assert_eq!(browser.current().id, 2);
        assert_eq!(browser.current().path, a);
        assert_eq!((browser.active, browser.columns.len()), (0, 1));
    }
    #[test]
    fn only_the_first_column_has_a_parent_entry_and_collapse_restores_it() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir_all(child.join("empty")).unwrap();
        std::fs::write(child.join("file.txt"), "test").unwrap();
        let mut browser = DirectoryBrowser::new(1, temp.path().into());
        browser
            .current_mut()
            .apply(0, read_directory(temp.path(), true, SortOrder::Name));
        assert!(browser.current().entries[0].parent);
        browser.current_mut().cursor = 1;
        browser.preview(2);
        browser.active = 1;
        browser
            .current_mut()
            .apply(0, read_directory(&child, true, SortOrder::Name));
        assert!(browser.current().entries.iter().all(|e| !e.parent));
        assert_eq!(browser.current().focused().unwrap().name, "empty");
        browser.current_mut().move_cursor(-1, false, 10, 1);
        assert_eq!(browser.current().cursor, 0);
        browser.preview(3);
        browser.active = 2;
        browser.current_mut().apply(
            0,
            read_directory(&child.join("empty"), true, SortOrder::Name),
        );
        assert!(browser.current().entries.is_empty());
        assert!(browser.current().focused().is_none());
        browser.active = 1;
        browser.current_mut().cursor = 1;
        browser
            .current_mut()
            .apply(0, read_directory(&child, true, SortOrder::Name));
        assert!(browser.current().entries.iter().all(|e| !e.parent));
        assert_eq!(browser.current().focused().unwrap().name, "file.txt");
        browser.collapse();
        assert!(browser.current().entries[0].parent);
        assert_eq!(browser.current().focused().unwrap().name, "file.txt");
        browser
            .current_mut()
            .apply(0, read_directory(&child, true, SortOrder::Name));
        assert_eq!(
            browser
                .current()
                .entries
                .iter()
                .filter(|e| e.parent)
                .count(),
            1
        );
        let root = read_directory(Path::new("/"), true, SortOrder::Name).unwrap();
        assert!(root.iter().all(|e| !e.parent));
    }
    #[test]
    fn validate_names() {
        for n in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(!valid_name(n));
        }
        assert!(valid_name("hello.txt"));
    }
}

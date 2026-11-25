use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    term::{Config, Term, TermMode, cell::Flags},
    vte::{
        ansi::Processor,
        ansi::{Color, NamedColor},
    },
};
use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, mpsc},
};

#[derive(Clone)]
pub struct Listener {
    reply: Arc<Mutex<Box<dyn Write + Send>>>,
    title: Arc<Mutex<String>>,
}
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                let _ = self.reply.lock().unwrap().write_all(s.as_bytes());
            }
            Event::Title(s) => *self.title.lock().unwrap() = s,
            _ => {}
        }
    }
}
#[derive(Clone, Copy)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}
pub struct Terminal {
    pub id: u64,
    pub shell: String,
    pub title: Arc<Mutex<String>>,
    pub term: Arc<Mutex<Term<Listener>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pub dirty: Arc<AtomicBool>,
    pub closed: Arc<AtomicBool>,
    size: Size,
    pub _notifications: mpsc::Receiver<()>,
}
impl Terminal {
    pub fn spawn(id: u64, cwd: &Path, shell: Option<&str>) -> Result<Self> {
        let shell = shell
            .map(String::from)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "powershell.exe".into()
                } else {
                    "/bin/sh".into()
                }
            });
        let size = Size {
            cols: 100,
            rows: 20,
        };
        let pair = native_pty_system().openpty(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        if !cfg!(windows) {
            cmd.arg("-l");
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let title = Arc::new(Mutex::new(
            shell.rsplit('/').next().unwrap_or("shell").to_string(),
        ));
        let term = Arc::new(Mutex::new(Term::new(
            Config::default(),
            &size,
            Listener {
                reply: writer.clone(),
                title: title.clone(),
            },
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        let closed = Arc::new(AtomicBool::new(false));
        let mut reader = pair.master.try_clone_reader()?;
        let (tx, notifications) = mpsc::channel();
        let (grid, changed, exited) = (term.clone(), dirty.clone(), closed.clone());
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut bytes = [0; 16384];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        parser.advance(&mut *grid.lock().unwrap(), &bytes[..n]);
                        changed.store(true, Ordering::Relaxed);
                    }
                }
            }
            exited.store(true, Ordering::Relaxed);
            let _ = tx.send(());
        });
        Ok(Self {
            id,
            shell,
            title,
            term,
            writer,
            master: pair.master,
            child,
            dirty,
            closed,
            size,
            _notifications: notifications,
        })
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.term.lock().unwrap().scroll_display(Scroll::Bottom);
        self.writer.lock().unwrap().write_all(bytes)?;
        Ok(())
    }
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if self.size.cols == cols && self.size.rows == rows {
            return;
        }
        self.size = Size { cols, rows };
        let _ = self.master.resize(PtySize {
            cols: cols as u16,
            rows: rows as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term.lock().unwrap().resize(self.size);
        self.dirty.store(true, Ordering::Relaxed);
    }
    pub fn scroll(&mut self, lines: i32) {
        self.term
            .lock()
            .unwrap()
            .scroll_display(Scroll::Delta(lines));
        self.dirty.store(true, Ordering::Relaxed);
    }
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self
            .term
            .lock()
            .unwrap()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            self.write(b"\x1b[200~")?;
        }
        self.write(text.replace('\x1b', "").as_bytes())?;
        if bracketed {
            self.write(b"\x1b[201~")?;
        }
        Ok(())
    }
    pub fn snapshot(&self) -> Vec<Vec<CellRun>> {
        let term = self.term.lock().unwrap();
        let content = term.renderable_content();
        let mut rows = vec![Vec::<CellRun>::new(); self.size.rows];
        for indexed in content.display_iter {
            let y = indexed.point.line.0 + content.display_offset as i32;
            if y < 0 || y as usize >= rows.len() || indexed.flags.contains(Flags::WIDE_CHAR_SPACER)
            {
                continue;
            }
            let cursor = content.mode.contains(TermMode::SHOW_CURSOR)
                && content.cursor.point == indexed.point;
            let (fg, bg) = if indexed.flags.contains(Flags::INVERSE) {
                (indexed.bg, indexed.fg)
            } else {
                (indexed.fg, indexed.bg)
            };
            let mut text = indexed.c.to_string();
            if let Some(extra) = indexed.zerowidth() {
                text.extend(extra);
            }
            let row = &mut rows[y as usize];
            if let Some(run) = row
                .last_mut()
                .filter(|r| r.fg == fg && r.bg == bg && r.cursor == cursor)
            {
                run.text.push_str(&text);
            } else {
                row.push(CellRun {
                    text,
                    fg,
                    bg,
                    cursor,
                });
            }
        }
        rows
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
#[derive(Clone)]
pub struct CellRun {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub cursor: bool,
}
pub fn color(color: Color, foreground: bool) -> u32 {
    const ANSI: [u32; 16] = [
        0x191e2a, 0xf28779, 0xbae67e, 0xffd580, 0x73d0ff, 0xd4bfff, 0x95e6cb, 0xcac8be, 0x707a8c,
        0xf28779, 0xbae67e, 0xffd580, 0x73d0ff, 0xd4bfff, 0x95e6cb, 0xffffff,
    ];
    match color {
        Color::Spec(c) => (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b),
        Color::Named(NamedColor::Foreground) => 0xcac8be,
        Color::Named(NamedColor::Background) => 0x171b24,
        Color::Named(n) => {
            ANSI.get(n as usize)
                .copied()
                .unwrap_or(if foreground { 0xcac8be } else { 0x171b24 })
        }
        Color::Indexed(i) if i < 16 => ANSI[i as usize],
        Color::Indexed(i) if i >= 232 => {
            let c = 8 + 10 * (i as u32 - 232);
            (c << 16) | (c << 8) | c
        }
        Color::Indexed(i) => {
            let n = i as u32 - 16;
            let c = |v| if v == 0 { 0 } else { 55 + 40 * v };
            (c(n / 36) << 16) | (c(n / 6 % 6) << 8) | c(n % 6)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn real_pty_handles_ansi_output_and_resize() {
        let dir = tempfile::tempdir().unwrap();
        let mut terminal = Terminal::spawn(1, dir.path(), Some("/bin/sh")).unwrap();
        terminal.resize(60, 8);
        terminal
            .write(b"printf '\\033[31mDOTDIR_PTY_OK\\033[0m\\n'\r")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let rows = terminal.snapshot();
            assert_eq!(rows.len(), 8);
            let output = rows
                .iter()
                .flatten()
                .map(|r| r.text.as_str())
                .collect::<String>();
            if output.contains("DOTDIR_PTY_OK")
                && rows
                    .iter()
                    .flatten()
                    .any(|r| r.fg == Color::Named(NamedColor::Red))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY output did not arrive: {output}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

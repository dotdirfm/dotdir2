use crate::archive::VirtualFile;
use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::{Read, Seek, SeekFrom},
    mem::MaybeUninit,
    ptr::null_mut,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 540;
const STRIDE: usize = WIDTH as usize * 4;
const SOURCE: &CStr = c"dotdir-media://video";
const IO_ERROR: i64 = -20; // MPV_ERROR_GENERIC

#[derive(Default, PartialEq, Clone)]
pub struct State {
    pub ready: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    pub error: Option<String>,
}

#[derive(Default)]
struct Update {
    state: State,
    frame: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub enum Control {
    Toggle,
    Visible(bool),
    Seek(f64),
    Volume(f64),
}

#[derive(Clone)]
pub struct Player {
    commands: mpsc::Sender<Control>,
    update: Arc<Mutex<Update>>,
}

impl Player {
    pub fn new(file: VirtualFile) -> Result<(Self, async_channel::Receiver<()>)> {
        let (commands, pending) = mpsc::channel();
        let (signal, changes) = async_channel::bounded(1);
        let update = Arc::new(Mutex::new(Update::default()));
        let worker = update.clone();
        std::thread::Builder::new()
            .name("dotdir-libmpv".into())
            .spawn(move || {
                let result =
                    Engine::new(file).and_then(|engine| engine.run(pending, &worker, &signal));
                if let Err(error) = result {
                    worker.lock().unwrap().state.error = Some(format!("{error:#}"));
                    let _ = signal.try_send(());
                }
            })
            .context("Could not start the video worker")?;
        Ok((Self { commands, update }, changes))
    }

    pub fn send(&self, command: Control) {
        let _ = self.commands.send(command);
    }

    pub fn state(&self) -> State {
        self.update.lock().unwrap().state.clone()
    }

    pub fn take_update(&self) -> (State, Option<Vec<u8>>) {
        let mut update = self.update.lock().unwrap();
        (update.state.clone(), update.frame.take())
    }
}

type Handle = *mut c_void;
type OpenStream = unsafe extern "C" fn(Handle, *mut c_char, *mut StreamInfo) -> c_int;
type FrameCallback = unsafe extern "C" fn(Handle);

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: Handle,
}

impl RenderParam {
    fn new<T>(kind: c_int, data: *const T) -> Self {
        Self {
            kind,
            data: data.cast_mut().cast(),
        }
    }
    const END: Self = Self {
        kind: 0,
        data: null_mut(),
    };
}

#[repr(C)]
struct Event {
    id: c_int,
    error: c_int,
    userdata: u64,
    data: Handle,
}

// The prefix of mpv_event_end_file used by this backend.
#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
}

#[repr(C)]
struct StreamInfo {
    cookie: Handle,
    read: Option<unsafe extern "C" fn(Handle, *mut c_char, u64) -> i64>,
    seek: Option<unsafe extern "C" fn(Handle, i64) -> i64>,
    size: Option<unsafe extern "C" fn(Handle) -> i64>,
    close: Option<unsafe extern "C" fn(Handle)>,
    cancel: Option<unsafe extern "C" fn(Handle)>,
}

struct Api {
    // Keep the library loaded until the core, render context, and callbacks are gone.
    _library: Library,
    create: unsafe extern "C" fn() -> Handle,
    set_option: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    initialize: unsafe extern "C" fn(Handle) -> c_int,
    destroy: unsafe extern "C" fn(Handle),
    error: unsafe extern "C" fn(c_int) -> *const c_char,
    command: unsafe extern "C" fn(Handle, *const *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(Handle, *const c_char, c_int, Handle) -> c_int,
    wait_event: unsafe extern "C" fn(Handle, f64) -> *mut Event,
    add_stream: unsafe extern "C" fn(Handle, *const c_char, Handle, OpenStream) -> c_int,
    render_create: unsafe extern "C" fn(*mut Handle, Handle, *mut RenderParam) -> c_int,
    render_free: unsafe extern "C" fn(Handle),
    render_callback: unsafe extern "C" fn(Handle, Option<FrameCallback>, Handle),
    render_update: unsafe extern "C" fn(Handle) -> u64,
    render_frame: unsafe extern "C" fn(Handle, *mut RenderParam) -> c_int,
}

impl Api {
    fn load() -> Result<Self> {
        let library = load_library()?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| concat!("Missing libmpv function ", $name))?
            };
        }
        // SAFETY: the signatures above match libmpv's public client, render and
        // stream headers. The versioned library and the resolved symbols stay live.
        unsafe {
            Ok(Self {
                create: symbol!("mpv_create"),
                set_option: symbol!("mpv_set_option_string"),
                initialize: symbol!("mpv_initialize"),
                destroy: symbol!("mpv_terminate_destroy"),
                error: symbol!("mpv_error_string"),
                command: symbol!("mpv_command"),
                get_property: symbol!("mpv_get_property"),
                wait_event: symbol!("mpv_wait_event"),
                add_stream: symbol!("mpv_stream_cb_add_ro"),
                render_create: symbol!("mpv_render_context_create"),
                render_free: symbol!("mpv_render_context_free"),
                render_callback: symbol!("mpv_render_context_set_update_callback"),
                render_update: symbol!("mpv_render_context_update"),
                render_frame: symbol!("mpv_render_context_render"),
                _library: library,
            })
        }
    }

    fn check(&self, code: c_int) -> Result<()> {
        if code >= 0 {
            return Ok(());
        }
        // SAFETY: libmpv returns a static, nul-terminated string for any error code.
        let message = unsafe { CStr::from_ptr((self.error)(code)) };
        bail!("{}", message.to_string_lossy())
    }
}

fn load_library() -> Result<Library> {
    if let Some(path) = std::env::var_os("DOTDIR_LIBMPV") {
        let path = std::path::PathBuf::from(path);
        ensure!(
            path.is_absolute(),
            "DOTDIR_LIBMPV must point to an absolute library path"
        );
        return load_at(&path).with_context(|| format!("Could not load {}", path.display()));
    }
    #[cfg(windows)]
    {
        let profile = std::env::var_os("DOTDIR_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::config_dir().map(|path| path.join("dev.dotdir.native")));
        let executable = std::env::current_exe()?;
        for directory in profile.as_deref().into_iter().chain(executable.parent()) {
            let path = directory.join("libmpv-2.dll");
            if path.is_file() {
                return load_at(&path)
                    .with_context(|| format!("Could not load {}", path.display()));
            }
        }
        bail!("Install libmpv-2.dll in the DotDir profile or set DOTDIR_LIBMPV to its path")
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: libmpv.so.2 is resolved by the system dynamic linker.
        unsafe { Library::new("libmpv.so.2") }
            .context("Install your distribution's libmpv 2 runtime or set DOTDIR_LIBMPV")
    }
}

fn load_at(path: &std::path::Path) -> Result<Library> {
    let path = path.canonicalize()?;
    // SAFETY: paths are chosen by the user or by DotDir, never by media contents.
    #[cfg(windows)]
    unsafe {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            Library as WindowsLibrary,
        };
        Ok(WindowsLibrary::load_with_flags(
            path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )?
        .into())
    }
    #[cfg(target_os = "linux")]
    unsafe {
        Ok(Library::new(path)?)
    }
}

struct Engine {
    api: Api,
    core: Handle,
    render: Handle,
    source: Box<VirtualFile>,
    redraw: Box<AtomicBool>,
}

impl Engine {
    fn new(file: VirtualFile) -> Result<Self> {
        let api = Api::load()?;
        // SAFETY: this worker owns the core and performs all render calls. The
        // source and redraw flag have stable addresses and outlive the callbacks.
        unsafe {
            let core = (api.create)();
            ensure!(!core.is_null(), "Could not create libmpv player");
            let mut engine = Self {
                api,
                core,
                render: null_mut(),
                source: Box::new(file),
                redraw: Box::new(AtomicBool::new(false)),
            };
            for (key, value) in [
                (c"config", c"no"),
                (c"load-scripts", c"no"),
                (c"ytdl", c"no"),
                (c"access-references", c"no"),
                (c"autoload-files", c"no"),
                (c"terminal", c"no"),
                (c"vo", c"libmpv"),
                (c"keep-open", c"yes"),
                (c"pause", c"yes"),
                (c"idle", c"yes"),
            ] {
                engine
                    .api
                    .check((engine.api.set_option)(core, key.as_ptr(), value.as_ptr()))
                    .with_context(|| {
                        format!("Could not set libmpv option {}", key.to_string_lossy())
                    })?;
            }
            engine.api.check((engine.api.initialize)(core))?;
            engine.api.check((engine.api.add_stream)(
                core,
                c"dotdir-media".as_ptr(),
                (&mut *engine.source as *mut VirtualFile).cast(),
                open_stream,
            ))?;
            let mut params = [RenderParam::new(1, c"sw".as_ptr()), RenderParam::END];
            engine
                .api
                .check((engine.api.render_create)(
                    &mut engine.render,
                    core,
                    params.as_mut_ptr(),
                ))
                .context("libmpv software rendering is unavailable")?;
            (engine.api.render_callback)(
                engine.render,
                Some(frame_ready),
                (&mut *engine.redraw as *mut AtomicBool).cast(),
            );
            engine.command(&[c"loadfile", SOURCE])?;
            Ok(engine)
        }
    }

    fn command(&self, args: &[&CStr]) -> Result<()> {
        let pointers: Vec<_> = args
            .iter()
            .map(|arg| arg.as_ptr())
            .chain([std::ptr::null()])
            .collect();
        // SAFETY: this is a terminated array of borrowed C strings.
        self.api
            .check(unsafe { (self.api.command)(self.core, pointers.as_ptr()) })
    }

    fn number(&self, property: &CStr) -> f64 {
        let mut value = 0_f64;
        // SAFETY: format 5 (MPV_FORMAT_DOUBLE) requires an f64 output pointer.
        let code = unsafe {
            (self.api.get_property)(
                self.core,
                property.as_ptr(),
                5,
                (&mut value as *mut f64).cast(),
            )
        };
        if code >= 0 && value.is_finite() {
            value.max(0.)
        } else {
            0.
        }
    }

    fn flag(&self, property: &CStr) -> bool {
        let mut value: c_int = 0;
        // SAFETY: format 3 (MPV_FORMAT_FLAG) requires an int output pointer.
        let code = unsafe {
            (self.api.get_property)(
                self.core,
                property.as_ptr(),
                3,
                (&mut value as *mut c_int).cast(),
            )
        };
        code >= 0 && value != 0
    }

    fn read_events(&self, ready: &mut bool) -> Result<()> {
        // SAFETY: each event and its payload are read before asking for the next.
        unsafe {
            loop {
                let event = &*(self.api.wait_event)(self.core, 0.);
                match event.id {
                    0 => return Ok(()), // MPV_EVENT_NONE
                    1 => bail!("The video player stopped"),
                    7 if !event.data.is_null() => {
                        let end = &*event.data.cast::<EndFile>();
                        self.api
                            .check(end.error)
                            .context("Cannot play this video")?;
                        ensure!(end.reason != 5, "Video playlists are not supported");
                    }
                    8 => *ready = true, // MPV_EVENT_FILE_LOADED
                    _ => {}
                }
            }
        }
    }

    fn set_pause(&self, paused: bool) -> Result<()> {
        self.command(&[c"set", c"pause", if paused { c"yes" } else { c"no" }])
    }

    fn run(
        self,
        controls: mpsc::Receiver<Control>,
        update: &Mutex<Update>,
        signal: &async_channel::Sender<()>,
    ) -> Result<()> {
        let mut surface = Surface::new();
        let mut visible = false;
        let mut resume = true;
        let mut ready = false;
        let mut status = State::default();
        let mut sampled = Instant::now();
        loop {
            let delay = if visible {
                Duration::from_millis(12)
            } else {
                Duration::from_millis(80)
            };
            let mut refresh_status = false;
            match controls.recv_timeout(delay) {
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(action) => {
                    refresh_status = true;
                    match action {
                        Control::Visible(show) if show != visible => {
                            visible = show;
                            if show {
                                self.set_pause(!resume)?;
                                self.redraw.store(true, Ordering::Release);
                            } else {
                                resume = !self.flag(c"pause");
                                self.set_pause(true)?;
                            }
                        }
                        Control::Toggle if visible && ready => {
                            if self.flag(c"eof-reached") {
                                self.command(&[c"seek", c"0", c"absolute"])?;
                                self.set_pause(false)?;
                            } else {
                                self.set_pause(!self.flag(c"pause"))?;
                            }
                        }
                        Control::Seek(position) if ready => {
                            let position = CString::new(position.clamp(0., 100.).to_string())?;
                            let _ = self.command(&[c"seek", &position, c"absolute-percent"]);
                        }
                        Control::Volume(volume) => {
                            let volume = CString::new(volume.clamp(0., 100.).to_string())?;
                            let _ = self.command(&[c"set", c"volume", &volume]);
                        }
                        _ => {}
                    }
                }
            }
            self.read_events(&mut ready)?;
            let frame = if self.redraw.swap(false, Ordering::AcqRel) {
                // SAFETY: the render context is owned and used only by this worker.
                let flags = unsafe { (self.api.render_update)(self.render) };
                if flags & 1 != 0 {
                    self.render(&mut surface, visible)?
                } else {
                    None
                }
            } else {
                None
            };
            let previous = status.clone();
            if refresh_status
                || ready != status.ready
                || sampled.elapsed() >= Duration::from_millis(150)
            {
                status = State {
                    ready,
                    paused: self.flag(c"pause") || self.flag(c"eof-reached"),
                    position: self.number(c"time-pos"),
                    duration: self.number(c"duration"),
                    error: None,
                };
                sampled = Instant::now();
            }
            if frame.is_some() || status != previous {
                let mut update = update.lock().unwrap();
                update.state = status.clone();
                if frame.is_some() {
                    update.frame = frame;
                }
                let _ = signal.try_send(());
            }
        }
    }

    fn render(&self, surface: &mut Surface, visible: bool) -> Result<Option<Vec<u8>>> {
        let size = [WIDTH as c_int, HEIGHT as c_int];
        let stride = STRIDE;
        let skip = c_int::from(!visible);
        let mut params = [
            RenderParam::new(17, size.as_ptr()), // MPV_RENDER_PARAM_SW_SIZE
            RenderParam::new(18, c"bgr0".as_ptr()),
            RenderParam::new(19, &stride),
            RenderParam::new(20, surface.blocks.as_mut_ptr()),
            RenderParam::new(13, &skip), // MPV_RENDER_PARAM_SKIP_RENDERING
            RenderParam::END,
        ];
        // SAFETY: the aligned surface is large enough for
        // stride * height and every parameter remains valid through this call.
        self.api
            .check(unsafe { (self.api.render_frame)(self.render, params.as_mut_ptr()) })?;
        Ok(visible.then(|| surface.bgra()))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: unregister the render callback and destroy the render context
        // before terminating the core. The stream source stays alive throughout.
        unsafe {
            if !self.render.is_null() {
                (self.api.render_callback)(self.render, None, null_mut());
                (self.api.render_free)(self.render);
            }
            (self.api.destroy)(self.core);
        }
    }
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Block([MaybeUninit<u8>; 64]);

struct Surface {
    blocks: Vec<Block>,
}

impl Surface {
    fn new() -> Self {
        Self {
            blocks: vec![Block([MaybeUninit::new(0); 64]); STRIDE * HEIGHT as usize / 64],
        }
    }

    fn bgra(&mut self) -> Vec<u8> {
        let len = STRIDE * HEIGHT as usize;
        let pixels = self.blocks.as_mut_ptr().cast::<u8>();
        // SAFETY: each block is exactly 64 bytes. libmpv wrote B, G and R.
        // Initialize the alpha bytes before viewing the whole surface as bytes.
        unsafe {
            for offset in (3..len).step_by(4) {
                pixels.add(offset).write(255);
            }
            std::slice::from_raw_parts(pixels, len).to_vec()
        }
    }
}

unsafe extern "C" fn frame_ready(context: Handle) {
    // SAFETY: this pointer is the redraw flag retained by Engine.
    unsafe { &*context.cast::<AtomicBool>() }.store(true, Ordering::Release);
}

unsafe extern "C" fn open_stream(source: Handle, uri: *mut c_char, info: *mut StreamInfo) -> c_int {
    // SAFETY: libmpv provides a C string and writable stream info. The original
    // file is retained until the core is destroyed. Each stream owns its cursor.
    unsafe {
        if CStr::from_ptr(uri) != SOURCE {
            return -13; // MPV_ERROR_LOADING_FAILED
        }
        let cursor = Box::new((&*source.cast::<VirtualFile>()).clone());
        info.write(StreamInfo {
            cookie: Box::into_raw(cursor).cast(),
            read: Some(read_stream),
            seek: Some(seek_stream),
            size: Some(stream_size),
            close: Some(close_stream),
            cancel: None,
        });
    }
    0
}

unsafe extern "C" fn read_stream(cookie: Handle, buffer: *mut c_char, count: u64) -> i64 {
    let count = count.min(1024 * 1024) as usize;
    if count == 0 {
        return 0;
    }
    // SAFETY: libmpv serializes reads/seeks for this cookie and provides a
    // writable buffer of at least count bytes for this call.
    unsafe {
        (&mut *cookie.cast::<VirtualFile>())
            .read(std::slice::from_raw_parts_mut(buffer.cast(), count))
            .map_or(IO_ERROR, |len| len as i64)
    }
}

unsafe extern "C" fn seek_stream(cookie: Handle, offset: i64) -> i64 {
    let Ok(offset) = u64::try_from(offset) else {
        return IO_ERROR;
    };
    // SAFETY: this live cursor belongs to the calling stream.
    unsafe { &mut *cookie.cast::<VirtualFile>() }
        .seek(SeekFrom::Start(offset))
        .ok()
        .and_then(|pos| i64::try_from(pos).ok())
        .unwrap_or(IO_ERROR)
}

unsafe extern "C" fn stream_size(cookie: Handle) -> i64 {
    // SAFETY: this live cursor belongs to the calling stream.
    i64::try_from(unsafe { &*cookie.cast::<VirtualFile>() }.len()).unwrap_or(IO_ERROR)
}

unsafe extern "C" fn close_stream(cookie: Handle) {
    // SAFETY: exactly one close is called for each cookie created by open_stream.
    drop(unsafe { Box::from_raw(cookie.cast::<VirtualFile>()) });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready() {
            assert!(Instant::now() < deadline, "Timed out waiting for libmpv");
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    #[test]
    #[ignore = "requires a compatible libmpv runtime"]
    fn plays_a_seekable_stream_and_pauses_when_hidden() {
        let mut bytes = b"YUV4MPEG2 W32 H32 F25:1 Ip A1:1 C420jpeg\n".to_vec();
        for _ in 0..100 {
            bytes.extend_from_slice(b"FRAME\n");
            bytes.extend(std::iter::repeat_n(160, 32 * 32));
            bytes.extend(std::iter::repeat_n(128, 2 * 16 * 16));
        }
        let (player, _updates) = Player::new(VirtualFile::memory(bytes)).unwrap();
        player.send(Control::Visible(true));
        wait(|| {
            let (state, frame) = player.take_update();
            assert!(state.error.is_none(), "{:?}", state.error);
            state.ready
                && frame.is_some_and(|frame| {
                    frame.len() == WIDTH as usize * HEIGHT as usize * 4
                        && frame
                            .chunks_exact(4)
                            .any(|pixel| pixel[0] > 50 && pixel[3] == 255)
                })
        });
        player.send(Control::Visible(false));
        wait(|| player.state().paused);
        player.send(Control::Seek(50.));
        wait(|| {
            let state = player.state();
            state.paused && (state.position - 2.).abs() < 0.15
        });
        player.send(Control::Visible(true));
        wait(|| {
            let state = player.state();
            !state.paused && state.position > 2.15
        });
    }
}

# DotDir Native

A native Rust/GPUI port of DotDir. The interface follows the existing app's
Ayu colors, compact column-based file panes, tabs, breadcrumbs, command line and
function-key bar. JavaScript extensions run on QuickJS. No Deno or browser shell
is used.

## Run

```sh
cargo run --release -- /path/to/start
```

Omit the path to restore the previous directory tabs. The project pins Rust
1.100.0 and GPUI Kit 0.11.0. Rustup installs the project toolchain automatically.
On macOS, install full Xcode and its command-line tools for GPUI/Metal builds.

Build a local, ad-hoc signed macOS app:

```sh
scripts/bundle-macos.sh        # optimized
scripts/bundle-macos.sh debug  # faster builds for development
```

Release builds optimize for executable size, use full link-time optimization,
and strip symbols. They take longer to compile. A Rust panic terminates the
process; ordinary application errors and JavaScript exceptions remain handled.
Development and test builds retain panic unwinding.
See the [release size measurements](docs/release-size.md) for the build comparison.

Open `dist/DotDir Native.app`. This is a local development bundle, not a notarized
distribution. Linux/Windows builds need the platform prerequisites described by
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

## Keyboard

| Key | Action |
| --- | --- |
| Tab | Switch file pane |
| Arrows / Home / End / Page Up / Page Down | Navigate entries and columns |
| Enter / Backspace | Open entry / parent directory |
| Space / Insert / Shift+arrows / ⌘A | Mark entries |
| F1 / ⌘⇧P | Command palette |
| F2 / ⌘S | Rename entry / save editor |
| F3 / F4 | View / edit file |
| ⌘F3 / ⌘F4 | Live preview / edit in opposite pane |
| ⌘Shift+Enter | Keep Preview Tab (make it permanent) |
| F5 / F6 / F7 / F8 | Copy / move / mkdir / Trash |
| F9 / F10 / F11 / F12 | Settings / close / extensions / terminal |
| ⌘L / ⌘T / ⌘W / Ctrl+Tab | Go to path / new tab / close tab / next tab |
| ⌘R / ⌘. / ⌘1 | Refresh / hidden entries / toggle Brief and Details |
| Alt+F7 | Recursive filename/content search |
| ⌘Enter in search | Reveal selected result in file pane |
| Ctrl+L | Focus the command line |
| ⌘O | Toggle file panes over terminal |
| Escape | Cancel dialog or close document with dirty-buffer confirmation |

Use the clickable function-key bar when macOS reserves the hardware function
keys. Settings and keybindings are separate from the original app, under
`~/Library/Application Support/dev.dotdir.native` on macOS. Override the location
with `DOTDIR_CONFIG_DIR`. Edit `keybindings.json` to override commands; restart
after changing bindings. Hold Alt, Control, Command or Shift to show that
modifier combination's F-key commands in the bottom bar (for example, Alt+F7
shows Find Files). The labels and clicks follow the active keyboard context
and custom bindings. Unbound slots are empty; unavailable commands are dimmed.

F1 lists commands available for the active pane. The same capability rules are
checked again when executing a command, including through custom keybindings
and the function-key bar. Keyboard contexts still control where shortcuts match;
opening the palette does not change the pane those commands target.
Play/Pause requires a playable video, media navigation requires a neighboring
file, file operations require a selection and no running transfer, and Save
requires an editable document that is not already saving. Read-only previews,
images and failed videos do not offer video playback. Existing API v1 extension
commands remain global; native viewer registrations are not executable commands.

⌘F3 opens an italic preview tab in the opposite pane and keeps keyboard focus
in the source file list. Moving to another file updates that same preview,
including text, images and video. Double-click its header or run **Keep Preview
Tab** from F1 (⌘Shift+Enter) to bind it to the current file. The command also
works while the source list has focus. Once kept, navigation leaves the tab
unchanged, and ⌘F3 opens a new preview beside it. Directory selections show an
empty preview until another file is selected.
Tab moves into the preview, and Tab from a preview returns to its source tab.
⌘F4 replaces temporary previews in the destination pane with a firm editor;
existing firm tabs remain open. Tab in a firm editor keeps its normal editing
behavior.

Drag tab headers to reorder them or move them to the other pane. Hold
**Option** while dropping into the other pane to duplicate a tab. Two tabs for
the same file share its edits.

## File pane presentations

Each pane has its own **Brief / Large icons / Details / Columns** selector below
its path. Choices are restored on restart and can also be changed in Settings.

- **Large icons** (⌘2): a grid using the selected file-icon theme. Arrows follow
  the grid; only visible tiles are rendered.
- **Details** (⌘3): a single list with Name, Size, Kind and Modified columns.
  Click a column heading to sort.
- **Columns** (⌘4): Finder-style directory browsing. Select a folder to preview
  its contents to the right. Only the first column shows `..`, and only when
  that directory has a parent. Left/Right moves between directory columns;
  horizontal trackpad scrolling, Shift+wheel, the scrollbar and ‹/› buttons
  move across the trail. File commands use the selected column, including while
  a child folder is being previewed. Revealing columns and using ‹/› animates
  with a 220 ms ease-out; direct scrolling stays responsive. Reduce Motion
  disables the transition.
- **Brief** (⌘⇧1): the original compact, column-based presentation. ⌘1 retains
  the existing Brief/Details toggle. Left/Right at a viewport edge scrolls a
  whole column, keeping the cursor's screen position when the list bounds allow.

Selection, marking, F3/F4, transfers, tabs, breadcrumbs and filesystem watches
are shared across all four presentations. Large icons are file-type icons;
image/video previews remain available through F3.
Vertical wheel input moves the cursor and viewport together, keeping the cursor
visible. In Large icons it advances grid rows. Small trackpad deltas accumulate,
and file-list bounds limit movement without changing marked entries.

## ZIP archives

Select a ZIP and press **Enter** or **F3** to browse it in the current pane.
Use **F3** to preview text, images, or hex data and **⌘F3** to preview in the
opposite pane. Previews are limited to 16 MiB per entry. Select files or
folders and press **F5** to extract them. The destination defaults to the
opposite local pane. Extraction uses the regular progress and conflict dialogs.
ZIP panes are read-only and support stored and DEFLATE entries.

## Extensions

**TypeScript 7** is detected from an unpacked `TypeScriptTeam.native-preview`
extension in the DotDir configuration directory's `extensions` folder. Opening
a JavaScript or TypeScript file in **Edit** starts the bundled compiler in LSP
mode. Edit supports diagnostics for unsaved changes, hovers, completions
(**Ctrl+Space**), and Go to Definition (**⌘⌥↓**, **⌘-click**, or **F1**).
DotDir prefers the native architecture and can use the Intel package through
Rosetta on Apple Silicon.

**rust-analyzer** starts when you open a Rust file in Edit. DotDir looks for
the native server at `server/rust-analyzer` inside an unpacked
`rust-lang.rust-analyzer` extension. For a VSIX that ships without a server,
add the matching native binary from the
[rust-analyzer releases](https://github.com/rust-lang/rust-analyzer/releases).
Rust projects use the same diagnostics, hover, completion, and definition
features as TypeScript.

The bundled **Image & Video Viewer** is a native port of the original 1.1.6
extension. Press **F3** on an image or video, then use **Left/Right** to browse the
gallery, **Space** to play/pause, and **Escape** to close. Native macOS video
controls provide seeking and volume; hidden tabs pause playback. SVG, TIFF and
system-decoded AVIF are included. Codec availability follows macOS; unsupported
files show an error and an external-open action. See the
[media viewer notes](extensions/image-viewer/README.md).

For file icons, open **F11 → Load icon theme…** (also available in Settings)
and enter the directory of a VS Code file-icon extension, such as Material Icon
Theme. DotDir loads the first `contributes.iconThemes` entry from `package.json`
and renders the SVG/image assets natively. Filename mappings take precedence over
compound extensions, then standard language IDs and defaults; named folders and
light-theme overrides are supported. Mappings are loaded in the background and
images are cached by GPUI. The selected directory is saved as `icon_theme` in
settings; keep that directory installed. **Use built-in icons** restores the
default appearance. Font-based icon themes and VS Code configuration commands
are not supported; the extension's JavaScript is not executed.

Open F11 and run **Selection Inspector**, or find it with F1. It executes a
QuickJS module and displays native GPUI controls, including interactive buttons.
The bundled example is in `extensions/selection-info`. User extensions are loaded
from `<config>/extensions/<extension-name>/extension.json`.

See [architecture](docs/architecture.md) for API v1, bytecode caching, ownership,
and the draft WASM-panel contract. The original DOM/Monaco extensions require
porting to the native panel API.

## Verify

```sh
cargo test --workspace
cargo check --workspace
cargo fmt --all -- --check
```

This initial native version includes filesystem operations, text editing and
text/hex/image viewing, search, terminals, persisted directory tabs, themes and
local script extensions. Remaining specialized viewers, marketplace/accounts,
full FSS styling and WASM loaders are tracked explicitly
in [architecture](docs/architecture.md). Source provenance and visual references
are recorded in [source reference](docs/source-reference.md).

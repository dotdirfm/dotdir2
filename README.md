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

In a text editor or read-only text viewer, use **F1 → Disable Line Wrap** or
**Enable Line Wrap**. The choice applies to the open document, including when
the same file is shown in both panes.

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

Open the three-dot menu on a pane, or press **⌥F1** for the left pane and
**⌥F2** for the right. **Duplicate Tab** copies the current tab. **Go To** opens
the opposite pane's location, your home directory, a bookmark, a mounted drive,
or a path you enter. Middle-click a saved location or use **Shift+Enter** to
open it in a new tab. Define bookmarks in the `pathAliases` object in
`settings.json` with the bookmark name as the key and a path or filesystem URI
as the value.

**Jump To…** moves the cursor to matching names in the current folder. Use
**↑** and **↓** to cycle through matches, **Enter** to open the selected match,
and **`.*`** to toggle regular expressions.

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

**TypeScript 7** runs the JavaScript client from an unpacked
`TypeScriptTeam.native-preview` VSIX. Enable it through **F11** and set
`js/ts.experimental.useTsgo` to `true` in `vscode.configuration`. Use a VSIX
that matches Node.js's architecture, or point `js/ts.tsdk.path` to the directory
containing the SDK's `tsc` executable. On Apple Silicon this setting can point
to an Intel SDK that runs under Rosetta. Edit supports diagnostics for unsaved
changes, hovers, completions (**Ctrl+Space**), and Go to Definition
(**⌘⌥↓**, **⌘-click**, or **F1**).

**rust-analyzer** runs the JavaScript client from an unpacked
`rust-lang.rust-analyzer` VSIX when enabled through **F11**. The package should
include its native server, or you can set `rust-analyzer.server.path` in
`vscode.configuration`. If DotDir starts from Finder and Cargo is outside the
app's PATH, set `rust-analyzer.server.extraEnv` to an object with a `PATH` entry
that includes Cargo's bin directory.
Rust projects use the same diagnostics, hover, completion, and definition
features as TypeScript. [rust-analyzer releases](https://github.com/rust-lang/rust-analyzer/releases)

**Red Hat YAML** runs through the VS Code extension host. Unpack
`redhat.vscode-yaml` into the extensions directory, enable **YAML** through
**F11**, and open a `.yaml` or `.yml` file in Edit. Its JavaScript client
starts the bundled server and provides schema diagnostics, completions, and
hover documentation. You can associate a local schema with a
`# yaml-language-server: $schema=./schema.json` comment.

**VS Code editor commands** run in a separate, on-demand Node.js process.
DotDir uses Node.js 22.15 or newer from PATH, common macOS install locations,
or nvm. To choose an executable, set `vscode.node` in `settings.json` to its
absolute path.

1. Unpack the [Sort Lines VSIX](https://marketplace.visualstudio.com/items?itemName=Tyriar.sort-lines)
   into a subdirectory of `<config>/extensions` so that the subdirectory
   contains `extension/package.json`. A package with `package.json` directly
   in the subdirectory also works.
2. Open **F11 → Reload Extensions**, then enable **Sort lines**.
3. Open a text file with **F4**, select multiple lines, and run **Sort lines
   (ascending, case sensitive)** from **F1**. Use **⌘Z** to undo the edit.

The host supports command activation, the active editor and primary selection,
document reads, and undoable text edits. It reads settings defaults from the
extension manifest and accepts overrides in `vscode.configuration` using
dotted keys such as `sortLines.ignoreUnselectedLastLine`. Enabled extensions
run with the same OS permissions as DotDir.

VS Code extension labels and runtime messages use the system locale and the
translations bundled with each extension. Set `vscode.locale` in
`settings.json` and restart DotDir to choose a locale such as `fr-CA`.
The command palette also matches the original command name when a translation
is available.

Enabled language extensions activate from their manifest language associations
and `onLanguage` events. The language host supports document open, change,
save, and close events, workspace folders, file watches, diagnostic collections,
and hover, completion, and definition providers. Literal `workspaceContains`
entries in the manifest identify project roots for language extensions.

Language extensions can open a single-selection picker with `window.showQuickPick`.
Workspace settings changed by a language extension are stored in DotDir's
profile for that extension and workspace.

Open **F11 → Extension Logs** to see recent extension output, startup events,
and errors from this DotDir run. Use **Copy Logs** to copy the log history.
Open **F11 → Browse Logs**, or enter `ext-logs://` in **⌘L**, to browse the same
logs in a file panel. Each extension has a folder with a read-only `output.log`
containing its retained output for this session. Open it with **Enter** or **F3**.

Open **F11 → Open DotDir Log** for persistent app and extension errors.
DotDir appends these errors to `dotdir.log` next to `settings.json`, including
VS Code command IDs and JavaScript stack traces when available. The default
path on macOS is `~/Library/Application Support/dev.dotdir.native/dotdir.log`.

Node.js extensions can register custom protocols for browsing folders and
previewing files in DotDir. [Filesystem extension API](docs/architecture.md#filesystem-extensions)

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

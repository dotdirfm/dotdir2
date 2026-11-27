# DotDir Native

A native Rust/GPUI port of DotDir. The interface follows the existing app's
Ayu colors, compact column-based file panes, tabs, breadcrumbs, command line and
function-key bar. Built-in extensions are compiled into the app. DotDir
JavaScript extensions use QuickJS.

## Run

```sh
cargo run --release -- /path/to/start
```

Omit the path to restore the previous directory tabs. The project pins Rust
1.100.0 and GPUI Kit 0.11.0. Rustup installs the project toolchain automatically.
On macOS, install full Xcode and its command-line tools for GPUI/Metal builds.

Build a runnable package on the platform where you plan to run it:

```sh
cargo dist        # optimized release
cargo dist debug  # faster build for development
```

To pull the current branch from its upstream and build in one step, run the
script for your platform. Both scripts require a fast-forward pull before
building.

**Windows PowerShell**

```powershell
.\scripts\pull-and-build.ps1
.\scripts\pull-and-build.ps1 debug
```

**macOS or Linux**

```sh
sh scripts/pull-and-build.sh
sh scripts/pull-and-build.sh debug
```

| Platform | Run this artifact |
| --- | --- |
| macOS | `dist/macos/DotDir Native.app` |
| Windows | `dist/windows/dotdir.exe` |
| Linux | `dist/linux/dotdir` |

Each build replaces that platform's package. The built-in extensions are part
of the executable. The macOS bundle is signed ad hoc for local use. Linux and
Windows builds need the platform prerequisites described by
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

Release builds optimize for executable size, use full link-time optimization,
and strip symbols. They take longer to compile. A Rust panic terminates the
process; ordinary application errors and JavaScript exceptions remain handled.
Development and test builds retain panic unwinding.
See the [release size measurements](docs/release-size.md) for the build comparison.

### Test packages

Run **Test packages** from the repository's Actions tab and download the
artifacts from the run summary. Changes to the workflow, packaging files,
or build configuration also trigger a run. The workflow uploads test
artifacts and does not publish a GitHub Release.

| Platform | Download | Install |
| --- | --- | --- |
| macOS Apple Silicon | `DotDir-macOS-arm64.dmg` | Open the disk image and drag DotDir Native to Applications. |
| Windows x64 | `DotDir-Windows-x64-Setup.exe` | Run the per-user installer. |
| Linux x64 | `DotDir-Linux-x86_64.AppImage` | Make the AppImage executable with `chmod +x` and run it. |

The DMG contains an ad-hoc signed app, and the Windows installer is unsigned.
The Linux job builds on Ubuntu 24.04.

### GitHub Releases

Push a version tag in the form `vMAJOR.MINOR.PATCH` (for example, `v0.1.0`)
on the commit you want to release. The **Release desktop packages** workflow
builds and verifies the three packages above, then publishes them together on
GitHub Releases with generated release notes. The release uses the same
package and signing settings as the test builds.

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

Use **F1 → Show Keystrokes** to briefly display shortcuts and navigation keys
above the function-key bar during a presentation. **Hide Keystrokes** turns it
off. The overlay starts off each time you launch DotDir.

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

## Archives

The bundled **ZIP**, **7-Zip**, and **TAR** providers use DotDir's shared archive
API and appear under **F11 → Extensions**. TAR browsing includes plain TAR and
gzip, bzip2, xz, and Zstandard tarballs.

Select an archive and press **Enter** or **F3** to browse it in the current pane.
Formats can be mixed within the same nested path, up to eight levels deep.
Use **F3** to preview text, hex, images, or supported videos, and **⌘F3** to
preview in the opposite pane. Use **Left/Right** in the media viewer to browse
images and videos in the same archive folder.

Browsing and previews use no temporary files. Text and image previews of
archive entries are limited to 16 MiB. Hex, video, and nested archive reads use
direct seeks for plain TAR and stored ZIP or 7z entries. Other entries that
need random access use up to 64 MiB each in memory. System AVIF decoding is
available on macOS. Video playback on Windows and Linux requires libmpv.

Press **F3** to view a binary file and **F4** to edit a local binary file.
**View as Hex** and **Edit as Hex** in the command palette can also open any
supported file as bytes. Use the offset field to jump to a hexadecimal address.
**View as Dump** fills the pane with one character per byte. It shows printable
ASCII as-is, ASCII control bytes as DOS-style glyphs, NUL as a blank, and bytes
above `0x7f` as bullets. Use **Hex** and **Dump** in the read-only byte viewer to
switch views without changing the current byte offset.
The byte inspector shows the current byte in hexadecimal and decimal. Choose
2, 4, or 8 bytes to read an unsigned integer starting at the cursor in little
or big endian order.
Type hex digits to overwrite bytes, then press **F2** to save only the changed
ranges. Hex previews read data on demand, and local binary files can exceed
the text editor's 16 MiB limit.

ZIP, 7z, and TAR support browsing, previews, extraction, and creation. ZIP
also supports editing existing archives.

Press **F4** to edit text inside a local ZIP and **⌘S** to save it back.
Saving checks for external changes and replaces the archive only after the
rewrite succeeds. Nested saves rebuild inner ZIPs in memory with the same
64 MiB limit.

With **TypeScript 7** enabled in **F11**, editing TypeScript or JavaScript in
a ZIP enables diagnostics, completion, hover, and Go to Definition. The
adapter serves source files and unsaved edits from memory and the archive.
The workspace is never unpacked to disk.

Use **F5** to copy and **F6** to move files or folders between a local directory
and a writable archive, or between archive directories. Open the destination
in the opposite pane before transferring. **F5 → Extract** also works from
read-only archives. A move removes a source only after its destination has
been written. A folder with skipped conflicts stays at the source.

Inside a writable ZIP, use **Shift+F4** to open or create a file, **F7** to make
a folder, or **F8** to permanently delete the selected entries. These operations
also work in nested ZIPs.

To pack a selection, open **F1 → Create Archive…** and choose a format and
local output path. ZIP offers storage and three DEFLATE levels. 7z offers
storage and three LZMA2 presets. Both support optional AES-256 passwords.
A 7z password protects both file contents and names. TAR offers uncompressed,
gzip, bzip2, xz, and Zstandard output. The chosen codec sets the filename suffix.

DotDir asks for a password when an operation needs encrypted ZIP or 7z contents,
or when it opens an encrypted 7z header.
Passwords are encrypted in memory and remembered for 15 minutes by default.
Change the duration under **F9 → Archive password cache**, or set
`archive_password_cache_minutes` in `settings.json`. Set it to 0 to keep a
password only for the operation in progress. Passwords are never stored on disk.

## Extensions

Open **F11 → Browse Open VSX…** to browse or search the registry by name,
extension ID, or Open VSX page URL. Select a result to read its README and
install a compatible release. README screenshots and animated GIFs render
inline. Direct video embeds play on macOS, with a browser link also available.
Use **Installed** to find local packages,
enable or disable executable extensions, and apply installed color or file
icon themes. Extension commands and features depend on DotDir's VS Code API
support.

Use **F11 → Install from VSIX…** to install a local VSIX from a publisher or
private registry. Choose the package for your platform, then enable the
extension through **F11** if it provides commands or a language server.

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
gallery, **Space** to play/pause, and **Escape** to close. The video controls
provide seeking and volume, and hidden tabs pause playback. macOS uses AVKit
and the system AVIF decoder. On Windows, put an architecture-matched
`libmpv-2.dll` next to DotDir or in its profile directory. On Linux, install the
system package that provides `libmpv.so.2`. On either platform you can set
`DOTDIR_LIBMPV` to the library’s absolute path. See [mpv’s installation
guide](https://mpv.io/installation/) for runtime sources. Unsupported local
files show an external-open action. See the
[media viewer notes](extensions/image-viewer/README.md).

For VS Code colors, open **F9 → Load VS Code Color Theme…** and enter a JSON,
JSONC, or TextMate theme file, or an unpacked VS Code extension directory.
Choose a variant when the extension includes several. DotDir restores the
selected theme on startup and maps its syntax styles to Tree-sitter captures.

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

Open **F11 → Selection Inspector**, or find it with F1. The built-in inspector
opens in the opposite pane and can refresh the selection without leaving the
tab. **Selection: Show Summary** reports the selected item count.

DotDir loads JavaScript extensions from
`<config>/extensions/<extension-name>/extension.json`. Copy
[`examples/selection-info`](examples/selection-info) there to try the QuickJS
version of the inspector.

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

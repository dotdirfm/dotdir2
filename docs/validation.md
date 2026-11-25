# Validation — 25 November 2026

Validated on this Mac with the project-pinned Rust 1.100.0 toolchain.

- `cargo test --workspace`: 72 passing tests (32 original filesystem-core tests,
  7 native model/operation/PTY tests, 3 icon-theme mapping/path tests,
  5 QuickJS/protocol/cache/viewer tests, 2 media-gallery tests,
  1 shortcut-context regression test, 8 presentation/column/grid/wheel tests,
  3 scroll easing/retargeting/interruption tests, 3 command-availability tests,
  2 modifier/function-bar keymap tests, 4 preview lifecycle tests,
  1 Keep Preview command-availability test, 1 preview Tab binding test).
- `cargo clippy -p dotdir -p dotdir-extension-host --no-deps`: clean.
- `cargo fmt --all -- --check`: clean.
- `scripts/bundle-macos.sh`: builds a runnable release `.app`.
- `codesign --verify --deep --strict`: local ad-hoc bundle verifies.
- Dependency lockfile contains GPUI Kit and rquickjs; no Deno or V8 runtime.
- Size-optimized release (`z`, full LTO, abort on panic, stripped) measured
  13.38 MiB before the media port versus 28.99 MiB previously; the `s` comparison measured 16.22 MiB.
  The selected release passed native editor, icon, extension-panel, PTY, hex
  and PNG smoke checks. Details are in `release-size.md`.

Editor performance follow-up: the initial review used an unoptimized debug
bundle. Bundling now defaults to release, and the run instructions use release.
Modified-state checks borrow Rope chunks and cache the result until the next
edit or successful save; cursor movement and redraws no longer materialize the
whole document. Text changes notify the app shell only when the modified flag
changes. The editor continues to redraw its own content independently.
The optimized, signed bundle was launched and checked with a 12,000-line Rust
fixture (about 657 KiB): typing, scrolling, undo back to the clean indicator,
save back to the clean indicator, and on-disk Unicode content preservation.
These are interaction checks, not a measured frame-time benchmark.

Interactive checks used isolated fixtures under `/tmp/dotdir-native-smoke`:

- Directory navigation, pane switching, column navigation, directory tabs and
  command-palette filtering.
- Real copy to the opposite pane; existing-destination conflict and Skip;
  keyboard selection/acceptance of conflict choices.
- Rust syntax highlighting, text entry, dirty-tab indicators, close confirmation,
  cancellation of that confirmation, save, and on-disk saved-text verification.
- PTY shell startup and interactive `printf` output in the native terminal.
- Recursive filename and content search, with keyboard reveal of a nested result.
- Opening a PNG in a native image tab.
- Loading the bundled Selection Inspector through QuickJS, rendering its native
  table, and invoking its refresh callback (counter and selection metadata update).
- Window resizing and comparison against the existing app's CSS/theme tokens.
- Loading the installed Material Icon Theme 4.31.0 through F11, including its
  814 SVG definitions; native colored file/folder rendering and selection
  restored after restarting the release app. No VS Code extension JavaScript
  is executed.

The original working tree was used as a read-only source reference. Its existing
application process was left running; only the additional reference instances
started for this task were stopped. Remaining feature and platform boundaries
are listed in `architecture.md`.

Native image/video port checks used `/tmp/dotdir-media-smoke`: PNG, SVG, AVIF
and TIFF rendering; MP4/H.264 playback; native timeline seeking; Space to
pause; gallery keyboard and mouse navigation; gallery updates on file creation;
live SVG content reload; Escape closing the viewer; hidden-tab pause/resume;
global command shortcuts; replay after reaching the end; and recovery from a
damaged MP4. The final release measures 13.44 MiB before signing (13.38 MiB signed).
Native video uses macOS AVKit; container
patterns do not guarantee support for every codec. Windows/Linux video and
AVIF backends remain unimplemented.

Screenshot: [`screenshots/media-viewer.png`](screenshots/media-viewer.png).

F3 focus regression: the View and Quick View shortcuts now match the DotDir
context so they follow the selected file even when the command-line input has
focus, consistent with the function-key bar. The keymap regression fails with
the former FilePane-only context and passes for both F3 and Command-F3 with
file-list and input focus. Existing modal and selected-file guards still apply.

Verified in the rebuilt release: select a PNG, focus the command line with
Control-L, then F3 opens it in the same pane; Command-F3 opens it in the
opposite pane. The ad-hoc bundle signature verifies, and Clippy is clean.

File pane presentations: validated Large icons, sortable Details, Finder-style
Columns and the retained Brief view with isolated fixtures under
`/tmp/dotdir-panels-smoke`. Checks covered grid arrow navigation and the final
partial row, scrolling (cursor-following behavior verified below), Unicode names, independent
pane choices restored on restart, deep column navigation and branch replacement,
live updates in a child column, F3 from a nested column, and an F5 copy whose
contents matched the selected source on disk. SVG theme icons are cached at
the requested device-pixel size. No new crate dependencies were added.

Screenshot: [`screenshots/panel-presentations.png`](screenshots/panel-presentations.png).

Column reveal and toolbar scrolling use a frame-driven 220 ms cubic ease-out.
Tests verify the midpoint and final offsets, minimum visibility, continuous
retargeting, repeated requests, deferred initial layout and cancellation on
manual scrolling. Trackpad/wheel input remains direct, and Reduce Motion skips
the transition. An isolated release instance verified toolbar scrolling between
the retained directory columns; the test instance was then closed. The existing
user instance was left open.

The Columns scrollbar overlay now belongs to a fixed viewport wrapper rather
than the scrolling strip. In a four-column trail wider than the pane, dragging
the thumb to each end reveals the first and last columns, and its track stays
within the pane. The selected file and neighboring pane remain unchanged.
Window resizing also updates the thumb proportion to the viewport width.
Screenshot: [`screenshots/columns-scrollbar.png`](screenshots/columns-scrollbar.png).

All modal backdrops use GPUI mouse occlusion. In an isolated app instance,
clicking files, changing presentation, clicking F12, right-clicking and scrolling
outside a Make directory prompt left the underlying panes unchanged. Text entry
inside the prompt and its Cancel button remained functional; no directory was
created. Screenshot: [`screenshots/modal-backdrop.png`](screenshots/modal-backdrop.png).

Only the first directory column retains `..` when it has a parent. A model
regression covers child reloads, keyboard selection, empty child directories,
restoring the parent entry without changing the focused file when collapsing
the trail, and absence of a parent entry at the filesystem root.

Command availability checks covered selected files/folders/parent entries,
busy operations, editable/read-only documents, empty directories, hidden panes,
media gallery boundaries and video readiness/failure. In an isolated native app,
F1 hid media commands in file panes and Play/Pause in images and damaged video;
playable video offered Play/Pause and Reload. F1 execution and Space toggled
playback (captured video frames advanced while playing and stayed identical
while paused). A custom global Make directory binding worked in a file pane
and was rejected in the image viewer. F3 still opened the selected file with
command-line focus, gallery arrows worked, and Save appeared only for editable
documents. The test app was closed without editing fixtures.

The bottom function-key bar resolves each F-key through GPUI's active keymap
using the held modifiers and focused keyboard context. Regression tests cover
Alt/Control/Command/Shift, combinations, release, input focus, extension command
IDs and user overrides (including unavailable overrides without fallback).
An isolated native instance verified normal startup labels, unavailable actions
on `..`, Alt-clicking F7 opening Find Files, a custom Shift+F7 opening Settings,
and normal labels returning after modifier release. Initial keymap inspection
waits until the focused element is in GPUI's rendered dispatch tree.

Opposite-pane previews: an isolated native app under `/tmp/dotdir-preview-check`
verified Cmd+F3 retaining source focus, italic headers, one tab reused across
text/image/video selections, video readiness without stealing focus, and clearing
the preview on `..`. F1 Keep Preview Tab, Cmd+Shift+Enter and header double-click
all promoted the tab; subsequent source navigation left it unchanged and Cmd+F3
opened a new preview beside the kept tabs. Closing preview and kept text tabs
removed the correct tab. A damaged video showed a stable error and recovered
when source selection changed; decoder events retain the source path spelling
for macOS path aliases. Both isolated instances were closed. Regression tests cover stable tab identity across
content changes, source isolation, stale reads, pinning during a pending load,
independent requests for the next preview, and command availability.

Screenshot: [`screenshots/opposite-preview.png`](screenshots/opposite-preview.png).

Preview follow-up checks under `/tmp/dotdir-preview-followup`: Cmd+F3, Tab, Tab
returned to the source file list for text, image and native video previews;
an arrow then changed source selection and updated the preview. Cmd+F3 then
Cmd+F4 removed the italic tab, opened a firm editor and retained other firm tabs.
Tab still indented in a firm text editor (undone without saving). A FIFO fixture
held a read pending while focus moved into the preview; after completing the
read, Tab returned to the source. Unit coverage includes a changed/missing source
tab, temporary-tab removal, firm-tab preservation and keymap precedence over
editor Tab bindings. The written reverse shortcut order (Cmd+F4 then Cmd+F3)
also left the firm editor unchanged: preview requires a file-list target.

File cursor behavior was compared with the reference `packages/file-list`
ColumnsScroller and cursor command handlers. Wheel input now moves cursor and
viewport together; Brief Left/Right at an edge advances the viewport by one
column. Tests cover three visible columns, both ends of the list, a partial
final column, empty directories, marked entries, fractional trackpad input and
icon-grid alignment. In an isolated app under `/tmp/dotdir-cursor-check`, wheel
input advanced both indices by the same amount. At a two-column viewport edge,
Right advanced cursor and viewport by 22 entries, and the highlight stayed at
the same pixel bounds: `(600, 250)`–`(1194, 301)`. Comparing selected/unselected
screenshots also found identical filename and size glyph bounds. Cursor borders
now paint over rows instead of changing their layout. The test app was closed.

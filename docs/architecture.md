# Native DotDir

The desktop application is Rust and GPUI Kit. There is no browser shell, Electron,
Tauri, Deno or V8. GPUI Kit supplies native text inputs and its source editor;
the file panes and application chrome are custom GPUI elements.

## Ownership

- `dotdir-core` is the original filesystem engine, copied from the current
  reference working tree. It contains copy/move/delete/search/watch/rename logic.
- `dotdir` owns panes, directory tab identities, focus, dialogs and documents.
  Filesystem reads, search, transfer and save operations run on worker threads.
  Generation numbers reject stale directory and document-open results.
- Directory tabs retain a stable browser ID and a trail of directory columns.
  Each column owns its cursor, marks, read generation and filesystem-watch ID;
  commands resolve against the active column, not the rightmost preview.
  Replacing a branch removes its watches, and late reads for removed IDs are
  ignored. Changing out of the Columns presentation retains the active directory.
- Pane presentations are persisted independently. Legacy `brief` preferences
  migrate to Brief or Details. Grid and list views render only the visible range;
  horizontal directory columns use GPUI scroll handles and a native GPUI scrollbar.
  Per-tab column motion uses animation-frame callbacks with cubic ease-out;
  repeated navigation retargets from the current offset. Manual input cancels
  motion, and the system Reduce Motion setting bypasses it.
  Vertical file-list scrolling advances both cursor and viewport, retaining
  their relative offset until a boundary is reached. Fractional wheel deltas
  accumulate per directory; icon grids advance whole rows. Brief horizontal
  cursor commands shift the viewport by a full column at its edges. Cursor
  borders are absolute overlays and do not affect row content layout.
- Built-in command metadata includes typed availability rules, distinct from
  shortcut focus contexts. The active pane supplies a capability snapshot used
  by both F1 filtering and the central execution guard. Palette focus does not
  replace its target; execution rechecks current capabilities. Media shortcuts
  bubble through the same dispatcher, including actions reserved by native video
  views. Gallery/decoder changes refresh palette availability without moving focus.
  API v1 extension commands remain global, subject to the shared modal gate.
- Each document retains an editor entity and its saved byte snapshot. Tab changes
  preserve edits and undo history. Saves capture the requested text and update the
  saved snapshot on completion, so edits made during a save remain dirty. Closing
  or quitting checks dirty buffers. An external change detected before a save is
  rejected. The check and write are not an atomic filesystem compare-and-swap.
- Each tab has its own stable identity, preview flag and optional source-tab ID,
  separate from its replaceable viewer content. Opposite-pane previews follow
  the source directory's focused file without changing keyboard focus. Preview
  loads have per-tab request versions; stale results are ignored, and keeping
  a pending preview freezes its path while allowing its current load to finish.
  The Keep Preview Tab command and header double-click share the same execution
  path. Promotion clears the preview flag and source binding. File tabs remain
  session-local, as before; directory paths and presentation preferences persist.
  Preview-specific Tab bindings route through Switch Panel, including inside
  the editor, and resolve the stored source tab rather than just the opposite
  pane's active index. Pending previews retain focus when their content arrives.
  Quick edit removes destination previews and rejects their late load results;
  firm tabs are preserved, with a directory fallback if no other tabs remain.
- One operation owns its progress receiver, cancellation token and conflict reply.
  Conflicts block only the operation worker. Cancellation waits for settlement
  before refreshing both panes. Completed filesystem changes are not rolled back.
- Terminal sessions own real PTYs, Alacritty terminal grids, and parser workers.
  Rendering and keyboard input are native. Closing a terminal kills its child.
- Each JavaScript extension has a QuickJS runtime on the extension worker.
  The application only receives serializable contributions and UI data.
- Media viewer entities own their gallery snapshot, file watcher, GPUI image
  source or native AVKit player. Directory enumeration runs in the background;
  generation checks reject stale snapshots. Native players pause when hidden or
  dropped, and are never passed to QuickJS.

## Extension API v1

An extension directory has an `extension.json` manifest and a JavaScript module.
Top-level code calls `dotdir.registerCommand(id, title, callback)` or
`dotdir.registerPanel(id, title, callback)`. The callback receives a context
containing the active path, opposite path and selected entry metadata, plus an
event (`null` when opened, `{id: "button-id"}` for panel buttons). Return an object
with `message` and/or `panel`. See `extensions/selection-info`.

`dotdir.registerViewer(id, title, {renderer: "media", patterns, priority})` adds
a declarative file viewer. Patterns match basenames without case sensitivity;
the highest priority wins. The host validates and owns the renderer, navigation,
file watching and playback. Viewer registrations are not commands and do not
expose filesystem, process or media objects to JavaScript. The initial renderer
is `media`; see `extensions/image-viewer` for the port of the old image/video
extension. This typed boundary can also be used by a future WASM transport.

The semantic panel tree supports column, row, text, button and table nodes.
The host validates depth and dimensions, then renders real GPUI controls. This is
also the intended contract for future WASM panels; `panels.wit` is a draft
transport contract. The WASM panel loader is not implemented in this version.
Canvas rendering and virtual table data requests need future protocol additions.

The VM exposes no filesystem, process, network, Node or DOM APIs. Extension
callbacks are synchronous in v1 and have a 250 ms execution budget, a 32 MiB VM
memory limit, and a 512 KiB stack limit. Module initialization has a 2 s budget.
These are interpreter controls, not operating-system process isolation.

QuickJS bytecode is generated locally from source and cached in a private
application cache directory. Keys include source, VM binding version, host API,
target OS and architecture. Checksums detect accidental cache corruption.
Bytecode from extension packages is never accepted. Like other native caches,
this cache assumes the user's application cache directory is trusted; the checksum
is not an authenticity check against another process running as the same user.
Build-option or vendored engine changes must bump `CACHE_ABI`.

Install local extensions under the native config directory's `extensions/` folder.
Reload through the Extensions dialog. This API does not claim compatibility with
the original DOM-based viewers or the VS Code shim.

## Compatibility boundaries

This port covers the native file-manager shell, core file operations, text/hex/image
viewing, text editing, recursive name/content search, real terminal sessions,
settings/session persistence and local JavaScript commands/panels. The original
website/demo, account/marketplace integration, full FSS styling language, external
LSP process integration, WASM filesystem-provider loading and specialized PDF,
SQLite and archive surfaces are separate remaining porting work. Image/video
viewing now has a native renderer; video and AVIF currently use macOS APIs. The
component editor has syntax highlighting and editor search; no LSP connection is
claimed merely because its upstream component supports LSP providers.

Native text/hex opening is currently capped at 16 MiB. Search retains the first
10,000 matching paths. User keybinding changes apply on restart. Platform-specific
validation in this checkout is on macOS; Windows/Linux backends are dependency
supported but have not been runtime-tested here.

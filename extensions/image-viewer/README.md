# Image & Video Viewer — native port

Rewritten from the behavior of `dotdir-image-viewer` 1.1.6. The original installed
package and its browser bundle are untouched.

The built-in file patterns and priority are registered in Rust. Third-party
QuickJS extensions can still contribute patterns using
`dotdir.registerViewer(id, title, {renderer: "media", patterns, priority})`.
The Rust host matches file names, chooses the highest-priority viewer and renders
the native surface. No DOM, iframe, MediaSource, mp4box or full-file JavaScript
buffers are involved. This is a native renderer contribution, not compatibility
with the old viewer's CommonJS/DOM API or an arbitrary native-library loader.

- Select a media file and press **F3**; **⌘F3** opens it in the opposite pane.
- **Left/Right** and the arrows over the preview move through matching files in
  the directory's sort order. The counter shows the current gallery position.
  Navigation stops at the ends and updates the source pane's cursor.
- **Space** plays/pauses video. The player includes seek and volume controls.
  **⌘R** reloads; **Escape** closes the viewer.
- Directory changes update the gallery. Changes to the current file reload its
  content, including atomic file replacements. Watch events are coalesced.
- Hidden video tabs pause; closing the tab pauses and releases the player.
- Decode failures show an error and an **Open externally** action.

PNG, JPEG, GIF, BMP, WebP, ICO, SVG and TIFF use GPUI's image renderer. AVIF uses
the system image decoder on macOS. macOS video uses an embedded `AVPlayerView`.
Windows and Linux use libmpv's software render API in a background worker and
present its frames through GPUI, including under Wayland. libmpv plays audio
and reads local and archive videos through DotDir's seekable file interface.
The software rendering surface is limited to 960 × 540 for preview performance.
The libmpv backend also accepts MKV, AVI, MPEG, WMV and FLV. Codec support
depends on the installed playback backend.

The libmpv runtime is installed separately. On Windows, use a matching
`libmpv-2.dll` next to the executable or in the DotDir profile; on Linux, install
the package that provides `libmpv.so.2`. Set `DOTDIR_LIBMPV` to an absolute library
path to override discovery. When playback is unavailable, local files can be
opened externally.

[Apple’s native playback component](https://developer.apple.com/documentation/avkit/avplayerview).

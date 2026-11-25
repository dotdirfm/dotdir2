# Image & Video Viewer — native port

Rewritten from the behavior of `dotdir-image-viewer` 1.1.6. The original installed
package and its browser bundle are untouched.

The QuickJS module registers a viewer contribution using
`dotdir.registerViewer(id, title, {renderer: "media", patterns, priority})`.
The Rust host matches file names, chooses the highest-priority viewer and renders
the native surface. No DOM, iframe, MediaSource, mp4box or full-file JavaScript
buffers are involved. This is a native renderer contribution, not compatibility
with the old viewer's CommonJS/DOM API or an arbitrary native-library loader.

- Select a media file and press **F3**; **⌘F3** opens it in the opposite pane.
- **Left/Right** and the arrows over the preview move through matching files in
  the directory's sort order. The counter shows the current gallery position.
  Navigation stops at the ends and updates the source pane's cursor.
- **Space** plays/pauses video. macOS AVKit supplies playback, seek and volume
  controls. **⌘R** reloads; **Escape** closes the viewer.
- Directory changes update the gallery. Changes to the current file reload its
  content, including atomic file replacements. Watch events are coalesced.
- Hidden video tabs pause; closing the tab pauses and releases the player.
- Decode failures show an error and an **Open externally** action.

PNG, JPEG, GIF, BMP, WebP, ICO, SVG and TIFF use GPUI's image renderer. AVIF uses
the system image decoder on macOS. Videos use an embedded `AVPlayerView`, reading
the local file directly rather than copying it into the VM. No codec library is
bundled. Video/container support therefore depends on the installed macOS media
frameworks: the declared MP4/M4V/MOV/WebM/Ogg patterns are candidates, not a claim
that every codec in these containers is decodable. Unsupported files show an
error. Native video and AVIF are currently macOS-only; other platforms expose
the error/external-open action while retaining the GPUI image formats.

[Apple’s native playback component](https://developer.apple.com/documentation/avkit/avplayerview).

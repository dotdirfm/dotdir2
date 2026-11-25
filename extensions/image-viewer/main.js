// Native port of dotdir-image-viewer 1.1.6. The host owns decoding and playback.
dotdir.registerViewer("image-viewer", "Image & Video Viewer", {
  renderer: "media",
  priority: 10,
  patterns: [
    "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.webp", "*.ico",
    "*.svg", "*.avif", "*.tiff", "*.tif",
    "*.mp4", "*.m4v", "*.webm", "*.ogv", "*.ogg", "*.mov"
  ]
});

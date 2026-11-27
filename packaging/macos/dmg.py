from pathlib import Path

app = Path(defines["app"]).resolve()
files = [str(app)]
symlinks = {"Applications": "/Applications"}
hide_extensions = [app.name]

background = str(Path(defines["background"]).resolve())
window_rect = ((180, 160), (600, 330))
show_toolbar = False
show_sidebar = False
show_status_bar = False
show_pathbar = False
show_tab_view = False
icon_size = 96
text_size = 13
icon_locations = {app.name: (168, 178), "Applications": (432, 178)}

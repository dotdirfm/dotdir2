use gpui_kit::{Action, App, KeyBinding, KeyContext, Keymap, Keystroke, Modifiers};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = dotdir)]
pub struct RunCommand {
    pub id: String,
}

/// Capabilities of the active pane, independent of keyboard focus. Opening F1
/// moves focus into an input without changing this command target.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandState {
    pub panels_visible: bool,
    pub directory: bool,
    pub entry: bool,
    pub real_entry: bool,
    pub file: bool,
    pub items: bool,
    pub selection: bool,
    pub parent: bool,
    pub can_save: bool,
    pub has_tab: bool,
    pub multiple_tabs: bool,
    pub preview: bool,
    pub operation_idle: bool,
    pub media: Option<MediaState>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MediaState {
    pub playable_video: bool,
    pub previous: bool,
    pub next: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Availability {
    Always,
    Panels,
    Directory,
    Entry,
    RealEntry,
    File,
    Items,
    Transfer,
    Parent,
    RenameOrSave,
    Save,
    Tab,
    MultipleTabs,
    Preview,
    Media,
    PreviousMedia,
    NextMedia,
    PlayableVideo,
}
impl Availability {
    pub fn matches(self, state: &CommandState) -> bool {
        match self {
            Self::Always => true,
            Self::Panels => state.panels_visible,
            Self::Directory => state.directory,
            Self::Entry => state.directory && state.entry,
            Self::RealEntry => state.directory && state.real_entry,
            Self::File => state.directory && state.file,
            Self::Items => state.directory && state.items,
            Self::Transfer => state.directory && state.selection && state.operation_idle,
            Self::Parent => state.directory && state.parent,
            Self::RenameOrSave => {
                (state.directory && state.real_entry && state.operation_idle) || state.can_save
            }
            Self::Save => state.can_save,
            Self::Tab => state.has_tab,
            Self::MultipleTabs => state.multiple_tabs,
            Self::Preview => state.panels_visible && state.preview,
            Self::Media => state.media.is_some(),
            Self::PreviousMedia => state.media.is_some_and(|m| m.previous),
            Self::NextMedia => state.media.is_some_and(|m| m.next),
            Self::PlayableVideo => state.media.is_some_and(|m| m.playable_video),
        }
    }
}

pub struct Command {
    pub id: &'static str,
    pub title: &'static str,
    pub key: &'static str,
    pub context: &'static str,
    pub availability: Availability,
}
impl Command {
    pub fn available(&self, state: &CommandState) -> bool {
        self.availability.matches(state)
    }

    pub fn keybar_label(&self, editor: bool) -> &str {
        match self.id {
            "palette" => "Commands",
            "rename" if editor => "Save",
            "rename" => "Rename",
            "view" => "View",
            "edit" => "Edit",
            "copy" => "Copy",
            "move" => "Move",
            "mkdir" => "Mkdir",
            "trash" => "Trash",
            "quit" => "Quit",
            "extensions" => "Plugins",
            "terminal" => "Terminal",
            "quick-view" => "Quick View",
            "quick-edit" => "Quick Edit",
            "new-file" => "New File",
            "sort-name" => "Sort Name",
            "sort-size" => "Sort Size",
            "sort-date" => "Sort Date",
            "sort-extension" => "Sort Type",
            _ => self.title,
        }
    }
}

/// Resolve the actual winning binding, including context and user overrides.
/// An unavailable winner must not fall back to a shadowed default command.
pub fn function_key_command(
    keymap: &Keymap,
    contexts: &[KeyContext],
    number: usize,
    modifiers: Modifiers,
) -> Option<String> {
    let keystroke = Keystroke {
        key: format!("f{number}"),
        modifiers,
        ..Default::default()
    };
    let (bindings, pending) = keymap.bindings_for_input(&[keystroke], contexts);
    if pending {
        return None;
    }
    bindings
        .first()?
        .action()
        .as_any()
        .downcast_ref::<RunCommand>()
        .map(|command| command.id.clone())
}
pub const COMMANDS: &[Command] = &[
    Command {
        id: "keep-preview",
        title: "Keep Preview Tab",
        key: "cmd-shift-enter",
        context: "DotDir",
        availability: Availability::Preview,
    },
    Command {
        id: "palette",
        title: "Show All Commands",
        key: "f1",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "rename",
        title: "Rename / Save File",
        key: "f2",
        context: "DotDir",
        availability: Availability::RenameOrSave,
    },
    Command {
        id: "view",
        title: "View File",
        key: "f3",
        context: "DotDir",
        availability: Availability::File,
    },
    Command {
        id: "edit",
        title: "Edit File",
        key: "f4",
        context: "FilePane",
        availability: Availability::File,
    },
    Command {
        id: "copy",
        title: "Copy to Opposite Panel",
        key: "f5",
        context: "FilePane",
        availability: Availability::Transfer,
    },
    Command {
        id: "move",
        title: "Move to Opposite Panel",
        key: "f6",
        context: "FilePane",
        availability: Availability::Transfer,
    },
    Command {
        id: "mkdir",
        title: "Make Directory",
        key: "f7",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "trash",
        title: "Move to Trash",
        key: "f8",
        context: "FilePane",
        availability: Availability::Transfer,
    },
    Command {
        id: "settings",
        title: "Settings",
        key: "f9",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "quit",
        title: "Close Window",
        key: "f10",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "extensions",
        title: "Extensions",
        key: "f11",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "terminal",
        title: "Toggle Terminal",
        key: "f12",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "switch",
        title: "Switch Panel",
        key: "tab",
        context: "FilePane || PreviewTab || (PreviewTab > Input)",
        availability: Availability::Panels,
    },
    Command {
        id: "up",
        title: "Cursor Up",
        key: "up",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "down",
        title: "Cursor Down",
        key: "down",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "left",
        title: "Previous Column",
        key: "left",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "right",
        title: "Next Column",
        key: "right",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "first",
        title: "First Entry",
        key: "home",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "last",
        title: "Last Entry",
        key: "end",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "page-up",
        title: "Previous Page",
        key: "pageup",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "page-down",
        title: "Next Page",
        key: "pagedown",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "open",
        title: "Open Entry",
        key: "enter",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "parent",
        title: "Go to Parent Directory",
        key: "backspace",
        context: "FilePane",
        availability: Availability::Parent,
    },
    Command {
        id: "home",
        title: "Go to Home Directory",
        key: "cmd-home",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "refresh",
        title: "Refresh Directory",
        key: "cmd-r",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "hidden",
        title: "Toggle Hidden Files",
        key: "cmd-.",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "new-tab",
        title: "New Directory Tab",
        key: "cmd-t",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "close-tab",
        title: "Close Tab",
        key: "cmd-w",
        context: "DotDir",
        availability: Availability::Tab,
    },
    Command {
        id: "next-tab",
        title: "Next Tab",
        key: "ctrl-tab",
        context: "DotDir",
        availability: Availability::MultipleTabs,
    },
    Command {
        id: "mark",
        title: "Mark / Unmark Entry",
        key: "space",
        context: "FilePane",
        availability: Availability::RealEntry,
    },
    Command {
        id: "mark-next",
        title: "Mark Entry and Move Down",
        key: "insert",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "select-all",
        title: "Mark All Entries",
        key: "cmd-a",
        context: "FilePane",
        availability: Availability::Items,
    },
    Command {
        id: "mark-down",
        title: "Select Down",
        key: "shift-down",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "mark-up",
        title: "Select Up",
        key: "shift-up",
        context: "FilePane",
        availability: Availability::Entry,
    },
    Command {
        id: "location",
        title: "Go to Directory",
        key: "cmd-l",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "find",
        title: "Find Files",
        key: "alt-f7",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "new-file",
        title: "Open / Create File",
        key: "shift-f4",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "save",
        title: "Save File",
        key: "cmd-s",
        context: "DotDir",
        availability: Availability::Save,
    },
    Command {
        id: "escape",
        title: "Cancel / Close",
        key: "escape",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "imageViewer.navigatePrev",
        title: "Image Viewer: Previous",
        key: "left",
        context: "MediaViewer",
        availability: Availability::PreviousMedia,
    },
    Command {
        id: "imageViewer.navigateNext",
        title: "Image Viewer: Next",
        key: "right",
        context: "MediaViewer",
        availability: Availability::NextMedia,
    },
    Command {
        id: "media-play-pause",
        title: "Media: Play / Pause",
        key: "space",
        context: "MediaViewer",
        availability: Availability::PlayableVideo,
    },
    Command {
        id: "media-refresh",
        title: "Media: Reload",
        key: "cmd-r",
        context: "MediaViewer",
        availability: Availability::Media,
    },
    Command {
        id: "sync",
        title: "Open Current Directory in Opposite Panel",
        key: "cmd-equal",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "brief",
        title: "Toggle Brief / Details View",
        key: "cmd-1",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "view-icons",
        title: "Large Icons View",
        key: "cmd-2",
        context: "DotDir",
        availability: Availability::Directory,
    },
    Command {
        id: "view-details",
        title: "Details View",
        key: "cmd-3",
        context: "DotDir",
        availability: Availability::Directory,
    },
    Command {
        id: "view-columns",
        title: "Finder Columns View",
        key: "cmd-4",
        context: "DotDir",
        availability: Availability::Directory,
    },
    Command {
        id: "view-brief",
        title: "Brief View",
        key: "cmd-shift-1",
        context: "DotDir",
        availability: Availability::Directory,
    },
    Command {
        id: "sort-name",
        title: "Sort by Name",
        key: "ctrl-f3",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "sort-size",
        title: "Sort by Size",
        key: "ctrl-f6",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "sort-date",
        title: "Sort by Modified Date",
        key: "ctrl-f5",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "sort-extension",
        title: "Sort by Extension",
        key: "ctrl-f4",
        context: "FilePane",
        availability: Availability::Directory,
    },
    Command {
        id: "quick-view",
        title: "Preview in Opposite Panel",
        key: "cmd-f3",
        context: "DotDir",
        availability: Availability::File,
    },
    Command {
        id: "quick-edit",
        title: "Edit in Opposite Panel",
        key: "cmd-f4",
        context: "FilePane",
        availability: Availability::File,
    },
    Command {
        id: "panels",
        title: "Toggle File Panels",
        key: "cmd-o",
        context: "DotDir",
        availability: Availability::Always,
    },
    Command {
        id: "command-line",
        title: "Focus Command Line",
        key: "ctrl-l",
        context: "DotDir",
        availability: Availability::Always,
    },
];

#[derive(Deserialize)]
pub struct UserBinding {
    pub key: String,
    pub command: String,
    pub context: Option<String>,
}
pub fn bind(cx: &mut App, path: &std::path::Path) {
    cx.bind_keys(
        COMMANDS
            .iter()
            .map(|c| KeyBinding::new(c.key, RunCommand { id: c.id.into() }, Some(c.context))),
    );
    cx.bind_keys([
        KeyBinding::new(
            "cmd-shift-p",
            RunCommand {
                id: "palette".into(),
            },
            Some("DotDir"),
        ),
        KeyBinding::new("cmd-q", RunCommand { id: "quit".into() }, Some("DotDir")),
        KeyBinding::new(
            "alt-pageup",
            RunCommand {
                id: "parent".into(),
            },
            Some("FilePane"),
        ),
        KeyBinding::new(
            "delete",
            RunCommand { id: "trash".into() },
            Some("FilePane"),
        ),
    ]);
    if let Ok(bytes) = std::fs::read(path) {
        match serde_json::from_slice::<Vec<UserBinding>>(&bytes) {
            Ok(bindings) => {
                let bindings = bindings
                    .into_iter()
                    .filter_map(|b| {
                        let context = gpui_kit::KeyBindingContextPredicate::parse(
                            b.context.as_deref().unwrap_or("FilePane"),
                        )
                        .ok()?;
                        KeyBinding::load(
                            &b.key,
                            Box::new(RunCommand { id: b.command }),
                            Some(std::rc::Rc::new(context)),
                            false,
                            None,
                            cx,
                        )
                        .ok()
                    })
                    .collect::<Vec<_>>();
                cx.bind_keys(bindings);
            }
            Err(e) => eprintln!("Cannot read keybindings: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, CommandState, MediaState, RunCommand, function_key_command};
    use gpui_kit::{KeyBinding, KeyContext, Keymap, Keystroke, Modifiers};

    fn available(id: &str, state: &CommandState) -> bool {
        COMMANDS
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .available(state)
    }

    #[test]
    fn keep_preview_requires_a_preview_target_in_visible_panels() {
        let mut state = CommandState::default();
        assert!(!available("keep-preview", &state));
        state.panels_visible = true;
        assert!(!available("keep-preview", &state));
        state.preview = true;
        assert!(available("keep-preview", &state));
        state.panels_visible = false;
        assert!(!available("keep-preview", &state));
    }

    #[test]
    fn tab_switches_out_of_previews_without_overriding_normal_editor_tab() {
        let mut bindings = vec![KeyBinding::new(
            "tab",
            RunCommand {
                id: "editor-tab".into(),
            },
            Some("Input"),
        )];
        bindings.extend(COMMANDS.iter().map(|command| {
            KeyBinding::new(
                command.key,
                RunCommand {
                    id: command.id.into(),
                },
                Some(command.context),
            )
        }));
        let keymap = Keymap::new(bindings);
        for (contexts, expected) in [
            (vec!["DotDir", "FilePane"], "switch"),
            (vec!["DotDir", "PreviewTab", "Input"], "switch"),
            (vec!["DotDir", "PreviewTab", "MediaViewer"], "switch"),
            (
                vec!["DotDir", "PreviewTab", "MediaViewer", "NativeView"],
                "switch",
            ),
            (vec!["DotDir", "Input"], "editor-tab"),
        ] {
            let contexts = contexts
                .into_iter()
                .map(|context| KeyContext::parse(context).unwrap())
                .collect::<Vec<_>>();
            let (bindings, pending) =
                keymap.bindings_for_input(&[Keystroke::parse("tab").unwrap()], &contexts);
            assert!(!pending);
            assert_eq!(
                bindings[0]
                    .action()
                    .as_any()
                    .downcast_ref::<RunCommand>()
                    .unwrap()
                    .id,
                expected
            );
        }
    }

    #[test]
    fn function_bar_resolves_held_modifiers_and_release_from_the_keymap() {
        let keymap = Keymap::new(
            COMMANDS
                .iter()
                .map(|c| KeyBinding::new(c.key, RunCommand { id: c.id.into() }, Some(c.context)))
                .collect(),
        );
        let contexts = [
            KeyContext::parse("DotDir").unwrap(),
            KeyContext::parse("FilePane").unwrap(),
        ];
        for (key, expected) in [
            ("f7", Some("mkdir")),
            ("alt-f7", Some("find")),
            ("ctrl-f3", Some("sort-name")),
            ("ctrl-f4", Some("sort-extension")),
            ("ctrl-f5", Some("sort-date")),
            ("ctrl-f6", Some("sort-size")),
            ("cmd-f3", Some("quick-view")),
            ("cmd-f4", Some("quick-edit")),
            ("shift-f4", Some("new-file")),
            ("alt-shift-f7", None),
        ] {
            let stroke = Keystroke::parse(key).unwrap();
            let number = stroke.key.trim_start_matches('f').parse().unwrap();
            assert_eq!(
                function_key_command(&keymap, &contexts, number, stroke.modifiers).as_deref(),
                expected,
                "{key}"
            );
        }
        assert_eq!(
            function_key_command(&keymap, &contexts, 7, Modifiers::default()).as_deref(),
            Some("mkdir")
        );
        let input = [
            KeyContext::parse("DotDir").unwrap(),
            KeyContext::parse("Input").unwrap(),
        ];
        assert_eq!(
            function_key_command(&keymap, &input, 7, Modifiers::default()),
            None
        );
        assert_eq!(
            function_key_command(&keymap, &input, 7, Modifiers::alt()).as_deref(),
            Some("find")
        );
    }

    #[test]
    fn function_bar_uses_contextual_user_overrides_without_falling_back() {
        let mut keymap = Keymap::new(
            COMMANDS
                .iter()
                .map(|c| KeyBinding::new(c.key, RunCommand { id: c.id.into() }, Some(c.context)))
                .collect(),
        );
        keymap.add_bindings([
            KeyBinding::new("alt-f7", RunCommand { id: "copy".into() }, Some("FilePane")),
            KeyBinding::new(
                "cmd-alt-f7",
                RunCommand {
                    id: "selection.inspect".into(),
                },
                Some("DotDir"),
            ),
        ]);
        let contexts = [
            KeyContext::parse("DotDir").unwrap(),
            KeyContext::parse("FilePane").unwrap(),
        ];
        let command = function_key_command(&keymap, &contexts, 7, Modifiers::alt()).unwrap();
        assert_eq!(command, "copy");
        // No selection: show the overridden command disabled, never Find Files.
        assert!(!available(
            &command,
            &CommandState {
                directory: true,
                ..Default::default()
            }
        ));
        assert_eq!(
            function_key_command(
                &keymap,
                &contexts,
                7,
                Keystroke::parse("cmd-alt-f7").unwrap().modifiers
            )
            .as_deref(),
            Some("selection.inspect")
        );
        let input = [
            KeyContext::parse("DotDir").unwrap(),
            KeyContext::parse("Input").unwrap(),
        ];
        assert_eq!(
            function_key_command(&keymap, &input, 7, Modifiers::alt()).as_deref(),
            Some("find")
        );
    }

    #[test]
    fn media_commands_follow_active_media_capabilities() {
        let directory = CommandState {
            panels_visible: true,
            directory: true,
            ..Default::default()
        };
        let image = CommandState {
            media: Some(MediaState {
                next: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let video = CommandState {
            media: Some(MediaState {
                playable_video: true,
                previous: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        for state in [CommandState::default(), directory, image] {
            assert!(!available("media-play-pause", &state));
        }
        assert!(!available("media-refresh", &directory));
        assert!(available("media-refresh", &image));
        assert!(available("imageViewer.navigateNext", &image));
        assert!(!available("imageViewer.navigatePrev", &image));
        assert!(available("media-play-pause", &video));
        assert!(available("imageViewer.navigatePrev", &video));
        assert!(!available("imageViewer.navigateNext", &video));
        // A video still loading, failing decoding, or replaced by an image
        // loses playback capability; a custom keybinding cannot retain it.
        let unavailable_video = CommandState {
            media: Some(MediaState::default()),
            ..video
        };
        assert!(!available("media-play-pause", &unavailable_video));
    }

    #[test]
    fn file_commands_require_their_target_and_recheck_busy_operations() {
        let mut state = CommandState {
            panels_visible: true,
            directory: true,
            entry: true, // the parent entry
            operation_idle: true,
            ..Default::default()
        };
        assert!(available("open", &state));
        assert!(available("mkdir", &state));
        for id in ["rename", "view", "edit", "copy", "move", "trash", "parent"] {
            assert!(!available(id, &state), "{id}");
        }
        state.real_entry = true; // a directory can be renamed/transferred, not edited
        state.selection = true;
        assert!(available("rename", &state));
        assert!(available("copy", &state));
        assert!(!available("edit", &state));
        state.file = true;
        for id in ["view", "edit", "quick-view", "quick-edit"] {
            assert!(available(id, &state), "{id}");
        }
        state.operation_idle = false;
        for id in ["copy", "move", "trash", "rename"] {
            assert!(!available(id, &state), "{id}");
        }
        state.operation_idle = true;
        state.real_entry = false; // parent focused but actual files remain marked
        state.file = false;
        assert!(available("copy", &state));
        assert!(!available("rename", &state));
    }

    #[test]
    fn editor_empty_directory_and_terminal_have_distinct_command_sets() {
        let mut editor = CommandState {
            has_tab: true,
            ..Default::default()
        };
        assert!(!available("save", &editor)); // read-only or saving
        editor.can_save = true;
        assert!(available("save", &editor));
        assert!(available("rename", &editor)); // F2 saves an editable document
        for id in [
            "edit",
            "copy",
            "view-columns",
            "sort-name",
            "media-play-pause",
        ] {
            assert!(!available(id, &editor), "{id}");
        }
        let empty_directory = CommandState {
            directory: true,
            ..Default::default()
        };
        assert!(available("mkdir", &empty_directory));
        assert!(available("refresh", &empty_directory));
        assert!(!available("open", &empty_directory));
        assert!(!available("select-all", &empty_directory));
        assert!(!available("next-tab", &editor));
        editor.multiple_tabs = true;
        assert!(available("next-tab", &editor));
        let terminal_only = CommandState::default();
        for id in ["palette", "settings", "terminal", "panels", "quit"] {
            assert!(available(id, &terminal_only), "{id}");
        }
        for id in ["copy", "switch", "close-tab", "media-refresh", "save"] {
            assert!(!available(id, &terminal_only), "{id}");
        }
    }

    #[test]
    fn view_shortcut_reaches_active_pane_when_command_line_has_focus() {
        let keymap = Keymap::new(
            COMMANDS
                .iter()
                .map(|c| KeyBinding::new(c.key, RunCommand { id: c.id.into() }, Some(c.context)))
                .collect(),
        );
        for key in ["f3", "cmd-f3"] {
            for context in ["FilePane", "Input"] {
                let (bindings, pending) = keymap.bindings_for_input(
                    &[Keystroke::parse(key).unwrap()],
                    &[
                        KeyContext::parse("DotDir").unwrap(),
                        KeyContext::parse(context).unwrap(),
                    ],
                );
                assert!(!pending);
                let expected = if key == "f3" { "view" } else { "quick-view" };
                assert!(available(
                    expected,
                    &CommandState {
                        directory: true,
                        file: true,
                        ..Default::default()
                    }
                ));
                assert!(
                    bindings.iter().any(|binding| {
                        binding
                            .action()
                            .as_any()
                            .downcast_ref::<RunCommand>()
                            .is_some_and(|action| action.id == expected)
                    }),
                    "{key} should open the active pane's selected file with {context} focused"
                );
            }
        }
    }
}

use crate::{theme::Palette, vscode_nls::Locale};
use anyhow::{Context, Result, bail, ensure};
use gpui_kit::{
    App, Hsla,
    component::{Theme, ThemeMode, highlighter::HighlightTheme},
    rgba,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ThemeSource {
    pub path: PathBuf,
    root: PathBuf,
    pub label: Option<String>,
    mode: Option<String>,
}

impl ThemeSource {
    pub fn discover(path: &Path, locale: Option<&str>) -> Result<Vec<Self>> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Cannot open {}", path.display()))?;
        if path.is_file() && path.file_name().is_some_and(|name| name != "package.json") {
            let root = path
                .parent()
                .context("Theme file has no parent directory")?
                .into();
            return Ok(vec![Self {
                path,
                root,
                label: None,
                mode: None,
            }]);
        }
        let manifest = if path.is_file() {
            path
        } else if path.join("package.json").is_file() {
            path.join("package.json")
        } else {
            path.join("extension/package.json")
        };
        let root = manifest
            .parent()
            .context("Extension manifest has no parent directory")?;
        let value: serde_json::Value = read_json(&manifest)?;
        let themes = value["contributes"]["themes"]
            .as_array()
            .filter(|themes| !themes.is_empty())
            .context("This extension does not contribute any color themes")?;
        let locale = Locale::detect(locale);
        let messages = locale.manifest(root);
        let mut sources = Vec::new();
        for theme in themes {
            let Some(path) = theme["path"].as_str().filter(|path| !path.is_empty()) else {
                continue;
            };
            sources.push(Self {
                path: root.join(path),
                root: root.into(),
                label: theme["label"]
                    .as_str()
                    .map(|label| messages.translate(label).into()),
                mode: theme["uiTheme"].as_str().map(str::to_owned),
            });
        }
        ensure!(
            !sources.is_empty(),
            "This extension has no usable color theme paths"
        );
        Ok(sources)
    }
}

pub(crate) struct ImportedTheme {
    pub source: ThemeSource,
    pub name: String,
    pub palette: Palette,
    editor: Arc<HighlightTheme>,
    colors: ThemeColors,
}

type ThemeColors = BTreeMap<String, Option<String>>;

impl ImportedTheme {
    pub fn load(mut source: ThemeSource) -> Result<Self> {
        source.root = source.root.canonicalize()?;
        source.path = theme_path(&source.root, &source.path)?;
        let mut theme = ThemeData::default();
        theme.read(&source.root, &source.path, &mut Vec::new())?;
        let name = source
            .label
            .clone()
            .or(theme.name.take())
            .unwrap_or_else(|| {
                source
                    .path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            });
        let editor_background = color(&theme.colors, &["editor.background"]).or_else(|| {
            theme
                .default_settings()
                .and_then(|rule| rule.background.as_deref())
                .and_then(parse_color)
        });
        let editor_foreground = color(&theme.colors, &["editor.foreground"]).or_else(|| {
            theme
                .default_settings()
                .and_then(|rule| rule.foreground.as_deref())
                .and_then(parse_color)
        });
        let mode = match source.mode.as_deref().or(theme.mode.as_deref()) {
            Some("vs" | "hc-light" | "light" | "hcLight") => ThemeMode::Light,
            Some(_) => ThemeMode::Dark,
            None if editor_background.is_some_and(|background| background.l > 0.6) => {
                ThemeMode::Light
            }
            None => ThemeMode::Dark,
        };
        let mut palette = Palette::named(if mode.is_dark() { "ayu" } else { "light" });
        let imported = |keys: &[&str], fallback| color(&theme.colors, keys).unwrap_or(fallback);
        palette.bg = imported(
            &["sideBar.background"],
            editor_background.unwrap_or(palette.bg),
        );
        palette.fg = imported(
            &["sideBar.foreground", "foreground", "editor.foreground"],
            editor_foreground.unwrap_or(palette.fg),
        );
        palette.secondary = imported(
            &[
                "titleBar.activeBackground",
                "editorGroupHeader.tabsBackground",
            ],
            palette.bg,
        );
        palette.muted = imported(
            &["descriptionForeground", "editorLineNumber.foreground"],
            palette.muted,
        );
        palette.border = imported(
            &[
                "panel.border",
                "sideBar.border",
                "editorGroup.border",
                "contrastBorder",
            ],
            palette.border,
        );
        palette.accent = imported(
            &["button.background", "focusBorder", "textLink.foreground"],
            palette.accent,
        );
        palette.accent_fg = imported(&["button.foreground"], palette.accent_fg);
        palette.selected = imported(
            &[
                "list.activeSelectionBackground",
                "list.hoverBackground",
                "editor.selectionBackground",
            ],
            palette.selected,
        );
        palette.selected_fg = Some(imported(&["list.activeSelectionForeground"], palette.fg));
        palette.marked = imported(&["list.inactiveSelectionBackground"], palette.marked);
        palette.input = imported(
            &[
                "input.background",
                "quickInput.background",
                "editor.background",
            ],
            palette.input,
        );
        palette.command_bg = palette.input;
        palette.folder = imported(
            &["symbolIcon.folderForeground", "icon.foreground"],
            palette.muted,
        );
        palette.error = imported(
            &["errorForeground", "editorError.foreground"],
            palette.error,
        );
        let mut editor = if mode.is_dark() {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        }
        .as_ref()
        .clone();
        editor.name = name.clone();
        editor.style.editor_background = editor_background;
        editor.style.editor_foreground = editor_foreground;
        editor.style.editor_active_line = color(&theme.colors, &["editor.lineHighlightBackground"]);
        editor.style.editor_line_number = color(&theme.colors, &["editorLineNumber.foreground"]);
        editor.style.editor_active_line_number =
            color(&theme.colors, &["editorLineNumber.activeForeground"]);
        editor.style.editor_gutter_background = color(&theme.colors, &["editorGutter.background"]);
        editor.style.editor_invisible = color(&theme.colors, &["editorWhitespace.foreground"]);
        theme.apply_token_colors(&mut editor)?;
        Ok(Self {
            source,
            name,
            palette,
            editor: Arc::new(editor),
            colors: theme.colors,
        })
    }

    pub fn apply(&self, cx: &mut App) {
        let palette = self.palette;
        palette.apply(cx);
        Theme::update(cx, |theme| {
            theme.highlight_theme = self.editor.clone();
            let colors = &mut theme.colors;
            colors.foreground = self.editor.style.editor_foreground.unwrap_or(palette.fg);
            colors.caret =
                color(&self.colors, &["editorCursor.foreground"]).unwrap_or(colors.foreground);
            colors.selection =
                color(&self.colors, &["editor.selectionBackground"]).unwrap_or(palette.selected);
            colors.ring = color(&self.colors, &["focusBorder"]).unwrap_or(palette.accent);
            colors.popover = color(
                &self.colors,
                &["editorHoverWidget.background", "editorWidget.background"],
            )
            .unwrap_or(palette.input);
            colors.popover_foreground = color(
                &self.colors,
                &["editorHoverWidget.foreground", "editorWidget.foreground"],
            )
            .unwrap_or(colors.foreground);
            colors.scrollbar_thumb = color(&self.colors, &["scrollbarSlider.background"])
                .unwrap_or(colors.scrollbar_thumb);
            colors.scrollbar_thumb_hover =
                color(&self.colors, &["scrollbarSlider.hoverBackground"])
                    .unwrap_or(colors.scrollbar_thumb_hover);
        });
    }
}

fn theme_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("Cannot open {}", path.display()))?;
    ensure!(
        path.starts_with(root) && path.is_file(),
        "Theme resource must be a file within {}",
        root.display()
    );
    Ok(path)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("Cannot read {}", path.display()))?;
    jsonc_parser::parse_to_serde_value(text.trim_start_matches('\u{feff}'), &Default::default())
        .with_context(|| format!("Invalid JSON theme data in {}", path.display()))
}

fn color(colors: &ThemeColors, names: &[&str]) -> Option<Hsla> {
    names
        .iter()
        .find_map(|name| parse_color(colors.get(*name)?.as_deref()?))
}

fn parse_color(text: &str) -> Option<Hsla> {
    let hex = text.strip_prefix('#')?;
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let value = match hex.len() {
        3 | 4 => {
            let expanded = (0..hex.len()).rev().fold(0, |result, i| {
                (result << 8) | ((value >> (i * 4)) & 15) * 17
            });
            if hex.len() == 3 {
                (expanded << 8) | 255
            } else {
                expanded
            }
        }
        6 => (value << 8) | 255,
        8 => value,
        _ => return None,
    };
    Some(rgba(value).into())
}

#[derive(Default)]
struct ThemeData {
    name: Option<String>,
    mode: Option<String>,
    colors: ThemeColors,
    tokens: Vec<TokenRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ColorThemeFile {
    name: Option<String>,
    #[serde(rename = "type")]
    mode: Option<String>,
    include: Option<PathBuf>,
    #[serde(default)]
    colors: ThemeColors,
    token_colors: Option<TokenColors>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenColors {
    Rules(Vec<TokenRule>),
    File(PathBuf),
}

#[derive(Deserialize)]
struct TextMateTheme {
    name: Option<String>,
    settings: Vec<TokenRule>,
}

#[derive(Deserialize)]
struct TokenRule {
    #[serde(default, deserialize_with = "read_scopes")]
    scope: Vec<String>,
    settings: TokenStyle,
}

fn read_scopes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scopes {
        Single(String),
        Multiple(Vec<String>),
    }
    Ok(match Option::<Scopes>::deserialize(deserializer)? {
        Some(Scopes::Single(scope)) => vec![scope],
        Some(Scopes::Multiple(scopes)) => scopes,
        None => vec![],
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenStyle {
    foreground: Option<String>,
    background: Option<String>,
    font_style: Option<String>,
}

impl ThemeData {
    fn read(&mut self, root: &Path, path: &Path, includes: &mut Vec<PathBuf>) -> Result<()> {
        let path = theme_path(root, path)?;
        ensure!(
            includes.len() < 32 && !includes.contains(&path),
            "Circular or too many theme includes at {}",
            path.display()
        );
        includes.push(path.clone());
        let result = self.read_file(root, &path, includes);
        includes.pop();
        result
    }

    fn read_file(&mut self, root: &Path, path: &Path, includes: &mut Vec<PathBuf>) -> Result<()> {
        let directory = path
            .parent()
            .context("Theme file has no parent directory")?;
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("tmTheme") {
            let theme: TextMateTheme = plist::from_file(path)
                .with_context(|| format!("Invalid TextMate theme {}", path.display()))?;
            self.name = theme.name.or(self.name.take());
            self.tokens.extend(theme.settings);
            return Ok(());
        }
        if !extension.eq_ignore_ascii_case("json") && !extension.eq_ignore_ascii_case("jsonc") {
            bail!("Expected a JSON, JSONC, or TextMate theme file");
        }
        let file: ColorThemeFile = read_json(path)?;
        if let Some(include) = file.include {
            self.read(root, &directory.join(include), includes)?;
        }
        self.name = file.name.or(self.name.take());
        self.mode = file.mode.or(self.mode.take());
        self.colors.extend(file.colors);
        match file.token_colors {
            Some(TokenColors::Rules(rules)) => self.tokens.extend(rules),
            Some(TokenColors::File(file)) => {
                let path = theme_path(root, &directory.join(file))?;
                let theme: TextMateTheme = plist::from_file(&path)
                    .with_context(|| format!("Invalid TextMate theme {}", path.display()))?;
                self.tokens.extend(theme.settings);
            }
            None => {}
        }
        Ok(())
    }

    fn default_settings(&self) -> Option<&TokenStyle> {
        self.tokens
            .iter()
            .rev()
            .find(|rule| rule.scope.is_empty())
            .map(|rule| &rule.settings)
    }

    fn token_property<'a>(
        &'a self,
        targets: &[&str],
        property: impl Fn(&'a TokenStyle) -> Option<&'a str>,
    ) -> Option<&'a str> {
        targets.iter().find_map(|target| {
            self.tokens
                .iter()
                .filter_map(|rule| {
                    let value = property(&rule.settings)?;
                    let specificity = rule
                        .scope
                        .iter()
                        .flat_map(|scope| scope.split(','))
                        .map(str::trim)
                        .filter(|scope| {
                            *scope == *target
                                || target
                                    .strip_prefix(scope)
                                    .is_some_and(|rest| rest.starts_with('.'))
                        })
                        .map(str::len)
                        .max()?;
                    Some((specificity, value))
                })
                .max_by_key(|&(specificity, _)| specificity)
                .map(|(_, value)| value)
        })
    }

    fn apply_token_colors(&self, editor: &mut HighlightTheme) -> Result<()> {
        let mut syntax = serde_json::to_value(&editor.style.syntax)?;
        for &(capture, scopes) in CAPTURE_SCOPES {
            let foreground = self
                .token_property(scopes, |rule| rule.foreground.as_deref())
                .and_then(parse_color);
            let font = self.token_property(scopes, |rule| rule.font_style.as_deref());
            let style = &mut syntax[capture];
            if let Some(foreground) = foreground {
                style["color"] = json!(foreground);
            }
            if let Some(font) = font {
                let mut styles = font.split_whitespace();
                style["font_style"] = json!(if styles.any(|style| style == "italic") {
                    "italic"
                } else {
                    "normal"
                });
                style["font_weight"] =
                    json!(if font.split_whitespace().any(|style| style == "bold") {
                        700
                    } else {
                        400
                    });
            }
        }
        editor.style.syntax = serde_json::from_value(syntax)?;
        Ok(())
    }
}

// Tree-sitter captures are broader than TextMate scopes. Use representative
// scopes for each capture and prefer a more specific matching rule.
const CAPTURE_SCOPES: &[(&str, &[&str])] = &[
    (
        "attribute",
        &["entity.other.attribute-name", "meta.annotation"],
    ),
    (
        "boolean",
        &["constant.language.boolean", "constant.language"],
    ),
    ("comment", &["comment.line.double-slash", "comment.block"]),
    (
        "comment_doc",
        &["comment.block.documentation", "comment.line.documentation"],
    ),
    ("constant", &["constant.other", "constant.language"]),
    (
        "constructor",
        &["entity.name.function.constructor", "support.class"],
    ),
    ("embedded", &["meta.embedded"]),
    ("emphasis", &["markup.italic"]),
    ("emphasis.strong", &["markup.bold"]),
    ("enum", &["entity.name.type.enum", "support.type"]),
    ("function", &["entity.name.function", "support.function"]),
    ("label", &["entity.name.label"]),
    (
        "link_text",
        &["string.other.link.title", "markup.underline.link"],
    ),
    ("link_uri", &["markup.underline.link"]),
    ("number", &["constant.numeric"]),
    (
        "keyword",
        &["keyword.control", "storage.type", "storage.modifier"],
    ),
    ("operator", &["keyword.operator"]),
    ("preproc", &["meta.preprocessor", "keyword.control.import"]),
    (
        "property",
        &["variable.other.property", "support.type.property-name"],
    ),
    ("punctuation", &["punctuation"]),
    (
        "punctuation.bracket",
        &["punctuation.section.brackets", "punctuation.section"],
    ),
    (
        "punctuation.delimiter",
        &["punctuation.separator", "punctuation.terminator"],
    ),
    ("punctuation.list_marker", &["punctuation.definition.list"]),
    (
        "punctuation.special",
        &["punctuation.definition.template-expression"],
    ),
    ("string", &["string.quoted.double", "string.quoted.single"]),
    ("string.escape", &["constant.character.escape"]),
    ("string.regex", &["string.regexp"]),
    (
        "string.special",
        &["string.interpolated", "string.template"],
    ),
    ("string.special.symbol", &["constant.other.symbol"]),
    ("tag", &["entity.name.tag"]),
    ("tag.doctype", &["meta.tag.sgml.doctype"]),
    ("text.code.span", &["markup.inline.raw"]),
    ("text.literal", &["markup.raw"]),
    ("title", &["markup.heading"]),
    (
        "type",
        &["entity.name.type", "support.type", "support.class"],
    ),
    (
        "variable",
        &["variable.other.readwrite", "variable.parameter"],
    ),
    ("variable.special", &["variable.language"]),
    (
        "variant",
        &["variable.other.enummember", "constant.other.enum"],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_localized_theme_variants_and_inherited_token_colors() {
        let extension = tempfile::tempdir().unwrap();
        let root = extension.path();
        fs::write(
            root.join("package.json"),
            r#"{
            "contributes":{"themes":[
                {"label":"%theme%","uiTheme":"vs","path":"themes/child.jsonc"},
                {"label":"Dark","uiTheme":"vs-dark","path":"themes/base.json"}
            ]}
        }"#,
        )
        .unwrap();
        fs::write(
            root.join("package.nls.json"),
            r#"{"theme":"Light variant"}"#,
        )
        .unwrap();
        fs::write(
            root.join("package.nls.fr.json"),
            r#"{"theme":"Thème clair"}"#,
        )
        .unwrap();
        fs::create_dir(root.join("themes")).unwrap();
        fs::write(root.join("themes/base.json"), r##"{
            "colors":{"editor.background":"#123","editor.foreground":"#FFF","sideBar.background":"#202020"},
            "tokenColors":[{"scope":"comment, string","settings":{"foreground":"#FF0000","fontStyle":"italic"}}]
        }"##).unwrap();
        fs::write(root.join("themes/child.jsonc"), r##"{
            // Use the base palette with a different editor background.
            "include":"./base.json",
            "colors":{"editor.background":"#F0E0D0",},
            "tokenColors":[{"scope":["string"],"settings":{"foreground":"#008000","fontStyle":"bold"}}],
        }"##).unwrap();

        let mut sources = ThemeSource::discover(root, Some("fr-CA"))
            .unwrap()
            .into_iter();
        let theme = ImportedTheme::load(sources.next().unwrap()).unwrap();
        assert_eq!(theme.name, "Thème clair");
        assert!(!theme.palette.mode.is_dark());
        assert_eq!(theme.palette.bg, Hsla::from(rgba(0x202020ff)));
        assert_eq!(
            theme.editor.style.editor_background,
            Some(rgba(0xf0e0d0ff).into())
        );
        let syntax = &theme.editor.style.syntax;
        let string = syntax.style("string").unwrap();
        assert_eq!(string.color, Some(rgba(0x008000ff).into()));
        assert_eq!(string.font_weight, Some(gpui_kit::FontWeight::BOLD));
        assert_eq!(
            syntax.style("comment").unwrap().font_style,
            Some(gpui_kit::FontStyle::Italic)
        );
        assert!(
            ImportedTheme::load(sources.next().unwrap())
                .unwrap()
                .palette
                .mode
                .is_dark()
        );
    }

    #[test]
    fn textmate_theme_supplies_editor_defaults_and_token_colors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.tmTheme");
        plist::to_file_xml(
            &path,
            &json!({
                "name": "Legacy theme",
                "settings": [
                    {"settings": {"background": "#fff", "foreground": "#000"}},
                    {"scope": "constant.numeric", "settings": {"foreground": "#639"}}
                ]
            }),
        )
        .unwrap();
        let theme = ImportedTheme::load(ThemeSource::discover(&path, None).unwrap().pop().unwrap())
            .unwrap();
        assert_eq!(theme.name, "Legacy theme");
        assert!(!theme.palette.mode.is_dark());
        assert_eq!(theme.palette.bg, Hsla::from(rgba(0xffffffff)));
        assert_eq!(
            theme.editor.style.syntax.style("number").unwrap().color,
            Some(rgba(0x663399ff).into())
        );
    }
}

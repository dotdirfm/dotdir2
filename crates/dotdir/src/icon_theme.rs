//! Declarative VS Code file-icon themes. No extension code is executed.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Associations {
    file: Option<String>,
    folder: Option<String>,
    file_names: HashMap<String, String>,
    file_extensions: HashMap<String, String>,
    folder_names: HashMap<String, String>,
    language_ids: HashMap<String, String>,
}
impl Associations {
    fn normalized(mut self) -> Self {
        for map in [
            &mut self.file_names,
            &mut self.file_extensions,
            &mut self.folder_names,
        ] {
            *map = std::mem::take(map)
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect();
        }
        self
    }
    fn overlay(&mut self, other: Self) {
        if other.file.is_some() {
            self.file = other.file;
        }
        if other.folder.is_some() {
            self.folder = other.folder;
        }
        self.file_names.extend(other.file_names);
        self.file_extensions.extend(other.file_extensions);
        self.folder_names.extend(other.folder_names);
        self.language_ids.extend(other.language_ids);
    }
    fn icon(&self, name: &str, directory: bool) -> Option<&str> {
        if directory {
            return self
                .folder_names
                .get(name)
                .map(String::as_str)
                .or(self.folder.as_deref());
        }
        self.file_names
            .get(name)
            .or_else(|| {
                name.match_indices('.')
                    .find_map(|(i, _)| self.file_extensions.get(&name[i + 1..]))
            })
            .or_else(|| language_id(name).and_then(|id| self.language_ids.get(id)))
            .map(String::as_str)
            .or(self.file.as_deref())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    icon_path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeFile {
    icon_definitions: HashMap<String, Definition>,
    #[serde(flatten)]
    associations: Associations,
    #[serde(default)]
    light: Associations,
}

#[derive(Deserialize)]
struct Package {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    version: Option<String>,
    contributes: Contributions,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contributions {
    icon_themes: Vec<Contribution>,
}
#[derive(Deserialize)]
struct Contribution {
    id: String,
    label: Option<String>,
    path: PathBuf,
}

pub struct IconTheme {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    dark: Associations,
    light: Associations,
    icons: HashMap<String, PathBuf>,
}
impl IconTheme {
    pub fn load(extension: &Path) -> Result<Self> {
        let root = extension
            .canonicalize()
            .context("Cannot open icon extension directory")?;
        let package: Package =
            serde_json::from_slice(&std::fs::read(root.join("package.json"))?)
                .context("The directory must contain a VS Code icon-theme package.json")?;
        let contribution = package
            .contributes
            .icon_themes
            .into_iter()
            .next()
            .context("This extension does not contribute a file icon theme")?;
        let path = confined_file(&root, &root, &contribution.path)?;
        let theme: ThemeFile = serde_json::from_slice(&std::fs::read(&path)?)
            .context("Cannot read icon-theme mappings")?;
        let mut icons = HashMap::new();
        for (id, definition) in theme.icon_definitions {
            if let Some(icon_path) = definition.icon_path {
                icons.insert(
                    id,
                    confined_file(&root, path.parent().unwrap(), &icon_path)?,
                );
            }
        }
        if icons.is_empty() {
            bail!("This theme has no image icons (font icon themes are not supported)");
        }
        let dark = theme.associations.normalized();
        let mut light = dark.clone();
        light.overlay(theme.light.normalized());
        Ok(Self {
            root,
            name: contribution
                .label
                .or(package.display_name)
                .unwrap_or(contribution.id),
            version: package.version.unwrap_or_default(),
            dark,
            light,
            icons,
        })
    }
    /// Maps were loaded once; lookup does no filesystem work. GPUI caches images.
    pub fn icon(&self, name: &str, directory: bool, light: bool) -> Option<&Path> {
        let associations = if light { &self.light } else { &self.dark };
        let name = name.to_lowercase();
        let id = associations.icon(&name, directory)?;
        self.icons
            .get(id)
            .or_else(|| {
                associations
                    .icon("", directory)
                    .and_then(|id| self.icons.get(id))
            })
            .map(PathBuf::as_path)
    }
}

fn confined_file(root: &Path, base: &Path, path: &Path) -> Result<PathBuf> {
    let resolved = base
        .join(path)
        .canonicalize()
        .with_context(|| format!("Missing theme asset: {}", path.display()))?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        bail!(
            "Theme asset must be a file inside the extension: {}",
            path.display()
        );
    }
    Ok(resolved)
}

// Themes may use languageIds instead of duplicating standard extensions.
fn language_id(name: &str) -> Option<&'static str> {
    Some(match name.rsplit_once('.')?.1 {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "json" => "json",
        "jsonc" => "jsonc",
        "json5" => "json5",
        "py" | "pyw" => "python",
        "rb" => "ruby",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "swift" => "swift",
        "php" => "php",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "ps1" => "powershell",
        "bat" | "cmd" => "bat",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "md" | "markdown" => "markdown",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "ini" | "cfg" => "ini",
        "sql" => "sql",
        "txt" => "plaintext",
        "vue" => "vue",
        "svelte" => "svelte",
        "dart" => "dart",
        "nix" => "nix",
        "svg" => "svg",
        "csv" => "csv",
        "tsv" => "tsv",
        "r" => "r",
        "jl" => "julia",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dist")).unwrap();
        std::fs::create_dir(dir.path().join("icons")).unwrap();
        let mut definitions = HashMap::new();
        for id in [
            "file",
            "folder",
            "rust",
            "typescript",
            "test",
            "named",
            "light",
            "src",
        ] {
            std::fs::write(dir.path().join(format!("icons/{id}.svg")), "<svg/>").unwrap();
            definitions.insert(id, json!({"iconPath":format!("../icons/{id}.svg")}));
        }
        std::fs::write(dir.path().join("package.json"), json!({"displayName":"Test icons", "version":"1", "contributes":{"iconThemes":[{"id":"test", "label":"Test icons", "path":"dist/icons.json"}]}}).to_string()).unwrap();
        std::fs::write(
            dir.path().join("dist/icons.json"),
            json!({
                "iconDefinitions":definitions, "file":"file", "folder":"folder",
                "fileNames":{"special.test.rs":"named"},
                "fileExtensions":{"rs":"rust", "test.rs":"test"},
                "folderNames":{"src":"src"}, "languageIds":{"typescript":"typescript"},
                "light":{"fileExtensions":{"rs":"light"}}
            })
            .to_string(),
        )
        .unwrap();
        dir
    }
    fn icon_name(theme: &IconTheme, name: &str, directory: bool, light: bool) -> String {
        theme
            .icon(name, directory, light)
            .unwrap()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
    #[test]
    fn resolves_specific_names_before_compound_extensions_and_language_ids() {
        let dir = fixture();
        let theme = IconTheme::load(dir.path()).unwrap();
        assert_eq!(icon_name(&theme, "SPECIAL.TEST.RS", false, false), "named");
        assert_eq!(icon_name(&theme, "other.test.rs", false, false), "test");
        assert_eq!(icon_name(&theme, "main.rs", false, false), "rust");
        assert_eq!(icon_name(&theme, "index.ts", false, false), "typescript");
        assert_eq!(icon_name(&theme, "unknown.xyz", false, false), "file");
        assert_eq!(icon_name(&theme, "SRC", true, false), "src");
        assert_eq!(icon_name(&theme, "other", true, false), "folder");
    }
    #[test]
    fn light_overrides_keep_more_specific_base_associations() {
        let dir = fixture();
        let theme = IconTheme::load(dir.path()).unwrap();
        assert_eq!(icon_name(&theme, "main.rs", false, true), "light");
        assert_eq!(icon_name(&theme, "other.test.rs", false, true), "test");
        assert_eq!(icon_name(&theme, "special.test.rs", false, true), "named");
        assert_eq!(icon_name(&theme, "src", true, true), "src");
    }
    #[test]
    fn rejects_assets_outside_the_package() {
        let dir = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            dir.path().join("dist/icons.json"),
            json!({"iconDefinitions":{"file":{"iconPath":outside.path()}},"file":"file"})
                .to_string(),
        )
        .unwrap();
        assert!(
            IconTheme::load(dir.path())
                .err()
                .unwrap()
                .to_string()
                .contains("inside the extension")
        );
    }
}

/// Color SVG icons must be rasterized at their displayed size, rather than
/// enlarging the 16px intrinsic image. GPUI caches this by path and device size.
#[derive(Clone)]
pub enum SizedThemeSvg {}
impl gpui_kit::Asset for SizedThemeSvg {
    type Source = (PathBuf, i32);
    type Output =
        std::result::Result<std::sync::Arc<gpui_kit::RenderImage>, gpui_kit::ImageCacheError>;
    fn load(
        source: Self::Source,
        cx: &mut gpui_kit::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let bytes = std::fs::read(source.0)?;
            let svg = renderer.parse_svg(&bytes)?;
            renderer
                .render_parsed(
                    &svg,
                    gpui_kit::SvgSize::Size(gpui_kit::size(
                        gpui_kit::DevicePixels(source.1),
                        gpui_kit::DevicePixels(source.1),
                    )),
                )
                .map_err(Into::into)
        }
    }
}
pub fn sized_icon(path: &Path, logical_size: f32) -> gpui_kit::ImageSource {
    let path = path.to_path_buf();
    gpui_kit::ImageSource::Custom(std::sync::Arc::new(move |window, cx| {
        let pixels = (logical_size * window.scale_factor())
            .ceil()
            .clamp(1., 512.) as i32;
        window.use_asset::<SizedThemeSvg>(&(path.clone(), pixels), cx)
    }))
}

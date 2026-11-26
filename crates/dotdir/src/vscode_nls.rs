use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

type Messages = BTreeMap<String, String>;

pub(crate) struct Locale(String);

impl Locale {
    pub fn detect(preferred: Option<&str>) -> Self {
        Self(
            preferred
                .and_then(normalize)
                .or_else(|| sys_locale::get_locale().as_deref().and_then(normalize))
                .unwrap_or_else(|| "en".into()),
        )
    }

    fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::successors(Some(self.0.as_str()), |locale| {
            locale.rsplit_once('-').map(|(parent, _)| parent)
        })
    }

    pub fn manifest(&self, root: &Path) -> ManifestMessages {
        let original = read_bundle(root, &root.join("package.nls.json"))
            .map(|bundle| bundle.messages)
            .unwrap_or_default();
        let mut translated = Messages::new();
        for candidate in self.candidates().collect::<Vec<_>>().into_iter().rev() {
            if let Some(bundle) =
                read_bundle(root, &root.join(format!("package.nls.{candidate}.json")))
            {
                translated.extend(bundle.messages);
            }
        }
        ManifestMessages {
            original,
            translated,
        }
    }

    pub fn extension(
        &self,
        root: &Path,
        l10n: Option<&Path>,
        package_json: Value,
    ) -> ExtensionLocalization {
        let bundle = l10n.and_then(|directory| {
            let directory = root.join(directory);
            self.candidates().find_map(|candidate| {
                read_bundle(
                    root,
                    &directory.join(format!("bundle.l10n.{candidate}.json")),
                )
            })
        });
        ExtensionLocalization {
            locale: self.0.clone(),
            package_json,
            bundle,
        }
    }
}

fn normalize(locale: &str) -> Option<String> {
    let locale = locale
        .trim()
        .split(['.', '@'])
        .next()?
        .replace('_', "-")
        .to_ascii_lowercase();
    let mut parts = locale.split('-');
    let language = parts.next()?;
    ((2..=8).contains(&language.len())
        && language.bytes().all(|b| b.is_ascii_alphabetic())
        && language != "posix"
        && parts.all(|part| {
            (1..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
        }))
    .then_some(locale)
}

pub(crate) struct ManifestMessages {
    original: Messages,
    translated: Messages,
}

impl ManifestMessages {
    pub fn original<'a>(&'a self, text: &'a str) -> &'a str {
        message_key(text)
            .and_then(|key| self.original.get(key))
            .map_or(text, String::as_str)
    }

    pub fn translate<'a>(&'a self, text: &'a str) -> &'a str {
        message_key(text)
            .and_then(|key| self.translated.get(key))
            .map_or_else(|| self.original(text), String::as_str)
    }

    pub fn localize(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                let translated = self.translate(text);
                if translated != text {
                    *text = translated.into();
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|value| self.localize(value)),
            Value::Object(fields) => fields.values_mut().for_each(|value| self.localize(value)),
            _ => {}
        }
    }
}

fn message_key(text: &str) -> Option<&str> {
    let key = text.strip_prefix('%')?.strip_suffix('%')?;
    (!key.is_empty() && !key.contains('%')).then_some(key)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtensionLocalization {
    locale: String,
    package_json: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle: Option<Bundle>,
}

#[derive(Clone, Serialize)]
struct Bundle {
    path: PathBuf,
    messages: Messages,
}

fn read_bundle(root: &Path, path: &Path) -> Option<Bundle> {
    let path = path.canonicalize().ok()?;
    if !path.starts_with(root) || !path.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&path).ok()?;
    let fields: Map<String, Value> =
        serde_json::from_str(contents.trim_start_matches('\u{feff}')).ok()?;
    let messages = fields
        .into_iter()
        .filter_map(|(key, value)| {
            let message = value.as_str().or_else(|| value.get("message")?.as_str())?;
            (!message.is_empty()).then(|| (key, message.into()))
        })
        .collect();
    Some(Bundle { path, messages })
}

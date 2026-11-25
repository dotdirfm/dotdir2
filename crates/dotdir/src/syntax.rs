use gpui_kit::component::highlighter::{GrammarConfig, LanguageRegistry};
use std::path::Path;

pub(crate) fn register_perl() {
    LanguageRegistry::singleton().register(
        "perl",
        &GrammarConfig::new(
            "perl",
            ts_parser_perl::LANGUAGE.into(),
            vec![],
            ts_parser_perl::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
    );
}

pub(crate) fn language_for(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|extension| extension.to_str());
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("rs") => "rust",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts" | "tsx" | "mts" | "cts") => "typescript",
        Some("json" | "jsonc") => "json",
        Some("toml") => "toml",
        Some("css") => "css",
        Some("md") => "markdown",
        Some("sh" | "zsh" | "bash") => "bash",
        Some("c" | "h") => "c",
        Some("go") => "go",
        Some("py" | "pyi" | "pyw") => "python",
        Some("pl" | "pm" | "t") => "perl",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{
        base::input::Rope,
        component::highlighter::{HighlightTheme, SyntaxHighlighter},
    };

    #[test]
    fn reported_file_types_produce_syntax_styles() {
        register_perl();
        let theme = HighlightTheme::default_dark();
        for (file, language, code) in [
            ("main.c", "c", "int main(void) { return 42; }\n"),
            ("api.h", "c", "int square(int value);\n"),
            (
                "main.go",
                "go",
                "package main\nfunc main() { println(42) }\n",
            ),
            ("main.py", "python", "def answer():\n    return 42\n"),
            ("api.pyi", "python", "def answer() -> int: ...\n"),
            (
                "main.pl",
                "perl",
                "my $answer = 42;\nprint \"$answer\\n\";\n",
            ),
            (
                "Foo.pm",
                "perl",
                "package Foo;\nsub answer { return 42; }\n1;\n",
            ),
        ] {
            let detected = language_for(Path::new(file));
            assert_eq!(detected, language, "{file}");
            let mut highlighter = SyntaxHighlighter::new(detected);
            assert_eq!(highlighter.language().as_ref(), language, "{file}");
            assert!(highlighter.update(None, &Rope::from(code), None), "{file}");
            let tree = highlighter.tree().expect(file);
            assert!(!tree.root_node().has_error(), "{file}");
            assert!(
                highlighter
                    .styles(&(0..code.len()), theme.as_ref())
                    .iter()
                    .any(|(_, style)| style.color.is_some()),
                "{file}"
            );
        }
    }
}

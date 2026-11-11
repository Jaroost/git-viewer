//! Syntax highlighting of diffs, language detected from the file name.

use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::theme::{EmbeddedThemeName, extra};

pub type Rgb = [u8; 3];

/// A piece of a line with its color. Concatenating the spans of a line gives back its text
/// (tabs are expanded to 4 spaces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub color: Rgb,
}

/// Lines longer than this (minified files…) are left plain: they would be slow and unreadable.
const MAX_LINE_LEN: usize = 2000;
/// Lines of a single file diff that get colored (about a second of work); the rest stays plain.
pub const MAX_HIGHLIGHT_LINES: usize = 5000;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// (dark theme, light theme)
fn themes() -> &'static (Theme, Theme) {
    static SET: OnceLock<(Theme, Theme)> = OnceLock::new();
    SET.get_or_init(|| {
        let set = extra();
        (
            set.get(EmbeddedThemeName::OneHalfDark).clone(),
            set.get(EmbeddedThemeName::OneHalfLight).clone(),
        )
    })
}

fn find_syntax(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let p = Path::new(path);
    // Whole file name first (Makefile, Dockerfile, Gemfile…), then the extension.
    let by_name = p.file_name().and_then(|n| n.to_str()).and_then(|n| ss.find_syntax_by_extension(n));
    by_name.or_else(|| {
        p.extension().and_then(|e| e.to_str()).and_then(|e| ss.find_syntax_by_extension(e))
    })
}

/// Human name of the language of `path` ("Rust", "TypeScript"…), if recognised.
pub fn language(path: &str) -> Option<&'static str> {
    find_syntax(path).map(|s| s.name.as_str())
}

/// Everything the diff view needs from the highlighter for one file.
#[derive(Debug, Clone, Default)]
pub struct Highlighted {
    pub language: Option<&'static str>,
    /// One entry per diff line, empty = draw plain (see [`highlight`]).
    pub lines: Vec<Vec<Span>>,
}

pub fn analyze(path: &str, lines: &[(char, &str)], dark: bool) -> Highlighted {
    Highlighted { language: language(path), lines: highlight(path, lines, dark) }
}

/// Highlights the lines of one file's diff. `lines` are `(origin, text)` as in a diff:
/// `'+'`, `'-'`, `' '` for content and `'H'` for a hunk header.
///
/// Returns one entry per input line; an empty entry means "draw it plain" (hunk headers,
/// unknown language, over-long lines). Added lines are highlighted as part of the new file and
/// removed lines as part of the old one, each with its own parser state, which is reset at every
/// hunk since a diff does not show what comes before it.
pub fn highlight(path: &str, lines: &[(char, &str)], dark: bool) -> Vec<Vec<Span>> {
    highlight_up_to(path, lines, dark, MAX_HIGHLIGHT_LINES)
}

fn highlight_up_to(path: &str, lines: &[(char, &str)], dark: bool, max_lines: usize) -> Vec<Vec<Span>> {
    highlight_diag(path, lines, dark, max_lines).0
}

/// Like [`highlight_up_to`], also returning how many lines the syntax engine failed on and the
/// first error message (for diagnostics).
fn highlight_diag(
    path: &str,
    lines: &[(char, &str)],
    dark: bool,
    max_lines: usize,
) -> (Vec<Vec<Span>>, usize, Option<String>) {
    let mut out = vec![Vec::new(); lines.len()];
    let (mut errors, mut first_error) = (0usize, None);
    let Some(syntax) = find_syntax(path) else { return (out, 0, None) };
    let ss = syntaxes();
    let theme = if dark { &themes().0 } else { &themes().1 };
    let (mut old, mut new) = (HighlightLines::new(syntax, theme), HighlightLines::new(syntax, theme));

    for (i, (origin, text)) in lines.iter().enumerate().take(max_lines) {
        if *origin == 'H' {
            old = HighlightLines::new(syntax, theme);
            new = HighlightLines::new(syntax, theme);
            continue;
        }
        if text.len() > MAX_LINE_LEN {
            continue;
        }
        let line = format!("{text}\n"); // the "newlines" syntaxes expect line endings
        let ranges = match origin {
            '-' => old.highlight_line(&line, ss),
            '+' => new.highlight_line(&line, ss),
            _ => {
                // Context lines exist on both sides: keep both parsers in sync.
                let _ = old.highlight_line(&line, ss);
                new.highlight_line(&line, ss)
            }
        };
        let ranges = match ranges {
            Ok(r) => r,
            Err(e) => {
                errors += 1;
                first_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        out[i] = ranges
            .into_iter()
            .filter_map(|(style, s)| {
                let s = s.trim_end_matches(['\n', '\r']);
                (!s.is_empty()).then(|| Span {
                    text: s.replace('\t', "    "),
                    color: [style.foreground.r, style.foreground.g, style.foreground.b],
                })
            })
            .collect();
    }
    (out, errors, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn languages_are_detected_from_extension_or_file_name() {
        assert_eq!(language("src/main.rs"), Some("Rust"));
        assert_eq!(language("a/b/script.py"), Some("Python"));
        assert_eq!(language("app/models/user.rb"), Some("Ruby"));
        assert_eq!(language("Makefile"), Some("Makefile"));
        assert_eq!(language("sub/dir/Makefile"), Some("Makefile"));
        assert_eq!(language("notes.unknownext"), None);
        assert_eq!(language("no_extension_file"), None);
        assert_eq!(language(""), None);
    }

    #[test]
    fn the_languages_of_the_projects_we_open_are_covered() {
        // TypeScript, Vue, TOML and ERB are not in syntect's default pack.
        for (file, expected) in [
            ("web/app.ts", "TypeScript"),
            ("Cargo.toml", "TOML"),
            ("config/app.yml", "YAML"),
            ("README.md", "Markdown"),
            ("index.html", "HTML"),
            ("style.css", "CSS"),
            ("data.json", "JSON"),
        ] {
            assert_eq!(language(file), Some(expected), "{file}");
        }
        assert!(language("components/Header.vue").is_some(), "Vue");
        assert!(language("views/index.html.erb").is_some(), "ERB");
    }

    #[test]
    fn spans_rebuild_the_original_line() {
        let lines = [
            ('H', "@@ -1,3 +1,3 @@"),
            (' ', "fn main() {"),
            ('-', "    let x = 1; // old"),
            ('+', "\tlet s = \"héllo\"; // new"),
            (' ', "}"),
        ];
        let out = highlight("main.rs", &lines, true);
        assert_eq!(out.len(), lines.len());
        assert!(out[0].is_empty()); // hunk header stays plain
        for (i, (_, text)) in lines.iter().enumerate().skip(1) {
            assert_eq!(joined(&out[i]), text.replace('\t', "    "), "line {i}");
        }
    }

    #[test]
    fn code_gets_several_colors() {
        let lines = [('+', "fn main() { let s = \"text\"; } // comment")];
        let out = highlight("main.rs", &lines, true);
        let colors: std::collections::HashSet<_> = out[0].iter().map(|s| s.color).collect();
        assert!(colors.len() >= 3, "keyword/string/comment should differ: {:?}", out[0]);
    }

    #[test]
    fn dark_and_light_themes_differ() {
        let lines = [('+', "fn main() {}")];
        assert_ne!(highlight("a.rs", &lines, true), highlight("a.rs", &lines, false));
    }

    #[test]
    fn unknown_language_or_empty_input_is_left_plain() {
        let lines = [('H', "@@"), ('+', "whatever")];
        assert!(highlight("file.unknownext", &lines, true).iter().all(|l| l.is_empty()));
        assert!(highlight("a.rs", &[], true).is_empty());
    }

    #[test]
    fn overlong_lines_are_left_plain_but_do_not_break_the_next_ones() {
        let long = "x".repeat(MAX_LINE_LEN + 1);
        let lines = [('+', long.as_str()), ('+', "let a = 1;")];
        let out = highlight("a.rs", &lines, true);
        assert!(out[0].is_empty());
        assert_eq!(joined(&out[1]), "let a = 1;");
    }

    #[test]
    fn removed_lines_are_parsed_with_the_old_file_state() {
        // The old side opens a block comment that the new side does not: the removed line
        // inside it must be colored as a comment, the added one as code.
        let lines = [
            ('H', "@@ -1,3 +1,2 @@"),
            ('-', "/* start of a comment"),
            ('-', "let hidden = 1;"),
            ('+', "let shown = 2;"),
        ];
        let out = highlight("a.rs", &lines, true);
        let comment_color = out[1][0].color;
        assert!(out[2].iter().all(|s| s.color == comment_color), "{:?}", out[2]);
        assert!(out[3].iter().any(|s| s.color != comment_color), "{:?}", out[3]);
    }

    #[test]
    fn parser_state_resets_at_each_hunk() {
        let lines = [
            ('H', "@@ -1 +1 @@"),
            ('+', "/* unterminated comment"),
            ('H', "@@ -50 +50 @@"),
            ('+', "let code = 1;"),
        ];
        let out = highlight("a.rs", &lines, true);
        let comment_color = out[1][0].color;
        assert!(out[3].iter().any(|s| s.color != comment_color), "{:?}", out[3]);
    }

    #[test]
    fn analyze_returns_the_language_with_the_highlighted_lines() {
        let h = analyze("lib.rs", &[('H', "@@"), ('+', "let a = 1;")], true);
        assert_eq!(h.language, Some("Rust"));
        assert_eq!(h.lines.len(), 2);
        let none = analyze("notes.zzz", &[('+', "x")], true);
        assert_eq!(none.language, None);
        assert!(none.lines[0].is_empty());
    }

    #[test]
    fn lines_past_the_limit_are_left_plain() {
        let lines = [('+', "let a = 1;"), ('+', "let b = 2;"), ('+', "let c = 3;")];
        let out = highlight_up_to("a.rs", &lines, true, 2);
        assert_eq!(out.len(), 3);
        assert!(!out[0].is_empty() && !out[1].is_empty());
        assert!(out[2].is_empty());
    }

    /// Which real files does the syntax engine fail on?
    /// `GV_BENCH_REPO=/path cargo test diagnose_highlight -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn diagnose_highlight_errors() {
        use crate::repo::Repo;
        let path = std::env::var("GV_BENCH_REPO").expect("set GV_BENCH_REPO");
        let repo = Repo::open(Path::new(&path)).unwrap();
        let mut seen = std::collections::HashSet::new();
        for c in repo.commits(60).unwrap() {
            let Ok(d) = repo.commit_detail(&c.id) else { continue };
            for f in d.files.iter().filter(|f| !f.binary && !f.lines.is_empty()) {
                let lines: Vec<(char, &str)> = f.lines.iter().map(|l| (l.origin, l.text.as_str())).collect();
                let (out, errors, first) = highlight_diag(&f.path, &lines, true, usize::MAX);
                let colored = out.iter().filter(|l| !l.is_empty()).count();
                if errors > 0 && seen.insert((f.path.clone(), errors)) {
                    println!(
                        "{} [{}]: {errors} errors / {} lines ({colored} colored); first: {}",
                        f.path,
                        language(&f.path).unwrap_or("-"),
                        lines.len(),
                        first.unwrap_or_default()
                    );
                }
            }
        }
    }

    /// Manual benchmark on a real repository:
    /// `GV_BENCH_REPO=/path cargo test --release bench_highlight -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_highlight_real_diffs() {
        use crate::repo::Repo;
        use std::time::Instant;
        let path = std::env::var("GV_BENCH_REPO").expect("set GV_BENCH_REPO");
        let repo = Repo::open(Path::new(&path)).unwrap();
        let t = Instant::now();
        let _ = language("a.rs"); // loads the syntax set
        println!("syntax set loading: {:?}", t.elapsed());

        let mut worst: Vec<(usize, String, std::time::Duration, usize)> = Vec::new();
        let (mut total_lines, mut total_time) = (0usize, std::time::Duration::ZERO);
        for c in repo.commits(300).unwrap() {
            let Ok(d) = repo.commit_detail(&c.id) else { continue };
            for f in d.files.iter().filter(|f| !f.binary && !f.lines.is_empty()) {
                let lines: Vec<(char, &str)> = f.lines.iter().map(|l| (l.origin, l.text.as_str())).collect();
                let t = Instant::now();
                let h = analyze(&f.path, &lines, true);
                let el = t.elapsed();
                total_lines += lines.len();
                total_time += el;
                let colored = h.lines.iter().filter(|l| !l.is_empty()).count();
                worst.push((lines.len(), f.path.clone(), el, colored));
            }
        }
        worst.sort_by_key(|w| std::cmp::Reverse(w.2));
        println!("{} lines highlighted in {:?}", total_lines, total_time);
        for (n, p, el, colored) in worst.iter().take(5) {
            println!("  {el:?}  {n} lines ({colored} colored)  {p}");
        }
    }
}

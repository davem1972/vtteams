//! Syntax colouring for fenced code.
//!
//! Every language the tree-sitter based `syntax-color-objectscript` crates bundle (ObjectScript and its class/routine
//! flavours, SQL, Python, JSON, YAML, XML, HTML, CSS, JavaScript, Markdown, TOML, diff, Rust ...: see
//! `Grammar::supported_names()`) goes through them; everything else uses `syntect`.

use crate::termtext::ansi_to_lines;
use highlight_spans::highlight_structures::{Grammar, SpanHighlighter};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme as SyntTheme, ThemeSet};
use syntect::parsing::SyntaxSet;

struct Engines {
    syntaxes: SyntaxSet,
    theme: SyntTheme,
    os_theme: Option<theme_engine::theme_structures::Theme>,
}

fn engines() -> &'static Engines {
    static E: OnceLock<Engines> = OnceLock::new();
    E.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        Engines {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes.remove("base16-mocha.dark").or_else(|| themes.into_values().next()).unwrap_or_default(),
            os_theme: theme_engine::common::load_theme("tokyonight-dark")
                .map_err(|e| log::warn!("the ObjectScript theme did not load, so ObjectScript code will not be coloured: {e:?}"))
                .ok(),
        }
    })
}

/// Which tree-sitter grammar (if any) a fence label selects: whatever the highlighter's bundle knows by that name, asked
/// of it directly, so a language added to the bundle is picked up without a change here; syntect takes the rest. Only
/// the spellings of ObjectScript that vtteams has always accepted and the bundle's table lacks are added. (`cls` is the
/// bundle's own: compared on a class definition its plain grammar colours exactly what the class-definition grammar
/// does, and does better on a bare snippet.)
fn ts_grammar(lang: &str) -> Option<Grammar> {
    match lang {
        "cos" | "iris" | "cache" | "objectscript-core" => Some(Grammar::ObjectScript),
        "objectscript-class" => Some(Grammar::ObjectScriptUdl),
        other => Grammar::from_name(other),
    }
}
/// Map common fence labels onto names syntect knows (its default pack has no TypeScript, TOML, PowerShell ...).
fn syntect_token(lang: &str) -> &str {
    match lang {
        "typescript" | "ts" | "tsx" | "jsx" | "javascript" | "node" => "js",
        "bash" | "shell" | "zsh" | "sh" | "console" | "shell-session" => "sh",
        "c++" | "cxx" | "hpp" => "cpp",
        "c#" | "csharp" => "cs",
        "yml" => "yaml",
        "golang" => "go",
        "py" | "python3" => "python",
        "rs" => "rust",
        "kt" | "kotlin" => "java",
        "text" | "txt" | "plain" | "plaintext" => "",
        other => other,
    }
}

fn plain(code: &str) -> Vec<Line<'static>> {
    code.lines()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect()
}

/// One tree-sitter highlighter, built the first time something needs it. Building one compiles every grammar's queries
/// (about 340 ms in a release build, several times that unoptimised), so it is built once and reused; creating one per
/// code block would let a message with many small blocks stall the UI thread.
///
/// The construction happens under the lock, so whoever needs it first builds it and every other caller (the warm-up
/// thread and a render at once, say) waits for that one construction instead of starting a second.
struct Shared {
    slot: Mutex<Option<SpanHighlighter>>,
    builds: AtomicUsize,
}

impl Shared {
    const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            builds: AtomicUsize::new(0),
        }
    }

    /// Run `f` with the highlighter, building it first if nobody has. `None` when it cannot be built. The lock is held
    /// for `f`, so `f` should do nothing but highlight.
    fn with<R>(&self, f: impl FnOnce(&mut SpanHighlighter) -> Option<R>) -> Option<R> {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            self.builds.fetch_add(1, Ordering::Relaxed);
            *slot = SpanHighlighter::new().ok();
        }
        f(slot.as_mut()?)
    }

    /// How many times the highlighter has been built (more than once only if building failed and was tried again).
    #[cfg(test)]
    fn builds(&self) -> usize {
        self.builds.load(Ordering::Relaxed)
    }
}

fn ts_highlighter() -> &'static Shared {
    static H: Shared = Shared::new();
    &H
}

fn ts_highlight(code: &str, grammar: Grammar) -> Option<Vec<Line<'static>>> {
    ts_highlight_with(ts_highlighter(), code, grammar)
}

fn ts_highlight_with(shared: &Shared, code: &str, grammar: Grammar) -> Option<Vec<Line<'static>>> {
    let theme = engines().os_theme.as_ref()?;
    let src = code.replace("\r\n", "\n");
    let ansi = shared.with(|highlighter| {
        render_ansi::common::highlight_to_ansi(
            src.as_bytes(),
            grammar,
            theme,
            Some(highlighter),
            None,
            None,
        )
        .ok()
    })?;
    let mut lines = ansi_to_lines(&ansi);
    // The renderer paints the theme's background behind everything; the caller owns the block background, so that
    // one goes. A different background is part of the colouring (a diff tints added and removed lines) and stays.
    let base = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .find_map(|s| s.style.bg);
    for l in &mut lines {
        for s in &mut l.spans {
            if s.style.bg == base {
                s.style.bg = None;
            }
        }
    }
    Some(lines)
}

fn syntect_highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    let e = engines();
    let token = syntect_token(lang);
    let Some(syntax) = (!token.is_empty())
        .then(|| e.syntaxes.find_syntax_by_token(token))
        .flatten()
    else {
        return plain(code);
    };
    let mut hl = HighlightLines::new(syntax, &e.theme);
    let mut out = Vec::new();
    for line in code.replace("\r\n", "\n").lines() {
        // The default syntaxes are "newlines" flavoured: they expect each line to end in \n.
        let with_nl = format!("{line}\n");
        match hl.highlight_line(&with_nl, &e.syntaxes) {
            Ok(ranges) => out.push(Line::from(
                ranges
                    .into_iter()
                    .map(|(st, text)| {
                        let c = st.foreground;
                        let mut style = Style::new().fg(Color::Rgb(c.r, c.g, c.b));
                        if st
                            .font_style
                            .contains(syntect::highlighting::FontStyle::BOLD)
                        {
                            style = style.bold();
                        }
                        if st
                            .font_style
                            .contains(syntect::highlighting::FontStyle::ITALIC)
                        {
                            style = style.italic();
                        }
                        Span::styled(text.trim_end_matches('\n').to_string(), style)
                    })
                    .filter(|s| !s.content.is_empty())
                    .collect::<Vec<_>>(),
            )),
            Err(_) => out.push(Line::from(line.to_string())),
        }
    }
    out
}

/// A stretch of source text (byte offsets) and the highlight.js class Teams colours it with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClassRange {
    pub start: usize,
    pub end: usize,
    /// Without the `hljs-` prefix: `keyword`, `string`, ...
    pub class: &'static str,
}

/// The colour a code card gives a highlight.js class, or `None` for the classes drawn in the plain text colour.
///
/// Teams'' stylesheet for a card styles only six classes (keyword indigo, string green, function red, comment grey, and
/// the diff pair), checked by sending a card with a line for every class in highlight.js''s documented list. But it
/// keeps and draws an inline `style="color:#rrggbb"` on a span, which wins over the class (sent as a second test), so
/// vtteams sends the colour itself. The class stays on the span for what reads it by class.
///
/// The colours are chosen to be readable on both themes: each has the same luminance, giving about 3.8 : 1 against the
/// white of the light theme and against `#292929` of the dark one (more against black), which is the best a single
/// colour can do for both. Variables, parameters, operators and punctuation are left plain, as in most themes.
pub fn class_colour(class: &str) -> Option<&'static str> {
    Some(match class {
        "keyword" => "#a266df",
        "built_in" => "#249288",
        "literal" => "#b9720f",
        "number" => "#d75e18",
        "string" => "#2b963d",
        "regexp" => "#25936f",
        "symbol" => "#cf5894",
        "function" | "title" | "section" => "#3c82e3",
        "type" => "#2c9099",
        "attr" | "property" => "#988126",
        "comment" | "doctag" => "#838383",
        "meta" => "#c05ac0",
        "tag" | "name" => "#df5548",
        "addition" => "#26972f",
        "deletion" => "#e54d48",
        _ => return None,
    })
}
/// highlight.js class for a tree-sitter capture (`@keyword`, `function.builtin`, ...). Operators and punctuation are
/// not coloured by highlight.js, so they have none.
fn capture_class(name: &str) -> Option<&'static str> {
    let name = name.trim_start_matches('@');
    let (head, rest) = name.split_once('.').unwrap_or((name, ""));
    Some(match (head, rest) {
        ("keyword", "operator") | ("operator", _) | ("punctuation", _) => return None,
        ("keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass", _) => {
            "keyword"
        }
        ("string", "escape") | ("escape", _) => "subst",
        // A key (JSON, YAML, TOML) is a name, not a value.
        ("string", "special.key") => "attr",
        ("markup" | "text", "heading" | "title") => "section",
        ("string" | "character", _) => "string",
        ("comment", _) => "comment",
        ("number" | "float", _) => "number",
        ("boolean", _) | ("constant", "builtin") => "literal",
        ("constant", _) => "literal",
        // Where a function is called rather than named, Teams' own cards are plain.
        ("function" | "method", rest) if rest.ends_with("call") => return None,
        ("function" | "method" | "constructor", "builtin") => "built_in",
        ("function" | "method" | "constructor", _) => "function",
        ("type", "builtin") | ("variable", "builtin") => "built_in",
        ("type", _) => "type",
        ("variable", "parameter") => "params",
        ("variable", _) => "variable",
        ("property" | "attribute" | "field", _) => "attr",
        ("tag", _) => "name",
        ("label", _) => "symbol",
        ("macro" | "preproc" | "define", _) => "meta",
        ("diff", "plus") => "addition",
        ("diff", "minus") => "deletion",
        _ => return None,
    })
}

/// highlight.js class for a TextMate scope as syntect reports it (`keyword.control.rust`).
fn scope_class(scope: &str) -> Option<&'static str> {
    const RULES: [(&str, &str); 18] = [
        ("comment", "comment"),
        ("string.regexp", "regexp"),
        ("string", "string"),
        ("constant.numeric", "number"),
        ("constant.language", "literal"),
        ("constant.character.escape", "subst"),
        ("keyword.operator", ""),
        ("keyword", "keyword"),
        ("storage", "keyword"),
        ("entity.name.function", "function"),
        ("support.function", "built_in"),
        ("entity.name", "title"),
        ("support", "built_in"),
        ("variable.parameter", "params"),
        ("variable.language", "built_in"),
        ("meta.annotation", "meta"),
        ("markup.inserted", "addition"),
        ("markup.deleted", "deletion"),
    ];
    RULES
        .iter()
        .find(|(prefix, _)| scope == *prefix || scope.starts_with(&format!("{prefix}.")))
        .and_then(|(_, class)| (!class.is_empty()).then_some(*class))
}

/// The ObjectScript grammar captures every argument of a call as `variable.parameter`, a string or a `$ZVERSION`
/// included, which a theme paints one colour. Teams draws a parameter plain, so those are told apart by their text.
fn refine_parameter(text: &str) -> &'static str {
    match text.trim_start().chars().next() {
        Some('"') => "string",
        Some('$') => "built_in",
        Some(c) if c.is_ascii_digit() => "number",
        _ => "params",
    }
}

fn ts_ranges(code: &str, grammar: Grammar) -> Option<Vec<ClassRange>> {
    let result = ts_highlighter().with(|h| h.highlight(code.as_bytes(), grammar).ok())?;
    // Spans may overlap (a capture inside another); the later one wins, as when the colours are drawn.
    let mut class: Vec<Option<&'static str>> = vec![None; code.len()];
    for span in &result.spans {
        let name = result.attrs.iter().find(|a| a.id == span.attr_id)?;
        let end = span.end_byte.min(code.len());
        let found = capture_class(&name.capture_name).map(|c| match c {
            "params" => code.get(span.start_byte..end).map_or(c, refine_parameter),
            other => other,
        });
        if let Some(c) = found.filter(|c| class_colour(c).is_some())
            && span.start_byte < end
        {
            class[span.start_byte..end].fill(Some(c));
        }
    }
    Some(runs(&class))
}

fn syntect_ranges(code: &str, lang: &str) -> Option<Vec<ClassRange>> {
    use syntect::easy::ScopeRegionIterator;
    use syntect::parsing::{ParseState, ScopeStack};
    let e = engines();
    let token = syntect_token(lang);
    let syntax = (!token.is_empty())
        .then(|| e.syntaxes.find_syntax_by_token(token))
        .flatten()?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut class: Vec<Option<&'static str>> = vec![None; code.len()];
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        // The default syntaxes expect each line to end in a newline.
        let with_nl = if line.ends_with('\n') {
            std::borrow::Cow::Borrowed(line)
        } else {
            std::borrow::Cow::Owned(format!("{line}\n"))
        };
        let ops = state.parse_line(&with_nl, &e.syntaxes).ok()?;
        let mut at = 0;
        for (text, op) in ScopeRegionIterator::new(&ops, &with_nl) {
            stack.apply(op).ok()?;
            let found = stack.as_slice().iter().rev().find_map(|scope| {
                scope_class(&scope.build_string()).filter(|c| class_colour(c).is_some())
            });
            let end = (at + text.len()).min(line.len());
            if let Some(c) = found
                && at < end
            {
                class[offset + at..offset + end].fill(Some(c));
            }
            at += text.len();
        }
        offset += line.len();
    }
    Some(runs(&class))
}

/// Consecutive bytes with the same class, as ranges (never splitting a character: classes are set on whole regions).
fn runs(class: &[Option<&'static str>]) -> Vec<ClassRange> {
    let mut out: Vec<ClassRange> = Vec::new();
    for (i, c) in class.iter().enumerate() {
        let Some(c) = c else { continue };
        match out.last_mut() {
            Some(last) if last.end == i && last.class == *c => last.end = i + 1,
            _ => out.push(ClassRange {
                start: i,
                end: i + 1,
                class: c,
            }),
        }
    }
    out
}

/// Where each highlight.js class that has a colour ([`class_colour`]) applies in `code` for the fence label `lang`, with
/// the same highlighters that colour the block in vtteams (tree-sitter for the languages its bundle has, syntect for the
/// rest).
/// Empty for a language neither knows. The classes, not the language, decide the colours, so they also work for a
/// language Teams does not list.
pub fn hljs_ranges(code: &str, lang: &str) -> Vec<ClassRange> {
    let lang = lang.trim().to_lowercase();
    let ranges = ts_grammar(&lang)
        .and_then(|grammar| ts_ranges(code, grammar))
        .or_else(|| syntect_ranges(code, &lang));
    // Ranges are byte offsets and must fall on character boundaries to be sliced.
    ranges
        .unwrap_or_default()
        .into_iter()
        .filter(|r| code.is_char_boundary(r.start) && code.is_char_boundary(r.end))
        .collect()
}

/// The language of the file a diff header names, as a fence label for [`highlight`]: its extension (`x.rs` is `rs`,
/// which syntect finds by extension and the bundle by alias). `/dev/null`, a name without an extension and a nested
/// diff give none.
fn diff_file_language(path: &str) -> Option<String> {
    let name = path.split(['\t', ' ']).next()?.rsplit('/').next()?;
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    (!ext.is_empty() && !matches!(ext.as_str(), "diff" | "patch")).then_some(ext)
}

/// The hunk lines of each file in a diff, with the language of the file: the line numbers of its ` `, `+` and `-`
/// lines. A file starts at its `---`/`+++` pair (or a `diff` line) and its lines are the ones after an `@@`.
fn diff_sections(lines: &[&str]) -> Vec<(String, Vec<usize>)> {
    let mut sections = Vec::new();
    let mut language: Option<String> = None;
    let mut body: Vec<usize> = Vec::new();
    let mut in_hunk = false;
    let mut flush = |language: &mut Option<String>, body: &mut Vec<usize>| {
        if let Some(l) = language.take()
            && !body.is_empty()
        {
            sections.push((l, std::mem::take(body)));
        }
        body.clear();
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("diff ") {
            flush(&mut language, &mut body);
            in_hunk = false;
        } else if line.starts_with("--- ")
            && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))
        {
            flush(&mut language, &mut body);
            in_hunk = false;
            language =
                diff_file_language(&lines[i + 1][4..]).or_else(|| diff_file_language(&line[4..]));
            i += 2;
            continue;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && (line.is_empty() || line.starts_with(['+', '-', ' '])) {
            body.push(i);
        }
        i += 1;
    }
    flush(&mut language, &mut body);
    sections
}

/// Gives the hunk lines of a diff the colours of the language they are written in, under the diff's own: its `+`/`-`
/// marker and line tint stay, and the code after the marker is coloured as the file's language would be (by the
/// bundle where it has the language, else syntect). The bundle's engine does this itself for the few languages it
/// injects by file name; this covers the rest (a `.rs` hunk).
fn colour_diff_hunks(code: &str, base: Vec<Line<'static>>) -> Vec<Line<'static>> {
    // As many lines as the highlighters give: a final newline does not make another.
    let source: Vec<&str> = code.lines().collect();
    if base.len() != source.len() {
        return base;
    }
    let mut out = base.clone();
    for (language, rows) in diff_sections(&source) {
        let text = rows
            .iter()
            .map(|&r| source[r].get(1..).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let coloured = highlight(&text, &language);
        if coloured.len() != rows.len() {
            continue;
        }
        for (&row, code_line) in rows.iter().zip(coloured) {
            let (Some(first), Some(marker)) = (base[row].spans.first(), source[row].chars().next())
            else {
                continue;
            };
            let tint = base[row].spans.iter().find_map(|s| s.style.bg);
            let mut spans = vec![Span::styled(marker.to_string(), first.style)];
            spans.extend(code_line.spans.into_iter().map(|mut s| {
                s.style.bg = tint;
                s
            }));
            out[row] = Line::from(spans);
        }
    }
    out
}

/// `code` without colour, one `Line` per source line, tabs expanded (what an uncoloured block shows).
pub fn uncoloured(code: &str) -> Vec<Line<'static>> {
    let lines = plain(&code.replace('\t', "    "));
    if lines.is_empty() {
        vec![Line::default()]
    } else {
        lines
    }
}

/// Colour `code` for the fence label `lang` (case-insensitive; may be empty). One `Line` per source line, tabs expanded.
pub fn highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    let lang = lang.trim().to_lowercase();
    let code = code.replace('\t', "    ");
    let grammar = ts_grammar(&lang);
    let mut lines = grammar
        .and_then(|g| ts_highlight(&code, g))
        .unwrap_or_else(|| syntect_highlight(&code, &lang));
    if grammar == Some(Grammar::Diff) {
        lines = colour_diff_hunks(&code, lines);
    }
    if lines.is_empty() {
        vec![Line::default()]
    } else {
        lines
    }
}

/// One line for each family of language that is slow the first time it is used: Rust, Python, SQL, JSON and ObjectScript
/// through tree-sitter, and Go for syntect, whose first use loads its whole syntax set and theme.
const WARM_SNIPPETS: [(&str, &str); 6] = [
    ("rust", "fn main() { let x = \"a\"; }"),
    ("python", "def f(x): return x + 1"),
    ("sql", "SELECT a FROM t WHERE b = 1"),
    ("json", "{\"a\": [1, true]}"),
    ("objectscript", "Set x = $Piece(y, \",\", 1)"),
    ("go", "func main() { x := 1 }"),
];

/// Colour each of [`WARM_SNIPPETS`] through `shared`, so that what the first real block needs is built. Returns how many
/// were coloured.
fn warm_with(shared: &Shared) -> usize {
    WARM_SNIPPETS
        .iter()
        .filter(|(lang, code)| match ts_grammar(lang) {
            Some(grammar) => ts_highlight_with(shared, code, grammar).is_some(),
            None => !syntect_highlight(code, lang).is_empty(),
        })
        .count()
}

/// Run `f`, answering `None` if it panicked: the warm-up is an optimisation and must never take anything down.
fn contained<R>(f: impl FnOnce() -> R) -> Option<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Whether a run warms the highlighter at start-up: an interactive one does, a one-shot (`--dump`) is deterministic and
/// should not start work it will not wait for.
pub fn warms_at_start(one_shot: bool) -> bool {
    !one_shot
}

/// Build the shared tree-sitter highlighter and the syntect engines on a thread of its own (`vtteams-warm`) and colour a
/// line of each slow-to-start language, so the first code block a person sees costs what the second does. A render that
/// needs the highlighter while this is building it waits for that one construction rather than starting another. Logs
/// how long it took (a count of languages and milliseconds, never any content); a panic in it is contained and logged.
/// `None` if no thread could be had, in which case the first block builds the highlighter itself, as it always did.
pub fn warm_in_background() -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("vtteams-warm".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            match contained(|| warm_with(ts_highlighter())) {
                Some(done) => log::debug!(
                    "highlighter warm-up: {done} of {} languages in {} ms",
                    WARM_SNIPPETS.len(),
                    started.elapsed().as_millis()
                ),
                None => log::debug!(
                    "highlighter warm-up: panicked after {} ms; the first block builds it instead",
                    started.elapsed().as_millis()
                ),
            }
        })
        .ok()
}

/// A guess at the language of code that arrived without a tag, for the one language Teams cannot carry: it keeps the
/// tag only for languages it knows, so a block sent as `objectscript` comes back with none. Only ObjectScript is
/// guessed, and only on signs other languages do not have together: `##class(`, a `$$$macro` or `$System.` anywhere,
/// or an ObjectScript command (`Set`, `Do`, `Quit` ...) beginning a line together with a `$Function(` or a `^global`.
pub fn guess_language(code: &str) -> Option<&'static str> {
    let lower = code.to_ascii_lowercase();
    let macro_use = lower
        .match_indices("$$$")
        .any(|(i, _)| lower[i + 3..].starts_with(|c: char| c.is_ascii_alphabetic()));
    if lower.contains("##class(") || lower.contains("$system.") || macro_use {
        return Some("objectscript");
    }
    const COMMANDS: [&str; 12] = [
        "set",
        "do",
        "quit",
        "write",
        "kill",
        "new",
        "zwrite",
        "hang",
        "lock",
        "tstart",
        "tcommit",
        "trollback",
    ];
    let starts_with_command = lower.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().is_some_and(|w| {
            let w = w.split(['(', ':']).next().unwrap_or(w);
            COMMANDS.contains(&w)
        })
    });
    let intrinsic = lower.match_indices('$').any(|(i, _)| {
        let after = &lower[i + 1..];
        let name = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        (2..=12).contains(&name)
            && after[name..].starts_with('(')
            && !lower[..i].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '$')
    });
    let global = lower.match_indices('^').any(|(i, _)| {
        lower[i + 1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '%')
            && !lower[..i].ends_with(|c: char| c.is_ascii_alphanumeric())
    });
    (starts_with_command && (intrinsic || global)).then_some("objectscript")
}

/// Nicer label for the code-block header.
pub fn language_label(lang: &str) -> String {
    match lang.trim().to_lowercase().as_str() {
        "" => "code".to_string(),
        "cos" | "objectscript" | "iris" => "ObjectScript".to_string(),
        "udl" | "cls" => "ObjectScript class".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colours(lines: &[Line<'_>]) -> usize {
        let mut set = std::collections::HashSet::new();
        for l in lines {
            for s in &l.spans {
                if let Some(c) = s.style.fg {
                    set.insert(format!("{c:?}"));
                }
            }
        }
        set.len()
    }

    const RUST: &str = "fn main() {\n    let x = \"hi\"; // c\n}";

    fn build_rust(shared: &Shared) -> Option<()> {
        shared.with(|h| {
            h.highlight(RUST.as_bytes(), Grammar::from_name("rust")?)
                .ok()
                .map(|_| ())
        })
    }

    #[test]
    fn concurrent_first_uses_build_the_highlighter_once() {
        let shared = Shared::new();
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    start.wait();
                    assert!(build_rust(&shared).is_some());
                });
            }
        });
        assert_eq!(shared.builds(), 1);
    }

    #[test]
    fn after_the_warm_up_a_rust_block_builds_nothing() {
        let shared = Shared::new();
        assert_eq!(warm_with(&shared), WARM_SNIPPETS.len());
        assert_eq!(shared.builds(), 1);
        assert!(ts_highlight_with(&shared, RUST, Grammar::from_name("rust").unwrap()).is_some());
        assert_eq!(shared.builds(), 1);
    }

    #[test]
    fn the_background_warm_up_leaves_the_shared_highlighter_built_once() {
        warm_in_background()
            .expect("a thread")
            .join()
            .expect("the warm-up does not panic");
        assert_eq!(ts_highlighter().builds(), 1);
        assert!(!highlight(RUST, "rust").is_empty());
        assert_eq!(ts_highlighter().builds(), 1);
    }

    #[test]
    fn a_panic_in_the_warm_up_is_contained() {
        assert_eq!(contained(|| 5), Some(5));
        assert_eq!(contained(|| -> u8 { panic!("a test panic") }), None);
        // A panic while the highlighter is locked does not poison it for the next caller, nor build it again.
        let shared = Shared::new();
        assert_eq!(
            contained(|| shared.with(|_| -> Option<()> { panic!("a test panic") })),
            None
        );
        assert!(build_rust(&shared).is_some());
        assert_eq!(shared.builds(), 1);
    }

    #[test]
    fn only_an_interactive_run_warms_the_highlighter() {
        assert!(warms_at_start(false));
        assert!(!warms_at_start(true));
    }

    /// How long the first code block costs in a fresh process, and the second for comparison.
    /// `cargo test --release highlight_first_use_cost -- --ignored --nocapture` (and again without `--release`, which is
    /// what `cargo run` builds). Run it alone: another test in the same process would warm what it measures.
    #[test]
    #[ignore = "a measurement, run on demand"]
    fn highlight_first_use_cost() {
        let time = |label: &str| {
            let t = std::time::Instant::now();
            let lines = highlight(RUST, "rust");
            println!(
                "{label}: {:.2} ms ({} lines)",
                t.elapsed().as_secs_f64() * 1e3,
                lines.len()
            );
        };
        time("first Rust block ");
        time("second Rust block");
        let t = std::time::Instant::now();
        let python = highlight("def f(x): return x", "python");
        println!(
            "first Python block after Rust: {:.2} ms ({} lines)",
            t.elapsed().as_secs_f64() * 1e3,
            python.len()
        );
    }

    /// The same after the start-up warm-up: the first block should cost what the second does, and the warm-up itself is
    /// what is paid on its own thread. `cargo test --release highlight_after_warm_up_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run on demand"]
    fn highlight_after_warm_up_cost() {
        let t = std::time::Instant::now();
        warm_with(ts_highlighter());
        println!(
            "warm-up (off the UI thread in the app): {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
        let time = |label: &str, code: &str, lang: &str| {
            let t = std::time::Instant::now();
            let lines = highlight(code, lang);
            println!(
                "{label}: {:.2} ms ({} lines)",
                t.elapsed().as_secs_f64() * 1e3,
                lines.len()
            );
        };
        time("first Rust block after warm-up  ", RUST, "rust");
        time("second Rust block               ", RUST, "rust");
        time(
            "first Python block after warm-up",
            "def f(x): return x",
            "python",
        );
        time(
            "first Go (syntect) after warm-up",
            "func main() { x := 1 }",
            "go",
        );
    }

    #[test]
    fn rust_goes_through_tree_sitter() {
        let lines = highlight("fn main() {\n    let x = \"hi\"; // c\n}", "rust");
        assert_eq!(lines.len(), 3);
        assert!(colours(&lines) >= 3, "{lines:?}");
    }

    #[test]
    fn objectscript_goes_through_tree_sitter() {
        let lines = highlight(
            "ClassMethod Hello() As %Status\n{\n    Write \"hi\", !\n    Quit $$$OK\n}",
            "objectscript",
        );
        assert!(lines.len() >= 4);
        assert!(colours(&lines) >= 2, "{lines:?}");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Write") && text.contains("Quit"));
    }

    #[test]
    fn what_the_objectscript_grammar_calls_a_parameter_is_classified_by_its_text() {
        let src = "Quit $TR($ZVERSION,\" \",\"@\",x,7)";
        let at = |text: &str| {
            let start = src.find(text).unwrap();
            hljs_ranges(src, "objectscript")
                .into_iter()
                .find(|r| r.start <= start && start < r.end)
                .map(|r| r.class)
        };
        assert_eq!(at("$ZVERSION"), Some("built_in"));
        assert_eq!(at("\" \""), Some("string"));
        assert_eq!(at("\"@\""), Some("string"));
        assert_eq!(at("7"), Some("number"));
        assert_eq!(at("x,"), None, "a plain argument stays plain");
        assert_eq!(at("Quit"), Some("keyword"));
    }

    #[test]
    fn the_languages_her_bundle_has_go_through_it_and_are_coloured() {
        for (lang, sample) in [
            ("python", "def f(a):\n    return \"x\" + a  # c"),
            ("py", "import os\nprint(1)"),
            ("sql", "SELECT a, COUNT(*) FROM t WHERE b = 'x' GROUP BY a"),
            ("json", "{\"a\": [1, true, null], \"b\": \"c\"}"),
            ("yaml", "a: 1\nb:\n  - x\n  - \"y\"\n# c"),
            ("css", "a.b { color: #fff; margin: 0 }"),
            ("html", "<div class=\"a\">hi</div>"),
            ("xml", "<a b=\"c\"><d>e</d></a>"),
            ("javascript", "const f = (a) => a + 1; // c"),
            ("js", "function f() { return \"x\"; }"),
            ("markdown", "# Title\n\n* item\n\n`code`"),
            ("diff", "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new"),
            ("rust", "fn main() { let x = 1; }"),
        ] {
            assert!(ts_grammar(lang).is_some(), "{lang} is in the bundle");
            assert!(
                colours(&highlight(sample, lang)) >= 2,
                "{lang}: {:?}",
                highlight(sample, lang)
            );
            assert!(
                !hljs_ranges(sample, lang).is_empty(),
                "{lang} has classes for Teams"
            );
        }
        // Whatever the bundle lists is the bundle's, with no list of ours to keep in step.
        for name in Grammar::supported_names() {
            assert!(ts_grammar(name).is_some(), "{name}");
        }
        // Not in the bundle: syntect still does these.
        for lang in ["go", "c", "java", "typescript", "bash", "ruby"] {
            assert!(ts_grammar(lang).is_none(), "{lang}");
        }
    }

    #[test]
    fn a_diff_keeps_the_colours_of_the_language_its_hunks_are_written_in() {
        let diff = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,3 @@\n fn a() {}\n-let x = 1;\n+let y = \"s\"; // c\n--- a/y.cls\n+++ b/y.cls\n@@ -1 +1 @@\n+Set x = $Get(^A)\n";
        let lines = highlight(diff, "diff");
        assert_eq!(lines.len(), diff.lines().count());
        let text = |l: &Line<'_>| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let row = |needle: &str| lines.iter().find(|l| text(l).contains(needle)).unwrap();
        let fgs = |l: &Line<'_>| {
            l.spans
                .iter()
                .filter_map(|s| s.style.fg)
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        // The text is unchanged: marker, then the code.
        assert_eq!(text(row("let y")), "+let y = \"s\"; // c");
        assert_eq!(text(row("fn a")), " fn a() {}");
        // A Rust hunk and an ObjectScript one (both in the bundle) are coloured, over the tint of an added line.
        assert!(fgs(row("let y")) >= 4, "{:?}", row("let y"));
        assert!(fgs(row("fn a")) >= 2, "{:?}", row("fn a"));
        assert!(fgs(row("Set x")) >= 3, "{:?}", row("Set x"));
        let added = row("let y")
            .spans
            .iter()
            .map(|s| s.style.bg)
            .collect::<Vec<_>>();
        assert!(
            added.iter().all(|b| b.is_some() && *b == added[0]),
            "{added:?}"
        );
        assert!(
            row("fn a").spans.iter().all(|s| s.style.bg.is_none()),
            "context is not tinted"
        );
        // A file whose language nothing knows keeps just the diff's own colours.
        let plain = highlight(
            "--- a/x.zzz\n+++ b/x.zzz\n@@ -1 +1 @@\n+hello world\n",
            "diff",
        );
        assert!(text(&plain[3]).starts_with("+hello"));
    }

    #[test]
    fn the_language_of_a_diffed_file_comes_from_its_name() {
        for (path, ext) in [
            ("b/src/x.rs", Some("rs")),
            ("b/Foo.CLS\t2026", Some("cls")),
            ("/dev/null", None),
            ("b/Makefile", None),
            ("b/x.diff", None),
        ] {
            assert_eq!(diff_file_language(path).as_deref(), ext, "{path}");
        }
    }

    #[test]
    fn objectscript_is_guessed_only_on_signs_other_languages_lack() {
        for code in [
            "Set err = $$$ERROR($$$AIFoo,baz)\nQuit err",
            "Do ##class(%SYSTEM.Process).Foo()",
            "Set x = $Get(^Counts(\"a\"), 0)\nQuit x",
            "set ^Log($Increment(^Log)) = $ZDateTime($Horolog)",
            "Write $System.Version.GetVersion()",
        ] {
            assert_eq!(guess_language(code), Some("objectscript"), "{code}");
        }
        for code in [
            "print(\"hello\")\nreturn x",
            "set -e\necho $HOME\nrm -rf ./x",
            "x = $.ajax({ url: \"a\" })",
            "$foo = bar($baz);",
            "Set x = 1\nQuit",
            "let a = 2^3;\nlet b = cost$ + 1;",
            "echo $$ and $$",
            "",
        ] {
            assert_eq!(guess_language(code), None, "{code}");
        }
    }

    #[test]
    fn unknown_languages_fall_back_to_plain_text() {
        let lines = highlight("just some text\nmore", "klingon");
        assert_eq!(lines.len(), 2);
        assert_eq!(colours(&lines), 0);
    }

    #[test]
    fn aliases_resolve() {
        assert!(colours(&highlight("def f():\n    return 1", "py")) >= 2);
        assert!(colours(&highlight("echo \"x\" | grep y", "bash")) >= 1);
    }

    #[test]
    fn empty_code_gives_one_empty_line() {
        assert_eq!(highlight("", "rust").len(), 1);
    }
}

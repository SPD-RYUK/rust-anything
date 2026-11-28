//! Small helpers over tree-sitter syntax trees, shared by the language plugins.

use tree_sitter::{Language, Node, Parser, Tree};

/// Parses `text`; `None` only when the grammar cannot be loaded.
pub fn parse(language: &Language, text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    parser.parse(text, None)
}

pub fn text<'t>(node: Node, source: &'t str) -> &'t str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// 1-based line of a node.
pub fn line(node: Node) -> u32 {
    u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)
}

/// Every node in the tree, depth first.
pub fn walk<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>)) {
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        visit(node);
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
}

pub fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Lines of code: lines that are neither blank nor a comment. `analyze`, `map` and `gate`'s size
/// check all count with it, so a unit's size means the same everywhere.
pub fn loc(text: &str, lang: crate::lang::Lang) -> usize {
    code_lines(text, lang.comment_prefixes())
}

/// Lines that are neither blank nor start with one of `comment`. A `<#` among the prefixes
/// (PowerShell) opens a block comment that runs to the matching `#>`, on a later line or the
/// same one; code outside the block on those lines still counts.
pub fn code_lines(text: &str, comment: &[&str]) -> usize {
    if !comment.contains(&"<#") {
        return text
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && !comment
                        .iter()
                        .any(|c| l.starts_with(c) || *l == c.trim_end())
            })
            .count();
    }
    let mut in_block = false;
    let mut quote = None;
    let mut count = 0;
    for raw in text.lines() {
        let outside = powershell_outside(raw, &mut in_block, &mut quote);
        let l = outside.trim();
        if !l.is_empty()
            && !comment
                .iter()
                .any(|c| l.starts_with(c) || l == c.trim_end())
        {
            count += 1;
        }
    }
    count
}

/// The part of a PowerShell line that is outside its comments: a `<#` block runs to its
/// `#>`, across lines or on the same one, and a `#` runs to the end of the line, trailing
/// code included. Quoted strings are kept as written and scanned first, so a delimiter
/// inside one, or after a `#`, does not open a block, and one before them still does.
/// A `--%` stop-parsing token turns the rest of the line into literal text: its quotes,
/// comments and delimiters are not parsed and cannot change the cross-line state.
fn powershell_outside(line: &str, in_block: &mut bool, quote: &mut Option<char>) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    // A comment only opens at a token boundary: at the start of the line, after
    // whitespace, or after an expression terminator (`;`, `|`, a bracket, `=` and
    // their kin, or a closing quote). Glued to a word it is part of the word, so
    // `foo<#bar` and `foo#bar` stay literal text.
    let mut boundary = true;
    while let Some(c) = chars.next() {
        if *in_block {
            if c == '#' && chars.peek() == Some(&'>') {
                chars.next();
                *in_block = false;
                out.push(' ');
                boundary = true;
            }
            continue;
        }
        if let Some(q) = *quote {
            out.push(c);
            if q == '\'' && c == '\'' {
                if chars.peek() == Some(&'\'') {
                    out.push(chars.next().unwrap_or_default());
                } else {
                    *quote = None;
                    boundary = true;
                }
            } else if q == '"' && c == '`' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if q == '"' && c == '"' {
                *quote = None;
                boundary = true;
            }
            continue;
        }
        // A backtick escapes the next character out here too (PowerShell's escape
        // character is not only for strings): an escaped quote opens no string, and
        // an escaped `#` or `<` opens no comment. Inside double quotes the quote
        // branch above already does the same.
        if c == '`' {
            out.push(c);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            boundary = false;
            continue;
        }
        // PowerShell's `--%` stop-parsing token: from here the rest of the line is
        // literal, so a quote or `<#` in it opens no string or block that would leak
        // onto the following lines, and a `#` in it starts no comment. Recognize it
        // before quotes and comments, and only at a token boundary (not `foo--%`).
        if c == '-' && boundary {
            let mut look = chars.clone();
            if look.next() == Some('-') && look.next() == Some('%') {
                out.push(c);
                out.extend(chars.by_ref());
                break;
            }
        }
        if c == '#' && boundary {
            break;
        }
        if c == '<' && boundary && chars.peek() == Some(&'#') {
            chars.next();
            *in_block = true;
            out.push(' ');
            boundary = true;
            continue;
        }
        if c == '\'' || c == '"' {
            *quote = Some(c);
            boundary = false;
            out.push(c);
            continue;
        }
        out.push(c);
        boundary = c.is_whitespace()
            || matches!(c, ';' | '|' | '(' | ')' | '{' | '}' | '=' | ',' | '&' | ':');
    }
    out
}

/// `text` with its `<!-- -->` comments removed (a `pom.xml`, a project file).
pub fn without_xml_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A build file with its comments removed, so a commented-out `implementation("a:b:1")` is not
/// read as a dependency the project declares. `//` counts only at the start of a line: a trailing
/// one follows code the project does declare.
pub fn without_slash_comments(text: &str) -> String {
    let mut unblocked = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                unblocked.push(c);
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        unblocked.push(escaped);
                    }
                } else if c == q || c == '\n' {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                unblocked.push(c);
            }
            None if c == '/' && chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            None if c == '/' && chars.peek() == Some(&'/') => {
                // `//` ends the line: a `/*` inside it is text, not the start of a block comment,
                // so the declarations on the lines after it stay visible.
                chars.next();
                for c in chars.by_ref() {
                    if c == '\n' {
                        unblocked.push('\n');
                        break;
                    }
                }
            }
            None => unblocked.push(c),
        }
    }
    unblocked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_starting_with_a_dereference_is_code() {
        let c = "/*\n * Doubles it.\n *\n */\nvoid twice(int *p) {\n  *p *= 2;\n}\n";
        assert_eq!(loc(c, crate::lang::Lang::C), 3);
    }

    #[test]
    fn powershell_block_comment_help_is_not_code() {
        let ps = "function Add-One {\n    <#\n    .SYNOPSIS\n    Adds one.\n    .DESCRIPTION\n    Adds one to the number it is given.\n    #>\n    param([int]$n)\n    $n + 1\n}\nExport-ModuleMember -Function Add-One\n";
        assert_eq!(loc(ps, crate::lang::Lang::Powershell), 5);
        assert_eq!(
            loc("<# Adds one. #>\n$n + 1\n", crate::lang::Lang::Powershell),
            1
        );
    }

    #[test]
    fn powershell_delimiters_after_a_comment_or_in_a_string_open_no_block() {
        // A `#` comment runs to the line's end: a `<#` in it, full-line or trailing,
        // opens nothing, while code before it still counts.
        assert_eq!(loc("# note <#\n$n + 1\n", crate::lang::Lang::Powershell), 1);
        assert_eq!(
            loc(
                "$n + 1 # trailing <#\n$m + 1\n",
                crate::lang::Lang::Powershell
            ),
            2
        );
        // A delimiter before the `#` still opens and closes its block.
        assert_eq!(
            loc(
                "$n + 1 <# note #> # trailing\n$m + 1\n",
                crate::lang::Lang::Powershell
            ),
            2
        );
        // Delimiters inside single- and double-quoted strings are text.
        assert_eq!(
            loc("$s = \"<#\"\n$n + 1\n", crate::lang::Lang::Powershell),
            2
        );
        assert_eq!(
            loc("$s = '<# #>'\n$n + 1\n", crate::lang::Lang::Powershell),
            2
        );
        assert_eq!(
            loc("$s = \"a `<# b\"\n$n + 1\n", crate::lang::Lang::Powershell),
            2
        );
        // Glued to a word, `<#` and `#` are literal text, not comment openers.
        assert_eq!(loc("foo<#bar\n$n + 1\n", crate::lang::Lang::Powershell), 2);
        assert_eq!(
            loc("$s = foo#bar\n$n + 1\n", crate::lang::Lang::Powershell),
            2
        );
        // After an expression terminator they still open a comment, even with no
        // whitespace in between, so a `<#` in that comment opens no block either.
        assert_eq!(
            loc("$n + 1;# note <#\n$m + 1\n", crate::lang::Lang::Powershell),
            2
        );
        assert_eq!(
            loc("($n);<# note #>\n$m + 1\n", crate::lang::Lang::Powershell),
            2
        );
        // An escaped quote out of any string opens none, so the block comment on
        // the lines after it is a comment still, not string content.
        assert_eq!(
            loc(
                "$s = `\"hi`\"\n<#\nhelp\n#>\n$n + 1\n",
                crate::lang::Lang::Powershell
            ),
            2
        );
    }

    #[test]
    fn powershell_stop_parsing_tail_is_literal_and_leaves_no_state() {
        // `--%` makes the rest of the line literal: its quotes, `#` comments and
        // `<#` markers are text, so they open no string or block that would change
        // how the following lines are parsed.
        for tail in [
            "\"unclosed",
            "<# not a block",
            "# a comment <#",
            "\" <# #> `",
        ] {
            let line = format!("git --% {tail}");
            let mut in_block = false;
            let mut quote: Option<char> = None;
            let out = powershell_outside(&line, &mut in_block, &mut quote);
            assert_eq!(out, line, "the literal tail is kept: {out:?}");
            assert!(!in_block, "`{tail}` opened a block");
            assert!(quote.is_none(), "`{tail}` opened a string");
        }
        // Delimiters before the token are still parsed; only its tail is literal.
        let mut in_block = false;
        let mut quote: Option<char> = None;
        let out = powershell_outside(
            "$s = \"ok\" ; git --% \"unclosed",
            &mut in_block,
            &mut quote,
        );
        assert!(out.contains("git --% \"unclosed"), "{out}");
        assert!(quote.is_none(), "the tail opened a string");
        assert!(!in_block, "the tail opened a block");
        // Glued to a word, `--%` is not the token, so a following quote still opens.
        let mut in_block = false;
        let mut quote: Option<char> = None;
        powershell_outside("foo--% \"unclosed", &mut in_block, &mut quote);
        assert_eq!(quote, Some('"'));
        // In LOC counting, a `<#` in the literal tail must not hide the next line.
        assert_eq!(
            loc(
                "git --% <# not a block\n$n + 1\n",
                crate::lang::Lang::Powershell
            ),
            2
        );
    }

    #[test]
    fn a_block_comment_marker_inside_a_line_comment_does_not_swallow_the_next_lines() {
        // `build/*.jar` in a `//` comment used to open a block comment, hiding both dependencies.
        let gradle = "// generated into build/*.jar\n\
                      implementation(\"a:b:1\")\n\
                      implementation(\"c:d:2\")\n";
        let out = without_slash_comments(gradle);
        assert!(out.contains("implementation(\"a:b:1\")"), "{out}");
        assert!(out.contains("implementation(\"c:d:2\")"), "{out}");
    }

    #[test]
    fn slash_comments_are_removed_and_block_comments_still_are() {
        let text = "// implementation(\"old:one:1\")\n\
                    implementation(\"a:b:1\") // trailing note\n\
                    /* implementation(\"gone:two:2\") */\n\
                    implementation(\"c:d:2\")";
        let out = without_slash_comments(text);
        assert!(!out.contains("old:one"), "{out}");
        assert!(!out.contains("trailing note"), "{out}");
        assert!(!out.contains("gone:two"), "{out}");
        assert!(out.contains("a:b:1") && out.contains("c:d:2"), "{out}");
    }

    #[test]
    fn a_slash_pair_inside_a_string_stays() {
        let text = "implementation(\"a:b:1\")\nval url = \"https://example.test/x\"\n";
        let out = without_slash_comments(text);
        assert!(out.contains("https://example.test/x"), "{out}");
        assert!(out.contains("a:b:1"), "{out}");
    }
}

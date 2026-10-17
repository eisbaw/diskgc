//! Test helpers that read docs/reference.md, so tests can check the code
//! against the doc's tables and examples (the doc is the contract; a test
//! keeps the two equal). The flake's workspace-tests source includes the
//! doc for this.

const REFERENCE_MD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reference.md");

pub(crate) fn reference_md() -> String {
    std::fs::read_to_string(REFERENCE_MD).unwrap_or_else(|e| {
        panic!("{REFERENCE_MD}: {e} (the flake's workspace-tests source must include it)")
    })
}

/// The section of `doc` under the heading line `heading`, up to the next
/// heading outside a code block.
pub(crate) fn section(doc: &str, heading: &str) -> String {
    let mut lines = doc.lines().skip_while(|line| *line != heading);
    let mut out = vec![lines.next().unwrap_or_else(|| panic!("no {heading:?}"))];
    let mut fenced = false;
    for line in lines {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            break;
        }
        out.push(line);
    }
    out.join("\n")
}

/// The cells of a markdown table row, `\|` unescaped.
pub(crate) fn cells(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_owned()),
            _ => cell.push(c),
        }
    }
    // Leading `|` makes an empty first cell; the trailing one ends the last.
    cells.remove(0);
    cells
}

/// The text inside a cell that is one code span.
pub(crate) fn code(cell: &str) -> &str {
    cell.strip_prefix('`')
        .and_then(|c| c.strip_suffix('`'))
        .unwrap_or_else(|| panic!("not one code span: {cell}"))
}

/// The rows of the section's table whose header starts with `header`.
pub(crate) fn table(section: &str, header: &str) -> Vec<Vec<String>> {
    let mut lines = section.lines().skip_while(|l| !l.starts_with(header));
    assert!(lines.next().is_some(), "no table headed {header:?}");
    lines.next(); // the |---| line
    let rows: Vec<Vec<String>> = lines
        .take_while(|l| l.starts_with('|'))
        .map(cells)
        .collect();
    assert!(!rows.is_empty(), "table {header:?} is empty");
    rows
}

/// The code blocks of `section` fenced as `lang`, in order.
pub(crate) fn code_blocks(section: &str, lang: &str) -> Vec<String> {
    let opening = format!("```{lang}");
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in section.lines() {
        match &mut current {
            None if line == opening => current = Some(Vec::new()),
            None => {}
            Some(_) if line.starts_with("```") => {
                blocks.push(current.take().unwrap_or_default().join("\n"));
            }
            Some(lines) => lines.push(line),
        }
    }
    blocks
}

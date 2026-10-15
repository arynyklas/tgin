//! A small block-style YAML writer for the migrated config: maps, lists,
//! scalars and comments, in the order they were built.

/// A YAML value.
#[derive(Clone, Debug)]
pub(super) enum Node {
    /// A scalar, already YAML: a plain word or number, or [`quoted`] text.
    Scalar(String),
    Map(Vec<Entry>),
    Seq(Vec<Node>),
}

/// One line of a map: a key and its value, or a comment.
#[derive(Clone, Debug)]
pub(super) enum Entry {
    Field(String, Node),
    Comment(String),
}

impl Node {
    /// A plain scalar: a number, a keyword or a name that needs no quotes.
    pub(super) fn plain(value: impl ToString) -> Self {
        Self::Scalar(value.to_string())
    }

    /// A string, double-quoted.
    pub(super) fn string(value: &str) -> Self {
        Self::Scalar(quoted(value))
    }

    pub(super) fn map() -> Self {
        Self::Map(Vec::new())
    }

    /// Adds `key: value` to a map; other nodes are returned unchanged.
    pub(super) fn field(mut self, key: impl Into<String>, value: Node) -> Self {
        if let Self::Map(entries) = &mut self {
            entries.push(Entry::Field(key.into(), value));
        }
        self
    }

    /// Adds a comment line to a map; other nodes are returned unchanged.
    pub(super) fn comment(mut self, text: impl Into<String>) -> Self {
        if let Self::Map(entries) = &mut self {
            entries.push(Entry::Comment(text.into()));
        }
        self
    }

    fn is_empty_collection(&self) -> Option<&'static str> {
        match self {
            Self::Map(entries) if !entries.iter().any(|e| matches!(e, Entry::Field(..))) => {
                Some("{}")
            }
            Self::Seq(items) if items.is_empty() => Some("[]"),
            _ => None,
        }
    }
}

/// `root`, a map, as a YAML document.
pub(super) fn render(root: &Node) -> String {
    let mut lines = Vec::new();
    match root {
        Node::Map(entries) => map_lines(entries, 0, &mut lines),
        other => node_lines(other, 0, &mut lines),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// A value that is not a map entry, starting at `indent`.
fn node_lines(node: &Node, indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    if let Some(empty) = node.is_empty_collection() {
        out.push(format!("{pad}{empty}"));
        return;
    }
    match node {
        Node::Scalar(s) => out.push(format!("{pad}{s}")),
        Node::Map(entries) => map_lines(entries, indent, out),
        Node::Seq(items) => seq_lines(items, indent, out),
    }
}

fn map_lines(entries: &[Entry], indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    for entry in entries {
        match entry {
            Entry::Comment(text) => out.push(format!("{pad}# {text}")),
            Entry::Field(key, value) => {
                if let Some(empty) = value.is_empty_collection() {
                    out.push(format!("{pad}{key}: {empty}"));
                    continue;
                }
                match value {
                    Node::Scalar(s) => out.push(format!("{pad}{key}: {s}")),
                    Node::Map(entries) => {
                        out.push(format!("{pad}{key}:"));
                        map_lines(entries, indent + 2, out);
                    }
                    Node::Seq(items) => {
                        out.push(format!("{pad}{key}:"));
                        seq_lines(items, indent + 2, out);
                    }
                }
            }
        }
    }
}

fn seq_lines(items: &[Node], indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    for item in items {
        if let Node::Scalar(s) = item {
            out.push(format!("{pad}- {s}"));
            continue;
        }
        // The item's first line goes on the dash line.
        let first = out.len();
        node_lines(item, indent + 2, out);
        if let Some(line) = out.get_mut(first) {
            let rest = line.get(indent + 2..).unwrap_or_default().to_owned();
            *line = format!("{pad}- {rest}");
        }
    }
}

/// `value` as a YAML double-quoted scalar. Quotes, backslashes and every
/// character YAML may not hold literally (controls, line and paragraph
/// separators, byte-order marks) are escaped.
pub(super) fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control()
                || matches!(
                    c,
                    '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
                ) =>
            {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string survives a YAML round trip unchanged.
    #[test]
    fn quoted_strings_round_trip() {
        for value in [
            "",
            "plain",
            "with \"quotes\" and \\ backslashes",
            "line\nbreak\r\ttab",
            "controls \u{0}\u{7}\u{1b}\u{7f}\u{85}\u{9f}",
            "separators \u{2028}\u{2029} bom \u{feff}",
            "# not a comment: {flow} [list] &anchor *alias !tag | > % @ `",
            "${env:X} $${literal}",
            "юникод ✓ 𝄞",
            "yes",
            "null",
            "123",
        ] {
            let yaml = format!("value: {}\n", quoted(value));
            let parsed: std::collections::BTreeMap<String, String> =
                serde_saphyr::from_str(&yaml).unwrap_or_else(|err| panic!("{yaml}: {err}"));
            assert_eq!(parsed["value"], value, "{yaml}");
        }
    }
}

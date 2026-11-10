//! YAML parser plugin - full-parse mode.
//!
//! Handles `.yaml` and `.yml` files.
//! Parses source with tree-sitter-yaml directly.
//!
//! Block-sequence identity heuristic: when a block sequence item contains a
//! mapping that has a "name", "id", or "key" key, that value is used as the
//! node label instead of the positional index.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct YamlParser;

const TRIVIA: &[&str] = &["comment"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "stream",
    "document",
    // Block structure
    "block_mapping",
    "block_mapping_pair",
    "block_sequence",
    "block_sequence_item",
    "block_node",
    // Flow structure
    "flow_mapping",
    "flow_pair",
    "flow_sequence",
    "flow_sequence_item",
    "flow_node",
    // Scalars
    "plain_scalar",
    "double_quote_scalar",
    "single_quote_scalar",
    "block_scalar",
    "boolean_scalar",
    "null_scalar",
    "integer_scalar",
    "float_scalar",
    // Anchors / aliases
    "alias",
    "anchor",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

/// Get the text of a scalar-like node (leaf) or its first leaf child.
fn scalar_text(node: &CstNode) -> &str {
    if node.is_leaf() {
        return node.text_or_empty();
    }
    for child in &node.children {
        if child.is_leaf() {
            return child.text_or_empty();
        }
    }
    ""
}

/// Try to find an identity label from a block_mapping inside a block_sequence_item.
/// Looks for a block_mapping_pair whose key is "name", "id", or "key".
fn identity_label_from_mapping(node: &CstNode) -> Option<String> {
    const ID_KEYS: &[&str] = &["name", "id", "key"];
    // Descend into block_node → block_mapping
    let mapping = find_first_child_of_type(node, "block_mapping")
        .or_else(|| find_first_child_of_type(node, "flow_mapping"))?;
    for pair in &mapping.children {
        if pair.node_type == "block_mapping_pair" || pair.node_type == "flow_pair" {
            if let Some(key_text) = pair_key_text(pair) {
                if ID_KEYS.contains(&key_text.as_str()) {
                    if let Some(val) = pair_value_text(pair) {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

fn find_first_child_of_type<'a>(node: &'a CstNode, node_type: &str) -> Option<&'a CstNode> {
    for child in &node.children {
        if child.node_type == node_type {
            return Some(child);
        }
        if let Some(found) = find_first_child_of_type(child, node_type) {
            return Some(found);
        }
    }
    None
}

/// Extract the plain-text key from a block_mapping_pair or flow_pair.
fn pair_key_text(pair: &CstNode) -> Option<String> {
    // tree-sitter-yaml: block_mapping_pair has a "key" child (named or just first child)
    for child in &pair.children {
        if child.node_type == "key"
            || child.node_type == "plain_scalar"
            || child.node_type == "double_quote_scalar"
            || child.node_type == "single_quote_scalar"
        {
            let t = scalar_text(child);
            if !t.is_empty() {
                return Some(strip_quotes(t).to_string());
            }
        }
    }
    // Fallback: first child
    pair.children.first().map(|c| scalar_text(c).to_string())
}

/// Extract the plain-text value from a block_mapping_pair or flow_pair.
fn pair_value_text(pair: &CstNode) -> Option<String> {
    // In tree-sitter-yaml, block_mapping_pair children: key, ":", value
    // The value may be the 2nd or 3rd named child depending on grammar version.
    let mut found_key = false;
    for child in &pair.children {
        if !found_key {
            found_key = true;
            continue;
        }
        let t = scalar_text(child);
        if !t.is_empty() && t != ":" {
            return Some(strip_quotes(t).to_string());
        }
    }
    None
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return strip_quotes(node.text_or_empty()).to_string();
    }
    match node.node_type.as_str() {
        "block_mapping_pair" | "flow_pair" => {
            if let Some(key) = pair_key_text(node) {
                return key;
            }
        }
        "block_sequence_item" => {
            if let Some(identity) = identity_label_from_mapping(node) {
                return identity;
            }
        }
        "document" => {
            return "document".to_string();
        }
        _ => {}
    }
    // Default: first scalar child text
    let t = scalar_text(node);
    if !t.is_empty() {
        return strip_quotes(t).to_string();
    }
    node.node_type.clone()
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    seq_index: Option<usize>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    if TRIVIA.contains(&node.node_type.as_str()) {
        return None;
    }

    if !is_semantic(&node.node_type) {
        return None;
    }

    let children: Vec<SemanticNode> = node
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let idx = if node.node_type == "block_sequence" || node.node_type == "flow_sequence" {
                Some(i)
            } else {
                None
            };
            convert(c, &format!("{}.{}", id_prefix, i), idx, memo)
        })
        .collect();

    let hash = structural_hash_with_memo(node, memo);

    let label = if node.node_type == "block_sequence_item" {
        // Try identity heuristic; fall back to positional index
        identity_label_from_mapping(node).unwrap_or_else(|| {
            seq_index
                .map(|i| format!("[{}]", i))
                .unwrap_or_else(|| label_for(node))
        })
    } else {
        label_for(node)
    };

    let builder = SemanticNodeBuilder::new(
        id_prefix,
        &node.node_type,
        label,
        node.start_line,
        node.start_col,
        node.end_line,
        node.end_col,
        hash,
    )
    .children(children);

    Some(builder.build())
}

/// Convert a tree-sitter node into the SDK CST, dropping trivia (comments).
///
/// Comments are insignificant in YAML, so they must not reach the CST at all:
/// a retained comment leaf would otherwise become the first-leaf label of its
/// container (e.g. a leading `# ...` relabelling the `stream` root) and would
/// feed the structural hash, turning a comment-only edit into a semantic
/// change. A node whose only named children were comments is an empty
/// container, not a leaf, so its (comment-bearing) source text is not kept.
fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let named: Vec<tree_sitter::Node<'_>> = (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .collect();
    let had_named_children = !named.is_empty();
    let children: Vec<CstNode> = named
        .into_iter()
        .filter(|child| !TRIVIA.contains(&child.kind()))
        .map(|child| node_to_cst(child, source))
        .collect();

    let text = if !had_named_children {
        Some(node.utf8_text(source).unwrap_or("").to_owned())
    } else {
        None
    };

    CstNode {
        node_type: node.kind().to_string(),
        named: node.is_named(),
        text,
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children,
    }
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_yaml::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load YAML grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for YamlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "yaml".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".yaml") || lower.ends_with(".yml") {
            return "yaml".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "name: my-app\nversion: \"1.0\"\nserver:\n  host: localhost\n  port: 8080\n".to_string(),
            new: "name: my-app\nversion: \"2.0\"\nserver:\n  host: 0.0.0.0\n  port: 8080\n  timeout: 30s\ndatabase:\n  host: db.example.com\n  port: 5432\n  name: mydb\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["yaml".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(YamlParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!YamlParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = YamlParser::grammar_id();
        let ids = YamlParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_yaml() {
        assert_eq!(
            YamlParser::detect_language("config.yaml".to_string(), "".to_string()),
            "yaml"
        );
    }

    #[test]
    fn detect_language_yml() {
        assert_eq!(
            YamlParser::detect_language("pipeline.yml".to_string(), "".to_string()),
            "yaml"
        );
    }

    #[test]
    fn detect_language_unknown() {
        let r = YamlParser::detect_language("main.py".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn strip_quotes_double() {
        assert_eq!(strip_quotes(r#""hello""#), "hello");
    }

    #[test]
    fn strip_quotes_single() {
        assert_eq!(strip_quotes("'world'"), "world");
    }

    #[test]
    fn strip_quotes_unquoted() {
        assert_eq!(strip_quotes("plain"), "plain");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    fn root_hash(source: &str) -> String {
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        tree["structural_hash"].as_str().unwrap().to_string()
    }

    #[test]
    fn long_scalar_suffix_changes_root_hash() {
        for length in [4095, 4096, 4097, 4200, 16384] {
            for prefix in ["a".repeat(length), "é🦀".repeat(length)] {
                for quote in ["", "\"", "'"] {
                    let old = format!("value: {quote}{prefix}x{quote}\n");
                    let new = format!("value: {quote}{prefix}y{quote}\n");
                    assert_ne!(root_hash(&old), root_hash(&new), "prefix length {length}");
                    assert_eq!(process_impl(&old), process_impl(&old));
                }
            }
        }
    }

    #[test]
    fn long_scalar_leaf_keeps_complete_utf8_source() {
        let payload = format!("{}suffix", "é🦀".repeat(4200));
        for quote in ["", "\"", "'"] {
            let source = format!("value: {quote}{payload}{quote}\n");
            let root = parse_source(&source).unwrap();
            fn find<'a>(node: &'a CstNode, payload: &str) -> Option<&'a CstNode> {
                if node.text.as_deref().is_some_and(|text| text.contains(payload)) {
                    return Some(node);
                }
                node.children.iter().find_map(|child| find(child, payload))
            }
            let leaf = find(&root, &payload).expect("complete scalar leaf");
            let text = leaf.text_or_empty();
            assert_eq!(leaf.end_col - leaf.start_col, text.len() as u32);
            assert_eq!(&source[leaf.start_col as usize..leaf.end_col as usize], text);
        }
    }

    #[test]
    fn unchanged_long_scalar_keeps_hash_across_spacing() {
        let payload = "é🦀".repeat(4200);
        let compact = format!("value: \"{payload}\"\n");
        let spaced = format!("value:   \"{payload}\"\n\n");
        assert_eq!(root_hash(&compact), root_hash(&spaced));
    }

    /// Project a tree onto its position-independent semantic content
    /// (type, label, structural hash, children).
    fn semantic_projection(value: &serde_json::Value) -> serde_json::Value {
        let children = value["children"]
            .as_array()
            .map(|c| c.iter().map(semantic_projection).collect::<Vec<_>>())
            .unwrap_or_default();
        serde_json::json!({
            "type": value["node_type"],
            "label": value["label"],
            "hash": value["structural_hash"],
            "children": children,
        })
    }

    fn projection(source: &str) -> serde_json::Value {
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        semantic_projection(&tree)
    }

    fn labels(value: &serde_json::Value, out: &mut Vec<String>) {
        out.push(value["label"].as_str().unwrap_or_default().to_string());
        for child in value["children"].as_array().into_iter().flatten() {
            labels(child, out);
        }
    }

    // buchochelliq-labs/intentumdiff-yaml-parser#5: data_config.yaml.safe_preservation
    // and data_config.yaml.external_safe_preservation (exact assessment sources).
    #[test]
    fn leading_comment_does_not_change_semantic_tree() {
        let pairs = [
            (
                "name: my-app\nversion: \"2.0\"\nserver:\n  host: 0.0.0.0\n  port: 8080\n  timeout: 30s\ndatabase:\n  host: db.example.com\n  port: 5432\n  name: mydb\n",
                "# ordinary assessment comment\nname: my-app\nversion: \"2.0\"\nserver:\n  host: 0.0.0.0\n  port: 8080\n  timeout: 30s\ndatabase:\n  host: db.example.com\n  port: 5432\n  name: mydb\n",
            ),
            (
                "- foo: bar\n- - baz\n  - baz\n",
                "# ordinary source-derived control\n- foo: bar\n- - baz\n  - baz\n",
            ),
        ];
        for (old, new) in pairs {
            let (old_tree, new_tree) = (projection(old), projection(new));
            assert_eq!(old_tree, new_tree, "comment changed semantic tree");
            assert_eq!(new_tree["label"], "stream");
            assert_eq!(root_hash(old), root_hash(new));
        }
    }

    #[test]
    fn comments_never_become_labels_or_hash_input() {
        let base = "a: 1\nb:\n  - x\n  - y\nc: {d: 2}\n";
        let commented = "# lead\na: 1 # trailing\n# between\nb:\n  # in seq\n  - x\n  - y # t\nc: {d: 2} # flow\n# end\n";
        assert_eq!(projection(base), projection(commented));
        let mut all = Vec::new();
        labels(&projection(commented), &mut all);
        assert!(all.iter().all(|l| !l.contains('#')), "{all:?}");
    }

    #[test]
    fn hash_inside_scalars_is_preserved() {
        // `#` inside quoted / non-comment positions is content, not a comment.
        for (old, new) in [
            ("v: \"a # b\"\n", "v: \"a # c\"\n"),
            ("v: 'a # b'\n", "v: 'a # c'\n"),
            ("v: a#b\n", "v: a#c\n"),
            ("v: |\n  # b\n", "v: |\n  # c\n"),
        ] {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} vs {new:?}");
        }
    }

    #[test]
    fn comment_only_document_matches_empty_document() {
        assert_eq!(projection(""), projection("# only a comment\n"));
    }
}

//! `ccc-tree-sitter` — Rust syntax extraction (TIS §7.2, ADR-003).
//!
//! Produces symbols and name-level references from the syntax tree. Syntax
//! parsing is NOT semantic resolution: every reference emitted here is a
//! name-level candidate unless a uniqueness argument can be made later
//! (TIS §7.2 — conservative default).

use ccc_domain::{blake3_hex, RelationshipType, SymbolKind};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

/// Version of the extraction logic (part of artifact cache keys, TIS §4.3).
pub const PARSER_VERSION: &str = "ccc-tree-sitter/0.1";
/// Grammar identity (part of artifact cache keys).
pub const GRAMMAR_VERSION: &str = concat!("tree-sitter-rust/", env!("CARGO_PKG_VERSION"));
/// Hash of the extraction configuration; no knobs exist in step 1.
pub const EXTRACTION_CONFIG_HASH: &str = "extraction-v1";

/// One symbol definition extracted from syntax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    /// Simple name as written in source.
    pub name: String,
    /// Module-path-qualified name, e.g. `crate::util::read_config`.
    pub qualified_name: String,
    pub kind: SymbolKind,
    /// 1-based line numbers, 0-based columns.
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// BLAKE3 over the item's declaration header (text before the body).
    pub signature_hash: String,
}

/// One name-level reference (call, import, or trait implementation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedReference {
    pub relationship: RelationshipType,
    /// Target as written in source (e.g. `read_config` or `std::io::Write`).
    pub target_name: String,
    /// Qualified name of the enclosing symbol, when inside one.
    pub from_qualified: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Extraction result for one Rust source file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractedFile {
    pub symbols: Vec<ExtractedSymbol>,
    pub references: Vec<ExtractedReference>,
    /// `true` when the syntax tree contains ERROR nodes; extraction still
    /// returns whatever was recoverable (TIS §7.2 step 5).
    pub parse_error: bool,
}

/// Rust extractor. One instance per thread; reuse it to amortize setup.
pub struct RustExtractor {
    parser: Parser,
}

impl Default for RustExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl RustExtractor {
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("tree-sitter-rust grammar loads");
        Self { parser }
    }

    /// Parses one Rust source file and extracts symbols + name-level references.
    pub fn extract(&mut self, source: &[u8]) -> ExtractedFile {
        let mut out = ExtractedFile::default();
        let Some(tree) = self.parser.parse(source, None) else {
            // Without a tree there is nothing to extract; report the failure
            // conservatively rather than pretending the file parsed.
            out.parse_error = true;
            return out;
        };
        out.parse_error = tree.root_node().has_error();
        walk(source, tree.root_node(), "crate", None, &mut out);
        out
    }
}

fn text<'a>(source: &'a [u8], node: Node<'a>) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Recursive walk tracking the module path and enclosing symbol.
fn walk(
    source: &[u8],
    node: Node,
    module_path: &str,
    enclosing: Option<String>,
    out: &mut ExtractedFile,
) {
    let kind = node.kind();

    match kind {
        "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
        | "const_item" | "static_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(source, name_node).to_string();
                // Items nested in a trait or impl body qualify under the
                // enclosing item (methods); everything else uses the module path.
                let qualified = match &enclosing {
                    Some(e) => format!("{e}::{name}"),
                    None => format!("{module_path}::{name}"),
                };
                out.symbols.push(ExtractedSymbol {
                    name,
                    qualified_name: qualified.clone(),
                    kind: symbol_kind(kind),
                    start_line: node.start_position().row as u32 + 1,
                    start_column: node.start_position().column as u32,
                    end_line: node.end_position().row as u32 + 1,
                    end_column: node.end_position().column as u32,
                    signature_hash: header_hash(source, node),
                });
                // Walk the body with the new enclosing symbol (trait items
                // become Trait::method names).
                for child in node.children(&mut node.walk()) {
                    walk(source, child, module_path, Some(qualified.clone()), out);
                }
                return;
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(source, name_node).to_string();
                let qualified = format!("{module_path}::{name}");
                if let Some(body) = node.child_by_field_name("body") {
                    out.symbols.push(ExtractedSymbol {
                        name,
                        qualified_name: qualified.clone(),
                        kind: SymbolKind::Module,
                        start_line: node.start_position().row as u32 + 1,
                        start_column: node.start_position().column as u32,
                        end_line: node.end_position().row as u32 + 1,
                        end_column: node.end_position().column as u32,
                        signature_hash: header_hash(source, node),
                    });
                    walk(source, body, &qualified, enclosing, out);
                }
                return;
            }
        }
        "impl_item" => {
            let type_text =
                text(source, node.child_by_field_name("type").unwrap_or(node)).to_string();
            let trait_text = node
                .child_by_field_name("trait")
                .map(|t| text(source, t).to_string());
            let impl_qualified = match &trait_text {
                Some(tr) => format!("{module_path}::<impl {tr} for {type_text}>"),
                None => format!("{module_path}::<impl {type_text}>"),
            };
            out.symbols.push(ExtractedSymbol {
                name: type_text,
                qualified_name: impl_qualified.clone(),
                kind: SymbolKind::Impl,
                start_line: node.start_position().row as u32 + 1,
                start_column: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_column: node.end_position().column as u32,
                signature_hash: header_hash(source, node),
            });
            // Trait implementations are candidate relationships by
            // construction (syntax cannot confirm trait resolution).
            if let Some(trait_node) = trait_text.and_then(|_| node.child_by_field_name("trait")) {
                out.references.push(ExtractedReference {
                    relationship: RelationshipType::Implements,
                    target_name: text(source, trait_node).to_string(),
                    from_qualified: Some(impl_qualified.clone()),
                    start_line: trait_node.start_position().row as u32 + 1,
                    start_column: trait_node.start_position().column as u32,
                    end_line: trait_node.end_position().row as u32 + 1,
                    end_column: trait_node.end_position().column as u32,
                });
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk(source, body, module_path, Some(impl_qualified), out);
            }
            return;
        }
        "use_declaration" => {
            record_use(source, node, out);
            return;
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                let target = call_target_text(source, func);
                if !target.is_empty() {
                    out.references.push(ExtractedReference {
                        relationship: RelationshipType::Calls,
                        target_name: target,
                        from_qualified: enclosing.clone(),
                        start_line: func.start_position().row as u32 + 1,
                        start_column: func.start_position().column as u32,
                        end_line: func.end_position().row as u32 + 1,
                        end_column: func.end_position().column as u32,
                    });
                }
            }
        }
        "macro_invocation" => {
            if let Some(macro_node) = node.child_by_field_name("macro") {
                out.references.push(ExtractedReference {
                    relationship: RelationshipType::References,
                    target_name: text(source, macro_node).to_string(),
                    from_qualified: enclosing.clone(),
                    start_line: macro_node.start_position().row as u32 + 1,
                    start_column: macro_node.start_position().column as u32,
                    end_line: macro_node.end_position().row as u32 + 1,
                    end_column: macro_node.end_position().column as u32,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(source, child, module_path, enclosing.clone(), out);
    }
}

/// Records import edges for a use declaration, expanding list forms.
fn record_use(source: &[u8], node: Node, out: &mut ExtractedFile) {
    let Some(arg) = node.child_by_field_name("argument") else {
        return;
    };
    let mut stack = vec![arg];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "use_list" => {
                let mut cursor = n.walk();
                for child in n.children(&mut cursor) {
                    if child.kind() == "," {
                        continue;
                    }
                    stack.push(child);
                }
            }
            "scoped_use_list" => {
                let prefix = n
                    .child_by_field_name("path")
                    .map(|p| text(source, p).to_string())
                    .unwrap_or_default();
                if let Some(list) = n.child_by_field_name("list") {
                    let mut cursor = list.walk();
                    for child in list.children(&mut cursor) {
                        if child.kind() == "," {
                            continue;
                        }
                        let target = match child.kind() {
                            "identifier" => {
                                format!("{prefix}::{}", text(source, child))
                            }
                            "use_as_clause" => {
                                let inner = child
                                    .child(0)
                                    .map(|c| text(source, c).to_string())
                                    .unwrap_or_default();
                                format!("{prefix}::{inner}")
                            }
                            "scoped_identifier" | "scoped_use_list" | "use_list" => {
                                // Nested lists: record the prefix path itself.
                                format!(
                                    "{prefix}::{{{}}}",
                                    text(source, child).chars().take(80).collect::<String>()
                                )
                            }
                            _ => format!("{prefix}::{}", text(source, child)),
                        };
                        push_import(out, target, node);
                    }
                }
            }
            "use_as_clause" => {
                let inner = n
                    .child(0)
                    .map(|c| text(source, c).to_string())
                    .unwrap_or_default();
                push_import(out, inner, node);
            }
            _ => {
                let target = text(source, n).to_string();
                push_import(out, target, node);
            }
        }
    }
}

fn push_import(out: &mut ExtractedFile, target: String, node: Node) {
    let target = target.trim().to_string();
    if target.is_empty() {
        return;
    }
    out.references.push(ExtractedReference {
        relationship: RelationshipType::Imports,
        target_name: target,
        from_qualified: None,
        start_line: node.start_position().row as u32 + 1,
        start_column: node.start_position().column as u32,
        end_line: node.end_position().row as u32 + 1,
        end_column: node.end_position().column as u32,
    });
}

fn call_target_text<'a>(source: &'a [u8], func: Node<'a>) -> String {
    match func.kind() {
        "identifier" | "scoped_identifier" | "field_expression" => text(source, func)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn symbol_kind(kind: &str) -> SymbolKind {
    match kind {
        "function_item" => SymbolKind::Function,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "type_item" => SymbolKind::TypeAlias,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "mod_item" => SymbolKind::Module,
        "impl_item" => SymbolKind::Impl,
        _ => SymbolKind::Other,
    }
}

/// Hashes the declaration header: everything before the first `{` (or the
/// whole text for items without bodies). Deterministic across calls.
fn header_hash(source: &[u8], node: Node) -> String {
    let full = text(source, node);
    let header = match full.find('{') {
        Some(idx) => &full[..idx],
        None => full,
    };
    blake3_hex(header.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> ExtractedFile {
        RustExtractor::new().extract(src.as_bytes())
    }

    #[test]
    fn extracts_top_level_and_nested_symbols() {
        let src = r#"
pub mod util {
    pub struct Config { pub name: String }
    pub fn read_config(name: &str) -> Config { Config { name: name.to_string() } }
}
pub trait Writer { fn write(&self); }
impl Writer for util::Config { fn write(&self) {} }
pub enum Mode { Fast, Slow }
"#;
        let out = extract(src);
        let qualified: Vec<&str> = out
            .symbols
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();
        assert!(qualified.contains(&"crate::util"), "{qualified:?}");
        assert!(qualified.contains(&"crate::util::Config"), "{qualified:?}");
        assert!(
            qualified.contains(&"crate::util::read_config"),
            "{qualified:?}"
        );
        assert!(qualified.contains(&"crate::Writer"), "{qualified:?}");
        assert!(qualified.contains(&"crate::Mode"), "{qualified:?}");
        let writer_impl = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Impl)
            .expect("impl symbol");
        assert!(writer_impl
            .qualified_name
            .contains("<impl Writer for util::Config"));
        assert!(!out.parse_error);
    }

    #[test]
    fn method_inside_trait_is_qualified() {
        let src = "pub trait Sink { fn drain(&mut self) {} }";
        let out = extract(src);
        assert!(out
            .symbols
            .iter()
            .any(|s| s.qualified_name == "crate::Sink::drain"));
    }

    #[test]
    fn calls_are_recorded_as_name_level_references() {
        let src = r#"
fn caller() { helper(1); std::mem::drop(2); }
"#;
        let out = extract(src);
        let calls: Vec<&ExtractedReference> = out
            .references
            .iter()
            .filter(|r| r.relationship == RelationshipType::Calls)
            .collect();
        let targets: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(targets.contains(&"helper"), "{targets:?}");
        assert!(targets.contains(&"std::mem::drop"), "{targets:?}");
        assert_eq!(calls[0].from_qualified.as_deref(), Some("crate::caller"));
    }

    #[test]
    fn use_declarations_expand_to_import_references() {
        let src = "use std::io;\nuse std::collections::{HashMap, HashSet};\nuse crate::util::Config as Cfg;";
        let out = extract(src);
        let imports: Vec<&str> = out
            .references
            .iter()
            .filter(|r| r.relationship == RelationshipType::Imports)
            .map(|r| r.target_name.as_str())
            .collect();
        assert!(imports.contains(&"std::io"), "{imports:?}");
        assert!(
            imports.contains(&"std::collections::HashMap"),
            "{imports:?}"
        );
        assert!(
            imports.contains(&"std::collections::HashSet"),
            "{imports:?}"
        );
        assert!(imports.contains(&"crate::util::Config"), "{imports:?}");
    }

    #[test]
    fn impl_trait_edges_are_implements_candidates() {
        let src = "impl Display for Foo {}";
        let out = extract(src);
        let implements = out
            .references
            .iter()
            .find(|r| r.relationship == RelationshipType::Implements)
            .expect("implements reference");
        assert_eq!(implements.target_name, "Display");
    }

    #[test]
    fn parse_error_is_reported_but_extraction_survives() {
        let src = "pub fn broken( {";
        let out = extract(src);
        assert!(out.parse_error);
    }

    #[test]
    fn signature_hash_is_stable_and_content_sensitive() {
        let a = "pub fn f(x: u8) -> u8 { x }";
        let b = "pub fn f(x: u8) -> u8 { x + 1 }";
        let c = "pub fn f(y: u8) -> u8 { y }";
        let ha = extract(a).symbols[0].signature_hash.clone();
        let hb = extract(b).symbols[0].signature_hash.clone();
        let hc = extract(c).symbols[0].signature_hash.clone();
        // Body change does not alter the header hash...
        assert_eq!(ha, hb);
        // ...but signature change does.
        assert_ne!(ha, hc);
    }
}

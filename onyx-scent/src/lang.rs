use tree_sitter::{Language, Node, Parser};

use crate::ontology::{EntityKind, ScentError, SupportedLang};

/// Intermediate symbol extracted from a single file before global ID assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEntity {
    pub short_name: String,
    pub owner_type: Option<String>,
    pub kind: EntityKind,
    pub signature: String,
    pub is_public: bool,
    pub start_line: usize,
    pub end_line: usize,
}

/// Intermediate call site discovered inside a callable body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCall {
    /// Index into `ParsedFile::entities`.
    pub caller_entity_idx: usize,
    pub callee_name: String,
    pub qualifier: Option<String>,
}

/// Cached per-file AST distillation payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub lang: SupportedLang,
    pub blake3_hash: [u8; 32],
    pub entities: Vec<ParsedEntity>,
    pub raw_calls: Vec<RawCall>,
}

fn language_grammar(lang: SupportedLang) -> Language {
    match lang {
        SupportedLang::Rust => tree_sitter_rust::language(),
        SupportedLang::Python => tree_sitter_python::language(),
        SupportedLang::Java => tree_sitter_java::language(),
    }
}

/// Parses `source` for `lang`, extracting symbols, compact signatures, line ranges, and call edges.
pub fn parse_source(
    path: &str,
    lang: SupportedLang,
    source: &str,
    blake3_hash: [u8; 32],
) -> Result<ParsedFile, ScentError> {
    let mut parser = Parser::new();
    parser
        .set_language(language_grammar(lang))
        .map_err(|e| ScentError::Parse {
            path: path.to_string(),
            message: format!("failed to configure tree-sitter grammar: {e}"),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ScentError::Parse {
            path: path.to_string(),
            message: "tree-sitter returned no syntax tree".to_string(),
        })?;

    let mut ctx = WalkCtx {
        source,
        lang,
        entities: Vec::new(),
        raw_calls: Vec::new(),
        type_stack: Vec::new(),
        callable_stack: Vec::new(),
    };
    ctx.visit(tree.root_node());

    Ok(ParsedFile {
        lang,
        blake3_hash,
        entities: ctx.entities,
        raw_calls: ctx.raw_calls,
    })
}

struct WalkCtx<'a> {
    source: &'a str,
    lang: SupportedLang,
    entities: Vec<ParsedEntity>,
    raw_calls: Vec<RawCall>,
    type_stack: Vec<String>,
    callable_stack: Vec<usize>,
}

impl<'a> WalkCtx<'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        &self.source[node.start_byte()..node.end_byte()]
    }

    fn visit(&mut self, node: Node<'_>) {
        match self.lang {
            SupportedLang::Rust => self.visit_rust(node),
            SupportedLang::Python => self.visit_python(node),
            SupportedLang::Java => self.visit_java(node),
        }
    }

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child);
        }
    }

    fn push_entity(
        &mut self,
        node: Node<'_>,
        short_name: String,
        owner_type: Option<String>,
        kind: EntityKind,
        signature: String,
        is_public: bool,
    ) -> usize {
        let idx = self.entities.len();
        self.entities.push(ParsedEntity {
            short_name,
            owner_type,
            kind,
            signature,
            is_public,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        idx
    }

    fn visit_rust(&mut self, node: Node<'_>) {
        match node.kind() {
            "struct_item" | "enum_item" | "trait_item" => {
                let kind = match node.kind() {
                    "struct_item" => EntityKind::Struct,
                    "enum_item" => EntityKind::Enum,
                    _ => EntityKind::Trait,
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let sig = extract_header_before(
                        self.source,
                        node.start_byte(),
                        node.end_byte(),
                        &['{', ';', '('],
                    );
                    let is_pub = has_rust_visibility(node);
                    self.push_entity(node, name.clone(), None, kind, sig, is_pub);
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "impl_item" => {
                if let Some(type_node) = node.child_by_field_name("type") {
                    let impl_name = clean_rust_type_name(self.text(type_node));
                    self.type_stack.push(impl_name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "function_item" | "function_signature_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let owner = self.type_stack.last().cloned();
                    let kind = if owner.is_some() {
                        EntityKind::Method
                    } else {
                        EntityKind::Function
                    };
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start])
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let is_pub = has_rust_visibility(node);
                    let idx = self.push_entity(node, name, owner, kind, sig, is_pub);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "call_expression" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        if let Some((callee_name, qualifier)) = self.extract_rust_callee(fn_node) {
                            self.raw_calls.push(RawCall {
                                caller_entity_idx: caller_idx,
                                callee_name,
                                qualifier,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn extract_rust_callee(&self, mut node: Node<'_>) -> Option<(String, Option<String>)> {
        if node.kind() == "generic_function" {
            node = node.child_by_field_name("function")?;
        }
        match node.kind() {
            "identifier" => Some((self.text(node).to_string(), None)),
            "field_expression" => {
                let field = node.child_by_field_name("field")?;
                Some((self.text(field).to_string(), None))
            }
            "scoped_identifier" => {
                let name = node.child_by_field_name("name")?;
                let qual = node
                    .child_by_field_name("path")
                    .map(|p| clean_rust_type_name(self.text(p)));
                Some((self.text(name).to_string(), qual))
            }
            _ => None,
        }
    }

    fn visit_python(&mut self, node: Node<'_>) {
        match node.kind() {
            "class_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start])
                        .trim_end_matches(':')
                        .trim()
                        .to_string();
                    let is_pub = !name.starts_with('_');
                    self.push_entity(node, name.clone(), None, EntityKind::Class, sig, is_pub);
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let owner = self.type_stack.last().cloned();
                    let kind = if owner.is_some() {
                        EntityKind::Method
                    } else {
                        EntityKind::Function
                    };
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start])
                        .trim_end_matches(':')
                        .trim()
                        .to_string();
                    let is_pub = !name.starts_with('_') || name == "__init__";
                    let idx = self.push_entity(node, name, owner, kind, sig, is_pub);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "call" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        match fn_node.kind() {
                            "identifier" => {
                                self.raw_calls.push(RawCall {
                                    caller_entity_idx: caller_idx,
                                    callee_name: self.text(fn_node).to_string(),
                                    qualifier: None,
                                });
                            }
                            "attribute" => {
                                if let Some(attr) = fn_node.child_by_field_name("attribute") {
                                    let qual = fn_node
                                        .child_by_field_name("object")
                                        .filter(|o| o.kind() == "identifier")
                                        .map(|o| self.text(o).to_string());
                                    self.raw_calls.push(RawCall {
                                        caller_entity_idx: caller_idx,
                                        callee_name: self.text(attr).to_string(),
                                        qualifier: qual,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn visit_java(&mut self, node: Node<'_>) {
        match node.kind() {
            "class_declaration" | "interface_declaration" => {
                let kind = if node.kind() == "interface_declaration" {
                    EntityKind::Interface
                } else {
                    EntityKind::Class
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = has_java_public(self.text(node));
                    self.push_entity(node, name.clone(), None, kind, sig, is_pub);
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let owner = self.type_stack.last().cloned();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start])
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let is_pub = has_java_public(self.text(node));
                    let idx = self.push_entity(node, name, owner, EntityKind::Method, sig, is_pub);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "method_invocation" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let qual = node
                            .child_by_field_name("object")
                            .filter(|o| o.kind() == "identifier")
                            .map(|o| self.text(o).to_string());
                        self.raw_calls.push(RawCall {
                            caller_entity_idx: caller_idx,
                            callee_name: self.text(name_node).to_string(),
                            qualifier: qual,
                        });
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }
}

fn has_rust_visibility(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return true;
        }
    }
    false
}

fn has_java_public(prefix_source: &str) -> bool {
    prefix_source
        .split_whitespace()
        .take(4)
        .any(|tok| tok == "public")
}

fn clean_rust_type_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_generics = trimmed.split('<').next().unwrap_or(trimmed).trim();
    without_generics
        .rsplit("::")
        .next()
        .unwrap_or(without_generics)
        .to_string()
}

fn extract_header_before(source: &str, start: usize, end: usize, stops: &[char]) -> String {
    let slice = &source[start..end];
    let cutoff = slice.find(stops).unwrap_or(slice.len());
    compact_ws(&slice[..cutoff])
}

fn compact_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

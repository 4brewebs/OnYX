use std::collections::BTreeSet;

use tree_sitter::{Language, Node, Parser};

use crate::ontology::{EntityKind, RelationKind, ScentError, SupportedLang};

/// Intermediate symbol extracted from a single file before global ID assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEntity {
    pub short_name: String,
    pub owner_type: Option<String>,
    pub kind: EntityKind,
    pub signature: String,
    pub is_public: bool,
    pub is_test: bool,
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
    pub is_method_call: bool,
}

/// Semantic type usage or trait implementation link extracted from AST signatures/impls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTypeRef {
    pub source_entity_idx: usize,
    pub target_type: String,
    pub kind: RelationKind,
}

/// Cached per-file AST distillation payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub lang: SupportedLang,
    pub blake3_hash: [u8; 32],
    /// File size in bytes at the moment the entry was (re)validated (racy-clean stat fast path).
    pub file_len: u64,
    /// File `mtime` in seconds since the Unix epoch at the moment the entry was validated.
    pub mtime_secs: u64,
    /// File `mtime` nanosecond remainder at the moment the entry was validated.
    pub mtime_nanos: u32,
    pub entities: Vec<ParsedEntity>,
    pub raw_calls: Vec<RawCall>,
    pub raw_type_refs: Vec<RawTypeRef>,
}

fn language_grammar(lang: SupportedLang, path: &str) -> Language {
    match lang {
        SupportedLang::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLang::Python => tree_sitter_python::LANGUAGE.into(),
        SupportedLang::Java => tree_sitter_java::LANGUAGE.into(),
        SupportedLang::TypeScript => {
            if path.to_ascii_lowercase().ends_with(".tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        SupportedLang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        SupportedLang::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    }
}

fn path_looks_like_test(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    norm.starts_with("tests/")
        || norm.contains("/tests/")
        || norm.contains("/__tests__/")
        || norm.starts_with("__tests__/")
        || norm.ends_with("_test.rs")
        || norm.ends_with("_test.py")
        || norm.ends_with("Test.java")
        || norm.ends_with(".test.ts")
        || norm.ends_with(".spec.ts")
        || norm.ends_with(".test.tsx")
        || norm.ends_with(".spec.tsx")
        || norm.ends_with(".test.js")
        || norm.ends_with(".spec.js")
        || norm.ends_with(".test.jsx")
        || norm.ends_with(".spec.jsx")
        || norm.ends_with("_test.go")
        || norm.ends_with("_test.cpp")
        || norm.ends_with("_test.c")
}

/// Parses `source` for `lang`, extracting symbols, compact signatures, line ranges,
/// call edges, and type/trait references.
pub fn parse_source(
    path: &str,
    lang: SupportedLang,
    source: &str,
    blake3_hash: [u8; 32],
) -> Result<ParsedFile, ScentError> {
    let mut parser = Parser::new();
    let language = language_grammar(lang, path);
    parser
        .set_language(&language)
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

    let file_is_test = path_looks_like_test(path);
    let lower_path = path.to_ascii_lowercase();
    let is_jsx_file = lower_path.ends_with(".tsx") || lower_path.ends_with(".jsx");
    let mut ctx = WalkCtx {
        source,
        lang,
        is_jsx_file,
        entities: Vec::new(),
        raw_calls: Vec::new(),
        raw_type_refs: Vec::new(),
        type_stack: Vec::new(),
        pub_trait_stack: Vec::new(),
        mod_stack: Vec::new(),
        in_test_depth: usize::from(file_is_test),
        in_export: false,
        go_receiver_var: None,
        cpp_member_public: Vec::new(),
        callable_stack: Vec::new(),
    };
    ctx.visit(tree.root_node());

    Ok(ParsedFile {
        lang,
        blake3_hash,
        file_len: 0,
        mtime_secs: 0,
        mtime_nanos: 0,
        entities: ctx.entities,
        raw_calls: ctx.raw_calls,
        raw_type_refs: ctx.raw_type_refs,
    })
}

struct EntityDraft {
    short_name: String,
    owner_type: Option<String>,
    kind: EntityKind,
    signature: String,
    is_public: bool,
    is_test: bool,
}

struct WalkCtx<'a> {
    source: &'a str,
    lang: SupportedLang,
    is_jsx_file: bool,
    entities: Vec<ParsedEntity>,
    raw_calls: Vec<RawCall>,
    raw_type_refs: Vec<RawTypeRef>,
    type_stack: Vec<String>,
    pub_trait_stack: Vec<bool>,
    mod_stack: Vec<String>,
    in_test_depth: usize,
    in_export: bool,
    go_receiver_var: Option<String>,
    cpp_member_public: Vec<bool>,
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
            SupportedLang::TypeScript | SupportedLang::JavaScript => self.visit_ts_js(node),
            SupportedLang::Go => self.visit_go(node),
            SupportedLang::Cpp => self.visit_cpp(node),
        }
    }

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child);
        }
    }

    fn current_owner_type(&self) -> Option<String> {
        match (self.mod_stack.is_empty(), self.type_stack.last()) {
            (true, Some(t)) => Some(t.clone()),
            (false, Some(t)) => Some(format!("{}::{t}", self.mod_stack.join("::"))),
            (false, None) => Some(self.mod_stack.join("::")),
            (true, None) => None,
        }
    }

    fn push_entity(&mut self, node: Node<'_>, draft: EntityDraft) -> usize {
        let idx = self.entities.len();
        self.entities.push(ParsedEntity {
            short_name: draft.short_name,
            owner_type: draft.owner_type,
            kind: draft.kind,
            signature: draft.signature,
            is_public: draft.is_public,
            is_test: draft.is_test,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        idx
    }

    fn visit_rust(&mut self, node: Node<'_>) {
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "mod_item" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if node.child_by_field_name("body").is_some() {
                        let enters_test = has_rust_test_attr(self.source, node);
                        if enters_test {
                            self.in_test_depth += 1;
                        }
                        self.mod_stack.push(self.text(name_node).to_string());
                        self.visit_children(node);
                        self.mod_stack.pop();
                        if enters_test {
                            self.in_test_depth -= 1;
                        }
                        return;
                    }
                }
            }
            "struct_item" | "enum_item" | "trait_item" if !inside_callable => {
                let kind = match node.kind() {
                    "struct_item" => EntityKind::Struct,
                    "enum_item" => EntityKind::Enum,
                    _ => EntityKind::Trait,
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let header = extract_header_before(
                        self.source,
                        node.start_byte(),
                        node.end_byte(),
                        &['{', ';'],
                    );
                    let sig = match kind {
                        EntityKind::Struct => summarize_rust_struct(self.source, node, &header),
                        EntityKind::Enum => summarize_rust_enum(self.source, node, &header),
                        _ => header,
                    };
                    let is_pub = has_rust_visibility(node);
                    let is_test = self.in_test_depth > 0 || has_rust_test_attr(self.source, node);
                    let owner = if self.mod_stack.is_empty() {
                        None
                    } else {
                        Some(self.mod_stack.join("::"))
                    };
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: owner,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test,
                        },
                    );
                    if kind == EntityKind::Struct {
                        self.record_rust_type_refs(node, idx, &name);
                    }
                    if is_test {
                        self.in_test_depth += 1;
                    }
                    self.type_stack.push(name);
                    if kind == EntityKind::Trait {
                        self.pub_trait_stack.push(is_pub);
                    }
                    self.visit_children(node);
                    if kind == EntityKind::Trait {
                        self.pub_trait_stack.pop();
                    }
                    self.type_stack.pop();
                    if is_test {
                        self.in_test_depth -= 1;
                    }
                    return;
                }
            }
            "impl_item" if !inside_callable => {
                if let Some(type_node) = node.child_by_field_name("type") {
                    let impl_name = clean_rust_type_name(self.text(type_node));
                    let trait_name = node
                        .child_by_field_name("trait")
                        .map(|t| clean_rust_type_name(self.text(t)));
                    let enters_test = has_rust_test_attr(self.source, node);
                    if enters_test {
                        self.in_test_depth += 1;
                    }
                    self.type_stack.push(impl_name.clone());
                    let prev_len = self.entities.len();
                    self.visit_children(node);
                    if let Some(tr) = trait_name {
                        // Vincular métodos del impl al trait que implementan.
                        for idx in prev_len..self.entities.len() {
                            self.raw_type_refs.push(RawTypeRef {
                                source_entity_idx: idx,
                                target_type: tr.clone(),
                                kind: RelationKind::Implements,
                            });
                            self.raw_type_refs.push(RawTypeRef {
                                source_entity_idx: idx,
                                target_type: impl_name.clone(),
                                kind: RelationKind::UsesType,
                            });
                        }
                    }
                    self.type_stack.pop();
                    if enters_test {
                        self.in_test_depth -= 1;
                    }
                    return;
                }
            }
            "function_item" | "function_signature_item" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let owner = self.current_owner_type();
                    let kind = if self.type_stack.is_empty() {
                        EntityKind::Function
                    } else {
                        EntityKind::Method
                    };
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start])
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let inherited_trait_pub = self.pub_trait_stack.last().copied().unwrap_or(false);
                    let is_pub = has_rust_visibility(node) || inherited_trait_pub;
                    let is_test = self.in_test_depth > 0 || has_rust_test_attr(self.source, node);
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name,
                            owner_type: owner,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test,
                        },
                    );
                    self.record_rust_fn_signature_types(node, idx);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "call_expression" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        if let Some((callee_name, qualifier, is_method_call)) =
                            self.extract_rust_callee(fn_node)
                        {
                            self.raw_calls.push(RawCall {
                                caller_entity_idx: caller_idx,
                                callee_name,
                                qualifier,
                                is_method_call,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn record_rust_type_refs(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut names = BTreeSet::new();
        if let Some(body) = node.child_by_field_name("body") {
            collect_rust_type_identifiers(self.source, body, &mut names);
        }
        for target in names {
            if target != self_name && !is_builtin_rust_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn record_rust_fn_signature_types(&mut self, node: Node<'_>, source_idx: usize) {
        let mut names = BTreeSet::new();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_rust_type_identifiers(self.source, params, &mut names);
        }
        if let Some(ret) = node.child_by_field_name("return_type") {
            collect_rust_type_identifiers(self.source, ret, &mut names);
        }
        for target in names {
            if !is_builtin_rust_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn extract_rust_callee(&self, mut node: Node<'_>) -> Option<(String, Option<String>, bool)> {
        if node.kind() == "generic_function" {
            node = node.child_by_field_name("function")?;
        }
        match node.kind() {
            "identifier" => Some((self.text(node).to_string(), None, false)),
            "field_expression" => {
                let field = node.child_by_field_name("field")?;
                let qual = node.child_by_field_name("value").and_then(|v| {
                    if v.kind() == "self" || self.text(v) == "self" {
                        Some("self".to_string())
                    } else if v.kind() == "identifier" {
                        let txt = self.text(v);
                        if txt.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                            Some(txt.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });
                Some((self.text(field).to_string(), qual, true))
            }
            "scoped_identifier" => {
                let name = node.child_by_field_name("name")?;
                let qual = node
                    .child_by_field_name("path")
                    .map(|p| clean_rust_type_name(self.text(p)));
                Some((self.text(name).to_string(), qual, false))
            }
            _ => None,
        }
    }

    fn visit_python(&mut self, node: Node<'_>) {
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "class_definition" if !inside_callable => {
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
                    self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind: EntityKind::Class,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "function_definition" if !inside_callable => {
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
                    let is_pub =
                        !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"));
                    let is_test = self.in_test_depth > 0 || name.starts_with("test_");
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name,
                            owner_type: owner,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test,
                        },
                    );
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
                                    is_method_call: false,
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
                                        is_method_call: true,
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
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "class_declaration" | "interface_declaration" | "enum_declaration"
                if !inside_callable =>
            {
                let kind = match node.kind() {
                    "interface_declaration" => EntityKind::Interface,
                    "enum_declaration" => EntityKind::Enum,
                    _ => EntityKind::Class,
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = has_java_public(self.source, node);
                    self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    return;
                }
            }
            "method_declaration" | "constructor_declaration" if !inside_callable => {
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
                    let is_pub = has_java_public(self.source, node);
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name,
                            owner_type: owner,
                            kind: EntityKind::Method,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
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
                            is_method_call: true,
                        });
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn visit_ts_js(&mut self, node: Node<'_>) {
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "export_statement" if !inside_callable => {
                let prev = self.in_export;
                self.in_export = true;
                self.visit_children(node);
                self.in_export = prev;
                return;
            }
            "class_declaration" | "abstract_class_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = self.in_export;
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind: EntityKind::Class,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.record_ts_class_heritage(node, idx, &name);
                    let prev_export = self.in_export;
                    self.in_export = false;
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    self.in_export = prev_export;
                    return;
                }
            }
            "interface_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = self.in_export;
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind: EntityKind::Interface,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.record_ts_type_refs(node, idx, &name);
                    let prev_export = self.in_export;
                    self.in_export = false;
                    self.type_stack.push(name);
                    self.visit_children(node);
                    self.type_stack.pop();
                    self.in_export = prev_export;
                    return;
                }
            }
            "type_alias_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let sig = compact_ws(self.text(node))
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let is_pub = self.in_export;
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind: EntityKind::TypeAlias,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    if let Some(val) = node.child_by_field_name("value") {
                        self.record_ts_type_refs(val, idx, &name);
                    }
                    return;
                }
            }
            "enum_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = self.in_export;
                    self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name,
                            owner_type: None,
                            kind: EntityKind::Enum,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    return;
                }
            }
            "function_declaration" | "generator_function_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body = node.child_by_field_name("body");
                    let body_start = body
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let kind = if self.is_jsx_file
                        && starts_ascii_upper(&name)
                        && body.is_some_and(node_contains_jsx)
                    {
                        EntityKind::Component
                    } else {
                        EntityKind::Function
                    };
                    let is_pub = self.in_export;
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.record_ts_fn_signature_types(node, idx, &name);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "method_definition" | "method_signature" if !inside_callable => {
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
                    let is_pub =
                        !name.starts_with('#') && !has_ts_private_or_protected(self.source, node);
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: owner,
                            kind: EntityKind::Method,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.record_ts_fn_signature_types(node, idx, &name);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "lexical_declaration" | "variable_declaration" if !inside_callable => {
                let mut handled_any = false;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    if name_node.kind() != "identifier" {
                        continue;
                    }
                    let Some(value_node) = child.child_by_field_name("value") else {
                        continue;
                    };
                    if !matches!(
                        value_node.kind(),
                        "arrow_function" | "function_expression" | "function"
                    ) {
                        continue;
                    }
                    handled_any = true;
                    let name = self.text(name_node).to_string();
                    let body = value_node.child_by_field_name("body");
                    let body_start = body
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| value_node.end_byte());
                    // Firma por declarador (no por la declaración completa): con varios
                    // declaradores (`const a = ..., b = ...`) cada firma es autónoma.
                    let sig = compact_ws(&self.source[child.start_byte()..body_start])
                        .trim_end_matches("=>")
                        .trim()
                        .to_string();
                    let kind = if self.is_jsx_file
                        && starts_ascii_upper(&name)
                        && body.is_some_and(node_contains_jsx)
                    {
                        EntityKind::Component
                    } else {
                        EntityKind::Function
                    };
                    let is_pub = self.in_export;
                    let idx = self.push_entity(
                        child,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    self.record_ts_fn_signature_types(value_node, idx, &name);
                    self.callable_stack.push(idx);
                    if let Some(body_node) = body {
                        self.visit(body_node);
                    }
                    self.callable_stack.pop();
                }
                if handled_any {
                    return;
                }
            }
            "call_expression" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        match fn_node.kind() {
                            "identifier" => {
                                self.raw_calls.push(RawCall {
                                    caller_entity_idx: caller_idx,
                                    callee_name: self.text(fn_node).to_string(),
                                    qualifier: None,
                                    is_method_call: false,
                                });
                            }
                            "member_expression" => {
                                if let Some(prop) = fn_node.child_by_field_name("property") {
                                    let qual =
                                        fn_node.child_by_field_name("object").and_then(|o| {
                                            if o.kind() == "this" || self.text(o) == "this" {
                                                Some("self".to_string())
                                            } else if o.kind() == "identifier" {
                                                let txt = self.text(o);
                                                if starts_ascii_upper(txt) {
                                                    Some(txt.to_string())
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });
                                    self.raw_calls.push(RawCall {
                                        caller_entity_idx: caller_idx,
                                        callee_name: self.text(prop).to_string(),
                                        qualifier: qual,
                                        is_method_call: true,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(tag) = node.child_by_field_name("name") {
                        let tag_txt = self.text(tag);
                        let leaf = tag_txt.rsplit('.').next().unwrap_or(tag_txt);
                        if starts_ascii_upper(leaf) {
                            self.raw_calls.push(RawCall {
                                caller_entity_idx: caller_idx,
                                callee_name: leaf.to_string(),
                                qualifier: None,
                                is_method_call: false,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn record_ts_class_heritage(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "class_heritage" {
                continue;
            }
            let mut h_cur = child.walk();
            for clause in child.children(&mut h_cur) {
                if matches!(clause.kind(), "implements_clause" | "extends_clause") {
                    let mut names = BTreeSet::new();
                    collect_ts_type_identifiers(self.source, clause, &mut names);
                    for target in names {
                        if target != self_name && !is_builtin_ts_type(&target) {
                            self.raw_type_refs.push(RawTypeRef {
                                source_entity_idx: source_idx,
                                target_type: target,
                                kind: RelationKind::Implements,
                            });
                        }
                    }
                }
            }
        }
    }

    fn record_ts_type_refs(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut names = BTreeSet::new();
        collect_ts_type_identifiers(self.source, node, &mut names);
        for target in names {
            if target != self_name && !is_builtin_ts_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn record_ts_fn_signature_types(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut names = BTreeSet::new();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_ts_type_identifiers(self.source, params, &mut names);
        }
        if let Some(ret) = node.child_by_field_name("return_type") {
            collect_ts_type_identifiers(self.source, ret, &mut names);
        }
        for target in names {
            if target != self_name && !is_builtin_ts_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn visit_go(&mut self, node: Node<'_>) {
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "type_spec" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let type_node = node.child_by_field_name("type");
                    let kind = match type_node.map(|n| n.kind()) {
                        Some("struct_type") => EntityKind::Struct,
                        Some("interface_type") => EntityKind::Interface,
                        _ => EntityKind::TypeAlias,
                    };
                    let header = extract_header_before(
                        self.source,
                        node.start_byte(),
                        node.end_byte(),
                        &['{'],
                    );
                    let sig = format!("type {header}");
                    let is_pub = starts_ascii_upper(&name);
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    if let Some(tn) = type_node {
                        let mut names = BTreeSet::new();
                        collect_go_type_identifiers(self.source, tn, &mut names);
                        for target in names {
                            if target != name && !is_builtin_go_type(&target) {
                                self.raw_type_refs.push(RawTypeRef {
                                    source_entity_idx: idx,
                                    target_type: target,
                                    kind: RelationKind::UsesType,
                                });
                            }
                        }
                    }
                    return;
                }
            }
            "function_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = starts_ascii_upper(&name);
                    // `scope:test` solo por ubicación de fichero (`*_test.go`): el prefijo
                    // `Test`/`Benchmark` es habitual en código de producción (p. ej.
                    // `TestConnection()` en health checks) y no debe contaminar el grafo.
                    let is_test = self.in_test_depth > 0;
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: None,
                            kind: EntityKind::Function,
                            signature: sig,
                            is_public: is_pub,
                            is_test,
                        },
                    );
                    self.record_go_fn_signature_types(node, idx, &name);
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    return;
                }
            }
            "method_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let (recv_var, recv_type) = node
                        .child_by_field_name("receiver")
                        .map(|r| extract_go_receiver(self.source, r))
                        .unwrap_or((None, None));
                    let body_start = node
                        .child_by_field_name("body")
                        .map(|b| b.start_byte())
                        .unwrap_or_else(|| node.end_byte());
                    let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                    let is_pub = starts_ascii_upper(&name);
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: recv_type.clone(),
                            kind: EntityKind::Method,
                            signature: sig,
                            is_public: is_pub,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    if let Some(rt) = recv_type {
                        if !is_builtin_go_type(&rt) {
                            self.raw_type_refs.push(RawTypeRef {
                                source_entity_idx: idx,
                                target_type: rt,
                                kind: RelationKind::UsesType,
                            });
                        }
                    }
                    self.record_go_fn_signature_types(node, idx, &name);
                    let prev_recv = self.go_receiver_var.replace(recv_var.unwrap_or_default());
                    self.callable_stack.push(idx);
                    self.visit_children(node);
                    self.callable_stack.pop();
                    self.go_receiver_var = prev_recv;
                    return;
                }
            }
            "call_expression" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        match fn_node.kind() {
                            "identifier" => {
                                self.raw_calls.push(RawCall {
                                    caller_entity_idx: caller_idx,
                                    callee_name: self.text(fn_node).to_string(),
                                    qualifier: None,
                                    is_method_call: false,
                                });
                            }
                            "selector_expression" => {
                                if let Some(field) = fn_node.child_by_field_name("field") {
                                    let qual =
                                        fn_node.child_by_field_name("operand").and_then(|o| {
                                            if o.kind() != "identifier" {
                                                return None;
                                            }
                                            let txt = self.text(o);
                                            if self
                                                .go_receiver_var
                                                .as_deref()
                                                .is_some_and(|rv| !rv.is_empty() && rv == txt)
                                            {
                                                Some("self".to_string())
                                            } else if starts_ascii_upper(txt) {
                                                Some(txt.to_string())
                                            } else {
                                                None
                                            }
                                        });
                                    self.raw_calls.push(RawCall {
                                        caller_entity_idx: caller_idx,
                                        callee_name: self.text(field).to_string(),
                                        qualifier: qual,
                                        is_method_call: true,
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

    fn record_go_fn_signature_types(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut names = BTreeSet::new();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_go_type_identifiers(self.source, params, &mut names);
        }
        if let Some(res) = node.child_by_field_name("result") {
            collect_go_type_identifiers(self.source, res, &mut names);
        }
        for target in names {
            if target != self_name && !is_builtin_go_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn visit_cpp(&mut self, node: Node<'_>) {
        let inside_callable = !self.callable_stack.is_empty();
        match node.kind() {
            "namespace_definition" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let ns = compact_ws(self.text(name_node));
                    if !ns.is_empty() {
                        self.mod_stack.push(ns);
                        self.visit_children(node);
                        self.mod_stack.pop();
                        return;
                    }
                }
            }
            "access_specifier" if !inside_callable => {
                let is_pub = self.text(node).trim().starts_with("public");
                if let Some(slot) = self.cpp_member_public.last_mut() {
                    *slot = is_pub;
                }
            }
            "class_specifier" | "struct_specifier" if !inside_callable => {
                if node.child_by_field_name("body").is_some() {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let name = clean_rust_type_name(self.text(name_node));
                        let kind = if node.kind() == "struct_specifier" {
                            EntityKind::Struct
                        } else {
                            EntityKind::Class
                        };
                        let sig = extract_header_before(
                            self.source,
                            node.start_byte(),
                            node.end_byte(),
                            &['{'],
                        );
                        let owner = if self.mod_stack.is_empty() {
                            None
                        } else {
                            Some(self.mod_stack.join("::"))
                        };
                        let idx = self.push_entity(
                            node,
                            EntityDraft {
                                short_name: name.clone(),
                                owner_type: owner,
                                kind,
                                signature: sig,
                                is_public: true,
                                is_test: self.in_test_depth > 0,
                            },
                        );
                        self.record_cpp_base_classes(node, idx, &name);
                        let default_pub = kind == EntityKind::Struct;
                        self.type_stack.push(name);
                        self.cpp_member_public.push(default_pub);
                        self.visit_children(node);
                        self.cpp_member_public.pop();
                        self.type_stack.pop();
                        return;
                    }
                }
            }
            "enum_specifier" if !inside_callable => {
                if node.child_by_field_name("body").is_some() {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let name = self.text(name_node).to_string();
                        let sig = extract_header_before(
                            self.source,
                            node.start_byte(),
                            node.end_byte(),
                            &['{'],
                        );
                        let owner = if self.mod_stack.is_empty() {
                            None
                        } else {
                            Some(self.mod_stack.join("::"))
                        };
                        self.push_entity(
                            node,
                            EntityDraft {
                                short_name: name,
                                owner_type: owner,
                                kind: EntityKind::Enum,
                                signature: sig,
                                is_public: true,
                                is_test: self.in_test_depth > 0,
                            },
                        );
                        return;
                    }
                }
            }
            "alias_declaration" if !inside_callable => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node).to_string();
                    let sig = compact_ws(self.text(node))
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    let owner = if self.mod_stack.is_empty() {
                        None
                    } else {
                        Some(self.mod_stack.join("::"))
                    };
                    let idx = self.push_entity(
                        node,
                        EntityDraft {
                            short_name: name.clone(),
                            owner_type: owner,
                            kind: EntityKind::TypeAlias,
                            signature: sig,
                            is_public: true,
                            is_test: self.in_test_depth > 0,
                        },
                    );
                    if let Some(ty) = node.child_by_field_name("type") {
                        self.record_cpp_type_refs(ty, idx, &name);
                    }
                    return;
                }
            }
            "function_definition" if !inside_callable => {
                if let Some(decl) = node.child_by_field_name("declarator") {
                    if let Some((qual_owner, short_name)) = extract_cpp_fn_name(self.source, decl) {
                        let owner = match (qual_owner, self.current_owner_type()) {
                            (Some(qo), Some(co)) => Some(format!("{co}::{qo}")),
                            (Some(qo), None) => Some(qo),
                            (None, co) => co,
                        };
                        let kind = if owner.is_some()
                            && (!self.type_stack.is_empty()
                                || owner
                                    .as_deref()
                                    .is_some_and(|o| o != self.mod_stack.join("::")))
                        {
                            EntityKind::Method
                        } else {
                            EntityKind::Function
                        };
                        let body_start = node
                            .child_by_field_name("body")
                            .map(|b| b.start_byte())
                            .unwrap_or_else(|| node.end_byte());
                        let sig = compact_ws(&self.source[node.start_byte()..body_start]);
                        let is_pub = self.cpp_member_public.last().copied().unwrap_or(true);
                        let idx = self.push_entity(
                            node,
                            EntityDraft {
                                short_name: short_name.clone(),
                                owner_type: owner,
                                kind,
                                signature: sig,
                                is_public: is_pub,
                                is_test: self.in_test_depth > 0,
                            },
                        );
                        self.record_cpp_fn_signature_types(node, decl, idx, &short_name);
                        self.callable_stack.push(idx);
                        if let Some(body) = node.child_by_field_name("body") {
                            self.visit(body);
                        }
                        self.callable_stack.pop();
                        return;
                    }
                }
            }
            "call_expression" => {
                if let Some(&caller_idx) = self.callable_stack.last() {
                    if let Some(fn_node) = node.child_by_field_name("function") {
                        match fn_node.kind() {
                            "identifier" => {
                                self.raw_calls.push(RawCall {
                                    caller_entity_idx: caller_idx,
                                    callee_name: self.text(fn_node).to_string(),
                                    qualifier: None,
                                    is_method_call: false,
                                });
                            }
                            "qualified_identifier" => {
                                if let Some(name_n) = fn_node.child_by_field_name("name") {
                                    let qual = fn_node
                                        .child_by_field_name("scope")
                                        .map(|s| clean_rust_type_name(self.text(s)));
                                    self.raw_calls.push(RawCall {
                                        caller_entity_idx: caller_idx,
                                        callee_name: self.text(name_n).to_string(),
                                        qualifier: qual,
                                        is_method_call: false,
                                    });
                                }
                            }
                            "field_expression" => {
                                if let Some(field) = fn_node.child_by_field_name("field") {
                                    let qual =
                                        fn_node.child_by_field_name("argument").and_then(|a| {
                                            let txt = self.text(a);
                                            if a.kind() == "this" || txt == "this" {
                                                Some("self".to_string())
                                            } else if a.kind() == "identifier"
                                                && starts_ascii_upper(txt)
                                            {
                                                Some(txt.to_string())
                                            } else {
                                                None
                                            }
                                        });
                                    self.raw_calls.push(RawCall {
                                        caller_entity_idx: caller_idx,
                                        callee_name: self.text(field).to_string(),
                                        qualifier: qual,
                                        is_method_call: true,
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

    fn record_cpp_base_classes(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "base_class_clause" {
                let mut names = BTreeSet::new();
                collect_cpp_type_identifiers(self.source, child, &mut names);
                for target in names {
                    if target != self_name && !is_builtin_cpp_type(&target) {
                        self.raw_type_refs.push(RawTypeRef {
                            source_entity_idx: source_idx,
                            target_type: target,
                            kind: RelationKind::Implements,
                        });
                    }
                }
            }
        }
    }

    fn record_cpp_type_refs(&mut self, node: Node<'_>, source_idx: usize, self_name: &str) {
        let mut names = BTreeSet::new();
        collect_cpp_type_identifiers(self.source, node, &mut names);
        for target in names {
            if target != self_name && !is_builtin_cpp_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }

    fn record_cpp_fn_signature_types(
        &mut self,
        fn_node: Node<'_>,
        decl_node: Node<'_>,
        source_idx: usize,
        self_name: &str,
    ) {
        let mut names = BTreeSet::new();
        if let Some(ty) = fn_node.child_by_field_name("type") {
            collect_cpp_type_identifiers(self.source, ty, &mut names);
        }
        collect_cpp_type_identifiers(self.source, decl_node, &mut names);
        for target in names {
            if target != self_name && !is_builtin_cpp_type(&target) {
                self.raw_type_refs.push(RawTypeRef {
                    source_entity_idx: source_idx,
                    target_type: target,
                    kind: RelationKind::UsesType,
                });
            }
        }
    }
}

fn summarize_rust_struct(source: &str, node: Node<'_>, header: &str) -> String {
    let Some(body) = node.child_by_field_name("body") else {
        return header.to_string();
    };
    if body.kind() != "field_declaration_list" {
        return header.to_string();
    }
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "field_declaration" {
            if let Some(name_node) = child.child_by_field_name("name") {
                fields.push(source[name_node.start_byte()..name_node.end_byte()].to_string());
            }
        }
    }
    append_summary_braces(header, &fields)
}

fn summarize_rust_enum(source: &str, node: Node<'_>, header: &str) -> String {
    let Some(body) = node.child_by_field_name("body") else {
        return header.to_string();
    };
    let mut variants = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "enum_variant" {
            if let Some(name_node) = child.child_by_field_name("name") {
                variants.push(source[name_node.start_byte()..name_node.end_byte()].to_string());
            }
        }
    }
    append_summary_braces(header, &variants)
}

fn append_summary_braces(header: &str, items: &[String]) -> String {
    if items.is_empty() {
        return header.to_string();
    }
    const MAX_PREVIEW: usize = 6;
    if items.len() <= MAX_PREVIEW {
        format!("{header} {{ {} }}", items.join(", "))
    } else {
        let rem = items.len() - MAX_PREVIEW;
        format!("{header} {{ {}, +{rem} }}", items[..MAX_PREVIEW].join(", "))
    }
}

fn collect_rust_type_identifiers(source: &str, node: Node<'_>, out: &mut BTreeSet<String>) {
    if node.kind() == "type_identifier" {
        out.insert(source[node.start_byte()..node.end_byte()].to_string());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_type_identifiers(source, child, out);
    }
}

fn is_builtin_rust_type(name: &str) -> bool {
    matches!(
        name,
        "Self"
            | "String"
            | "str"
            | "Vec"
            | "Option"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "Mutex"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "VecDeque"
            | "Path"
            | "PathBuf"
            | "Duration"
            | "bool"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
    )
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

fn has_rust_test_attr(source: &str, node: Node<'_>) -> bool {
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "attribute_item" {
            let txt = &source[sib.start_byte()..sib.end_byte()];
            if txt.contains("cfg(test)") || txt.contains("#[test]") {
                return true;
            }
            prev = sib.prev_sibling();
        } else {
            break;
        }
    }
    false
}

fn has_java_public(source: &str, node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mods_txt = &source[child.start_byte()..child.end_byte()];
            return mods_txt.split_whitespace().any(|tok| tok == "public");
        }
    }
    false
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

fn starts_ascii_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn node_contains_jsx(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "jsx_element" | "jsx_self_closing_element" | "jsx_fragment"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if node_contains_jsx(child) {
            return true;
        }
    }
    false
}

fn has_ts_private_or_protected(source: &str, node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            let txt = source[child.start_byte()..child.end_byte()].trim();
            if txt == "private" || txt == "protected" {
                return true;
            }
        }
    }
    false
}

fn collect_ts_type_identifiers(source: &str, node: Node<'_>, out: &mut BTreeSet<String>) {
    if matches!(node.kind(), "type_identifier" | "identifier")
        && !matches!(
            node.parent().map(|p| p.kind()),
            Some("required_parameter" | "optional_parameter" | "formal_parameters")
        )
    {
        let txt = source[node.start_byte()..node.end_byte()].trim();
        if node.kind() == "type_identifier" || starts_ascii_upper(txt) {
            out.insert(txt.to_string());
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ts_type_identifiers(source, child, out);
    }
}

fn is_builtin_ts_type(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "ReadonlyArray"
            | "Promise"
            | "Record"
            | "Partial"
            | "Required"
            | "Readonly"
            | "Pick"
            | "Omit"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "Date"
            | "Error"
            | "RegExp"
            | "Function"
            | "Object"
            | "String"
            | "Number"
            | "Boolean"
            | "Symbol"
            | "BigInt"
            | "Uint8Array"
            | "JSX"
            | "React"
            | "FC"
            | "ReactNode"
            | "ReactElement"
    )
}

fn extract_go_receiver(source: &str, receiver: Node<'_>) -> (Option<String>, Option<String>) {
    let mut var_name: Option<String> = None;
    let mut type_name: Option<String> = None;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            if let Some(n) = child.child_by_field_name("name") {
                var_name = Some(source[n.start_byte()..n.end_byte()].to_string());
            }
            if let Some(t) = child.child_by_field_name("type") {
                let mut names = BTreeSet::new();
                collect_go_type_identifiers(source, t, &mut names);
                type_name = names.into_iter().next();
            }
        }
    }
    (var_name, type_name)
}

fn collect_go_type_identifiers(source: &str, node: Node<'_>, out: &mut BTreeSet<String>) {
    if node.kind() == "type_identifier" {
        out.insert(source[node.start_byte()..node.end_byte()].to_string());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_go_type_identifiers(source, child, out);
    }
}

fn is_builtin_go_type(name: &str) -> bool {
    matches!(
        name,
        "any"
            | "bool"
            | "byte"
            | "comparable"
            | "complex64"
            | "complex128"
            | "error"
            | "float32"
            | "float64"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "rune"
            | "string"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
    )
}

fn extract_cpp_fn_name(source: &str, mut node: Node<'_>) -> Option<(Option<String>, String)> {
    // Desenvuelve pointer_declarator / reference_declarator / function_declarator / parenthesized_declarator.
    loop {
        match node.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator" => {
                node = node.child_by_field_name("declarator")?;
            }
            "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
                return Some((None, source[node.start_byte()..node.end_byte()].to_string()));
            }
            "qualified_identifier" => {
                let scope = node
                    .child_by_field_name("scope")
                    .map(|s| clean_rust_type_name(&source[s.start_byte()..s.end_byte()]));
                let name = node.child_by_field_name("name")?;
                return Some((
                    scope,
                    source[name.start_byte()..name.end_byte()].to_string(),
                ));
            }
            _ => return None,
        }
    }
}

fn collect_cpp_type_identifiers(source: &str, node: Node<'_>, out: &mut BTreeSet<String>) {
    if node.kind() == "type_identifier" {
        out.insert(source[node.start_byte()..node.end_byte()].to_string());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_cpp_type_identifiers(source, child, out);
    }
}

fn is_builtin_cpp_type(name: &str) -> bool {
    matches!(
        name,
        "size_t"
            | "ssize_t"
            | "ptrdiff_t"
            | "int8_t"
            | "int16_t"
            | "int32_t"
            | "int64_t"
            | "uint8_t"
            | "uint16_t"
            | "uint32_t"
            | "uint64_t"
            | "string"
            | "string_view"
            | "vector"
            | "map"
            | "unordered_map"
            | "set"
            | "optional"
            | "unique_ptr"
            | "shared_ptr"
    )
}

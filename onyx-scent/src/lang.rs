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
    pub entities: Vec<ParsedEntity>,
    pub raw_calls: Vec<RawCall>,
    pub raw_type_refs: Vec<RawTypeRef>,
}

fn language_grammar(lang: SupportedLang) -> Language {
    match lang {
        SupportedLang::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLang::Python => tree_sitter_python::LANGUAGE.into(),
        SupportedLang::Java => tree_sitter_java::LANGUAGE.into(),
    }
}

fn path_looks_like_test(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    norm.starts_with("tests/")
        || norm.contains("/tests/")
        || norm.ends_with("_test.rs")
        || norm.ends_with("_test.py")
        || norm.ends_with("Test.java")
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
    let language = language_grammar(lang);
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
    let mut ctx = WalkCtx {
        source,
        lang,
        entities: Vec::new(),
        raw_calls: Vec::new(),
        raw_type_refs: Vec::new(),
        type_stack: Vec::new(),
        pub_trait_stack: Vec::new(),
        mod_stack: Vec::new(),
        in_test_depth: usize::from(file_is_test),
        callable_stack: Vec::new(),
    };
    ctx.visit(tree.root_node());

    Ok(ParsedFile {
        lang,
        blake3_hash,
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
    entities: Vec<ParsedEntity>,
    raw_calls: Vec<RawCall>,
    raw_type_refs: Vec<RawTypeRef>,
    type_stack: Vec<String>,
    pub_trait_stack: Vec<bool>,
    mod_stack: Vec<String>,
    in_test_depth: usize,
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

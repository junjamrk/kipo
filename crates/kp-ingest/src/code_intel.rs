use std::path::{Component, Path, PathBuf};

use kp_model::{ActorId, ContentHash, EpochMs, VerifyStatus};
use kp_store::{CodeAnchor, CodeLocus, Store};
use serde::Serialize;
use tree_sitter::{Language, Node, Parser};

use crate::{IngestError, Result};

#[derive(Clone, Debug, Serialize)]
pub struct Symbol {
    pub symbol_name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub sig_hash: ContentHash,
    pub body_hash: ContentHash,
}

pub fn language(path: &Path) -> Option<(&'static str, Language)> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some(("rust", tree_sitter_rust::LANGUAGE.into())),
        "py" => Some(("python", tree_sitter_python::LANGUAGE.into())),
        "js" | "jsx" => Some(("javascript", tree_sitter_javascript::LANGUAGE.into())),
        "ts" => Some((
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        )),
        "tsx" => Some(("tsx", tree_sitter_typescript::LANGUAGE_TSX.into())),
        "go" => Some(("go", tree_sitter_go::LANGUAGE.into())),
        _ => None,
    }
}

pub fn symbols(path: &Path, source: &str) -> Result<Vec<Symbol>> {
    let (_, language) =
        language(path).ok_or(IngestError::Invalid("unsupported source language"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| IngestError::Invalid("incompatible grammar"))?;
    let tree = parser
        .parse(source, None)
        .ok_or(IngestError::Invalid("source parse cancelled"))?;
    let mut pending = vec![(tree.root_node(), 0usize)];
    let mut nodes = 0usize;
    while let Some((node, depth)) = pending.pop() {
        nodes += 1;
        if depth > 128 || nodes > 500_000 {
            return Err(IngestError::Invalid("source syntax tree limit exceeded"));
        }
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, depth + 1)),
        );
    }
    let mut result = Vec::new();
    collect(tree.root_node(), source, &[], &mut result)?;
    Ok(result)
}

fn collect(node: Node<'_>, source: &str, scope: &[String], out: &mut Vec<Symbol>) -> Result<()> {
    let mut scope = scope.to_vec();
    if let Some(receiver) = node.child_by_field_name("receiver") {
        let mut cursor = receiver.walk();
        let receiver_type = receiver
            .named_children(&mut cursor)
            .find_map(|parameter| parameter.child_by_field_name("type"));
        if let Some(ty) = receiver_type {
            scope.push(source[ty.byte_range()].trim_start_matches('*').into());
        }
    }
    let is_symbol = matches!(
        node.kind(),
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "function_definition"
            | "class_definition"
            | "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "method_signature"
            | "class_declaration"
            | "interface_declaration"
            | "abstract_class_declaration"
            | "method_declaration"
            | "method_elem"
            | "type_spec"
            | "arrow_function"
            | "function_expression"
    );
    let name = node.child_by_field_name("name").or_else(|| {
        node.parent()
            .filter(|p| matches!(p.kind(), "variable_declarator" | "pair"))
            .and_then(|p| {
                p.child_by_field_name("name")
                    .or_else(|| p.child_by_field_name("key"))
            })
    });
    if is_symbol {
        if let Some(name) = name {
            scope.push(source[name.byte_range()].into());
            let body = node.child_by_field_name("body").or_else(|| {
                let mut cursor = node.walk();
                let found = node.named_children(&mut cursor).find(|n| {
                    matches!(
                        n.kind(),
                        "field_declaration_list"
                            | "ordered_field_declaration_list"
                            | "interface_body"
                            | "class_body"
                            | "struct_type"
                            | "interface_type"
                    )
                });
                found
            });
            let body_start = body
                .map(|b| {
                    if matches!(b.kind(), "struct_type" | "interface_type") {
                        let mut cursor = b.walk();
                        let brace = b.children(&mut cursor).find(|n| n.kind() == "{");
                        brace.map(|n| n.start_byte()).unwrap_or(b.start_byte())
                    } else {
                        b.start_byte()
                    }
                })
                .unwrap_or(node.end_byte());
            out.push(Symbol {
                symbol_name: scope.join("::"),
                start_line: u32::try_from(node.start_position().row + 1)
                    .map_err(|_| IngestError::Invalid("too many source lines"))?,
                end_line: u32::try_from(node.end_position().row + 1)
                    .map_err(|_| IngestError::Invalid("too many source lines"))?,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                sig_hash: normalized_hash(node, source, node.start_byte()..body_start),
                body_hash: normalized_hash(node, source, body_start..node.end_byte()),
            });
        }
    } else {
        let container = match node.kind() {
            "impl_item" => node.child_by_field_name("type").map(|ty| {
                let ty = &source[ty.byte_range()];
                match node.child_by_field_name("trait") {
                    Some(trait_name) => format!("<{ty} as {}>", &source[trait_name.byte_range()]),
                    None => ty.to_owned(),
                }
            }),
            "mod_item" | "internal_module" => node
                .child_by_field_name("name")
                .map(|name| source[name.byte_range()].to_owned()),
            _ => None,
        };
        if let Some(container) = container {
            scope.push(container);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, source, &scope, out)?;
    }
    Ok(())
}

fn normalized_hash(node: Node<'_>, source: &str, range: std::ops::Range<usize>) -> ContentHash {
    let mut tokens = Vec::new();
    normalize(node, source, &range, &mut tokens);
    ContentHash::digest(tokens.join(" "))
}

fn normalize<'a>(
    node: Node<'_>,
    source: &'a str,
    range: &std::ops::Range<usize>,
    tokens: &mut Vec<&'a str>,
) {
    if node.end_byte() <= range.start
        || node.start_byte() >= range.end
        || node.kind().contains("comment")
    {
        return;
    }
    // Literals are indivisible: whitespace and comment markers inside strings are semantic.
    if node.child_count() == 0
        || matches!(
            node.kind(),
            "string"
                | "string_literal"
                | "raw_string_literal"
                | "interpreted_string_literal"
                | "template_string"
        )
    {
        if node.start_byte() >= range.start && node.end_byte() <= range.end {
            let token = &source[node.byte_range()];
            if !token.trim().is_empty() {
                tokens.push(token);
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        normalize(child, source, range, tokens);
    }
}

/// Check lexical traversal and every existing ancestor, including external symlinks.
pub fn resolve_path(repo: &Path, file: &Path) -> Result<PathBuf> {
    if file.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(IngestError::Invalid("code path escapes repository"));
    }
    let root = repo.canonicalize()?;
    if !root.is_dir() {
        return Err(IngestError::Invalid("code path escapes repository"));
    }
    let target = root.join(file);
    let mut ancestor = target.as_path();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let suffix = target
                    .strip_prefix(ancestor)
                    .map_err(|_| IngestError::Invalid("invalid code path"))?;
                let mut resolved = ancestor.canonicalize()?;
                if !suffix.as_os_str().is_empty() {
                    resolved.push(suffix);
                }
                if !resolved.starts_with(&root) || resolved == root {
                    return Err(IngestError::Invalid("code path escapes repository"));
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or(IngestError::Invalid("invalid code path"))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn locate(repo: &Path, file: &Path, symbol: &str) -> Result<CodeLocus> {
    let root = repo.canonicalize()?;
    let path = resolve_path(&root, file)?;
    let bytes = crate::read_file(&path)?;
    let source = std::str::from_utf8(&bytes)
        .map_err(|_| IngestError::Invalid("source code must be UTF-8"))?;
    let symbols = symbols(&path, source)?;
    let found = select(&symbols, symbol)?.ok_or(IngestError::Invalid("code symbol not found"))?;
    Ok(CodeLocus {
        repo_root: root.to_string_lossy().into_owned(),
        file_path: path
            .strip_prefix(&root)
            .map_err(|_| IngestError::Invalid("code path escapes repository"))?
            .to_string_lossy()
            .into_owned(),
        language: language(&path)
            .ok_or(IngestError::Invalid("unsupported source language"))?
            .0
            .into(),
        symbol_name: found.symbol_name.clone(),
        sig_hash: found.sig_hash.clone(),
        body_hash: found.body_hash.clone(),
        start_line: found.start_line,
        end_line: found.end_line,
    })
}

fn select<'a>(symbols: &'a [Symbol], name: &str) -> Result<Option<&'a Symbol>> {
    let exact = symbols.iter().any(|s| s.symbol_name == name);
    let mut matches = symbols.iter().filter(|s| {
        if exact {
            s.symbol_name == name
        } else {
            s.symbol_name.rsplit("::").next() == Some(name)
        }
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err(IngestError::Invalid(
            "ambiguous symbol; use its qualified name",
        ));
    }
    Ok(found)
}

pub fn verify(
    store: &mut Store,
    repo: Option<&Path>,
    viewer: &ActorId,
    at: EpochMs,
) -> Result<Vec<CodeAnchor>> {
    let repo = repo.map(Path::canonicalize).transpose()?;
    let mut anchors = store.code_anchors(viewer, at)?;
    anchors.retain(|a| {
        repo.as_ref()
            .is_none_or(|root| root == Path::new(&a.locus.repo_root))
    });
    for anchor in &mut anchors {
        let root = Path::new(&anchor.locus.repo_root);
        let file = Path::new(&anchor.locus.file_path);
        if file.is_absolute() && !file.starts_with(root) {
            return Err(IngestError::Invalid("code path escapes repository"));
        }
        let path = match resolve_path(root, file) {
            Ok(path) => path,
            Err(IngestError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(root)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                anchor.verify_status = VerifyStatus::Orphaned;
                anchor.verified_at = at;
                continue;
            }
            Err(e) => return Err(e),
        };
        let source = match crate::read_file(&path) {
            Ok(bytes) => Some(
                String::from_utf8(bytes)
                    .map_err(|_| IngestError::Invalid("source code must be UTF-8"))?,
            ),
            Err(IngestError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let symbols = source
            .as_ref()
            .map(|s| symbols(&path, s))
            .transpose()?
            .unwrap_or_default();
        let found = select(&symbols, &anchor.locus.symbol_name)?;
        anchor.verify_status = match found {
            None => VerifyStatus::Orphaned,
            Some(s) if s.sig_hash != anchor.locus.sig_hash => VerifyStatus::SigChanged,
            Some(s) if s.body_hash != anchor.locus.body_hash => VerifyStatus::BodyModified,
            Some(s)
                if (s.start_line, s.end_line)
                    != (anchor.locus.start_line, anchor.locus.end_line) =>
            {
                VerifyStatus::MovedUnchanged
            }
            Some(_) => VerifyStatus::Verified,
        };
        if let Some(s) = found {
            anchor.locus.start_line = s.start_line;
            anchor.locus.end_line = s.end_line;
        }
        anchor.verified_at = at;
    }
    store.update_code_anchors(&anchors)?;
    kp_derive::derive(store)?;
    Ok(anchors)
}

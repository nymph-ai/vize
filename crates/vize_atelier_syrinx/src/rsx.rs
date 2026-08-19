//! Dioxus RSX compiler target for Syrinx live v3b.
//!
//! The generated Rust owns the render tree. Vue expressions are represented by
//! typed, source-located render-model fields; later expression passes fill
//! those fields directly in Rust. There is no mutation protocol, JavaScript
//! guest, site table, or renderer node identity in this artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vize_atelier_core::{
    ElementNode, ElementType, ExpressionNode, PropNode, SourceLocation, TemplateChildNode,
    TemplateSyntaxMode, TextCallContent,
};
use vize_atelier_sfc::{
    SfcDescriptor, SfcParseOptions, StyleCompileOptions, parse_sfc, style::compile_style,
};
use vize_atelier_vapor::{VaporCompilerOptions, compile_vapor_ir_with_template_syntax};
use vize_carton::Bump;

use crate::{SyrinxCompileFailure, SyrinxDiagnostic};

const VIZE_RSX_REVISION: &str = "fd841c9fb20edc6e538d1c951e16a9780ae4e013";

/// Inputs for the renderer-native v3b target.
///
/// This deliberately excludes every v2 guest/ABI option. An RSX artifact is a
/// Rust render module, so it has no protocol version, guest runtime, component
/// ID, capability bits, or wire limits to configure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyrinxRsxOptions {
    pub filename: String,
    pub component_name: Option<String>,
    pub compiler_revision: String,
    pub template_syntax: TemplateSyntaxMode,
}

impl Default for SyrinxRsxOptions {
    fn default() -> Self {
        Self {
            filename: "Component.vue".to_owned(),
            component_name: None,
            compiler_revision: VIZE_RSX_REVISION.to_owned(),
            template_syntax: TemplateSyntaxMode::Standard,
        }
    }
}

/// One expression boundary in an emitted RSX render model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RsxExpressionHook {
    pub field: String,
    pub role: String,
    pub expression: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// A deterministic, standalone Rust source artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyrinxRsxArtifact {
    pub component_name: String,
    pub rust_source: String,
    pub css: String,
    /// `<script setup>` bindings that were emitted as native Rust state or
    /// derived values rather than render-model inputs.
    pub compiled_bindings: Vec<String>,
    pub expression_hooks: Vec<RsxExpressionHook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldKind {
    Text,
    Attribute,
    DynamicAttributes,
    Condition,
    Event(&'static str),
    Slot,
    List(String),
}

impl FieldKind {
    fn rust_type(&self) -> String {
        match self {
            Self::Text | Self::Attribute => "String".to_owned(),
            Self::DynamicAttributes => "Vec<Attribute>".to_owned(),
            Self::Condition => "bool".to_owned(),
            Self::Event(event) => format!("EventHandler<{event}>"),
            Self::Slot => "Element".to_owned(),
            Self::List(item) => format!("Vec<{item}>"),
        }
    }
}

#[derive(Debug, Clone)]
struct ModelField {
    name: String,
    kind: FieldKind,
    expression: String,
}

#[derive(Debug, Clone)]
struct ListModel {
    id: u32,
    item_type: String,
    source: String,
    value_alias: Option<String>,
    index_alias: Option<String>,
    uses_index: bool,
    data_fields: BTreeSet<String>,
    fields: Vec<ModelField>,
}

#[derive(Debug, Clone)]
struct RefBinding {
    kind: RefKind,
    initial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    OptionalString,
    Bool,
    String,
    MountedData,
}

impl RefKind {
    fn rust_type(self) -> &'static str {
        match self {
            Self::OptionalString => "Signal<Option<String>>",
            Self::Bool => "Signal<bool>",
            Self::String => "Signal<String>",
            Self::MountedData => "Signal<Option<Rc<MountedData>>>",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ScriptLowering {
    refs: BTreeMap<String, RefBinding>,
    computed: BTreeMap<String, String>,
    setters: BTreeMap<String, String>,
    formatters: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
enum Scope {
    Root,
    List(usize),
}

struct RsxEmitter {
    component_name: String,
    model_name: String,
    root_fields: Vec<ModelField>,
    lists: Vec<ListModel>,
    script: ScriptLowering,
    scope_token: Option<String>,
    used_bindings: BTreeSet<String>,
    hooks: Vec<RsxExpressionHook>,
    next_field: u32,
    indent: usize,
    body: String,
}

impl ScriptLowering {
    fn parse(source: &str) -> Self {
        let mut lowering = Self::default();
        for line in source.lines().map(str::trim) {
            let Some(declaration) = line.strip_prefix("const ") else {
                continue;
            };
            let Some((name, initializer)) = declaration.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let initializer = initializer.trim();
            if let Some(argument) = initializer
                .strip_prefix("ref(")
                .and_then(|value| value.strip_suffix(')'))
            {
                let argument = argument.trim();
                let binding = if argument == "null" {
                    Some(RefBinding {
                        kind: RefKind::OptionalString,
                        initial: "None::<String>".to_owned(),
                    })
                } else if matches!(argument, "true" | "false") {
                    Some(RefBinding {
                        kind: RefKind::Bool,
                        initial: argument.to_owned(),
                    })
                } else {
                    js_string_literal(argument).map(|value| RefBinding {
                        kind: RefKind::String,
                        initial: format!("{}.to_owned()", rust_string(&value)),
                    })
                };
                if let Some(binding) = binding {
                    lowering.refs.insert(name.to_owned(), binding);
                }
                continue;
            }
            if let Some(expression) = initializer
                .strip_prefix("computed(() =>")
                .and_then(|value| value.trim().strip_suffix(')'))
            {
                lowering
                    .computed
                    .insert(name.to_owned(), expression.trim().to_owned());
                continue;
            }
            if initializer.contains("=>") && initializer.contains(".value =") {
                if let Some((_, assignment)) = initializer.split_once("=>") {
                    let assignment = assignment
                        .trim()
                        .trim_start_matches('{')
                        .trim_end_matches('}')
                        .trim();
                    if let Some((target, _)) = assignment.split_once(".value =") {
                        lowering
                            .setters
                            .insert(name.to_owned(), target.trim().to_owned());
                    }
                }
                continue;
            }
            if initializer.contains("=>") && initializer.contains("${index + 1}") {
                lowering.formatters.insert(name.to_owned());
            }
        }
        lowering
    }
}

/// Compile an ordinary Vue SFC to an explicit Dioxus `rsx!` render module.
///
/// This frontend parses the SFC and renderer-neutral Vapor IR directly. It
/// does not invoke or configure the v2 ComponentPlan/guest compiler.
pub fn compile_syrinx_rsx(
    source: &str,
    options: SyrinxRsxOptions,
) -> Result<SyrinxRsxArtifact, SyrinxCompileFailure> {
    let descriptor = parse_sfc(
        source,
        SfcParseOptions {
            filename: options.filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    )
    .map_err(|error| sfc_failure(source, &options.filename, error))?;
    let template = descriptor.template.as_ref().ok_or_else(|| {
        file_failure(
            source,
            &options.filename,
            "SYRINX_RSX_TEMPLATE_REQUIRED",
            "an RSX component requires a <template> block",
            0,
            source.len(),
        )
    })?;
    let script_content = descriptor
        .script_setup
        .as_ref()
        .map(|block| block.content.as_ref())
        .unwrap_or("");
    let mut script_context = vize_atelier_sfc::script::ScriptCompileContext::new(script_content);
    script_context.analyze();

    let allocator = Bump::new();
    let lowered = compile_vapor_ir_with_template_syntax(
        &allocator,
        template.content.as_ref(),
        VaporCompilerOptions {
            prefix_identifiers: true,
            binding_metadata: Some(script_context.bindings.clone()),
            inline: true,
            custom_renderer: true,
            ..Default::default()
        },
        options.template_syntax,
    );
    let fatal = lowered
        .parser_diagnostics
        .iter()
        .filter(|diagnostic| !diagnostic.is_recoverable())
        .map(|diagnostic| {
            let (start, end) =
                diagnostic
                    .loc
                    .as_ref()
                    .map_or((template.loc.start, template.loc.end), |loc| {
                        (
                            template.loc.start + loc.start.offset as usize,
                            template.loc.start + loc.end.offset as usize,
                        )
                    });
            source_diagnostic(
                source,
                &options.filename,
                &format!("SYRINX_RSX_TEMPLATE_{:?}", diagnostic.code).to_uppercase(),
                diagnostic.message.as_str(),
                start,
                end,
            )
        })
        .collect::<Vec<_>>();
    if !fatal.is_empty() || !lowered.transform_diagnostics.is_empty() {
        let mut diagnostics = fatal;
        diagnostics.extend(lowered.transform_diagnostics.iter().map(|message| {
            source_diagnostic(
                source,
                &options.filename,
                "SYRINX_RSX_TEMPLATE_LOWERING",
                message,
                template.loc.start,
                template.loc.end,
            )
        }));
        return Err(SyrinxCompileFailure { diagnostics });
    }
    let modifier_diagnostics = validate_event_modifiers(
        source,
        &options.filename,
        template.loc.start,
        &lowered.root.children,
    );
    if !modifier_diagnostics.is_empty() {
        return Err(SyrinxCompileFailure {
            diagnostics: modifier_diagnostics,
        });
    }
    let dynamic_component_prop_diagnostics = validate_dynamic_component_props(
        source,
        &options.filename,
        template.loc.start,
        &lowered.root.children,
    );
    if !dynamic_component_prop_diagnostics.is_empty() {
        return Err(SyrinxCompileFailure {
            diagnostics: dynamic_component_prop_diagnostics,
        });
    }
    let component_name = options
        .component_name
        .clone()
        .unwrap_or_else(|| component_name_from_filename(&options.filename));
    let scope_token = descriptor
        .styles
        .iter()
        .any(|style| style.scoped)
        .then(|| format!("data-v-syrinx-{}", &sha256(source.as_bytes())[..8]));
    if scope_token.is_some() {
        let diagnostics = validate_scoped_component_boundaries(
            source,
            &options.filename,
            template.loc.start,
            &lowered.root.children,
        );
        if !diagnostics.is_empty() {
            return Err(SyrinxCompileFailure { diagnostics });
        }
    }
    let css = compile_rsx_styles(
        source,
        &options.filename,
        &descriptor,
        scope_token.as_deref(),
    )?;
    let mut script_lowering = ScriptLowering::parse(script_content);
    let template_ref_diagnostics = prepare_template_refs(
        source,
        &options.filename,
        template.loc.start,
        &lowered.root.children,
        false,
        &mut script_lowering,
    );
    if !template_ref_diagnostics.is_empty() {
        return Err(SyrinxCompileFailure {
            diagnostics: template_ref_diagnostics,
        });
    }
    let reactive_bindings = script_lowering
        .refs
        .keys()
        .chain(script_lowering.computed.keys())
        .cloned()
        .collect::<Vec<_>>();
    let mut emitter = RsxEmitter::new(component_name, script_lowering, scope_token);
    emitter.emit_children(&lowered.root.children, Scope::Root);
    let mut artifact = emitter.finish(css);
    for hook in &mut artifact.expression_hooks {
        hook.start_byte += template.loc.start as u32;
        hook.end_byte += template.loc.start as u32;
    }
    let unsupported_reactive = artifact.expression_hooks.iter().find_map(|hook| {
        reactive_bindings
            .iter()
            .find(|name| {
                contains_identifier(&hook.expression, name) && !hook.field.starts_with("native_")
            })
            .map(|name| (hook, name))
    });
    if let Some((hook, name)) = unsupported_reactive {
        return Err(file_failure(
            source,
            &options.filename,
            "SYRINX_RSX_REACTIVE_LOWERING_REQUIRED",
            &format!(
                "reactive binding `{name}` has no native Rust lowering for `{}`",
                hook.expression
            ),
            hook.start_byte as usize,
            hook.end_byte as usize,
        ));
    }
    Ok(artifact)
}

fn compile_rsx_styles(
    source: &str,
    filename: &str,
    descriptor: &SfcDescriptor<'_>,
    scope_token: Option<&str>,
) -> Result<String, SyrinxCompileFailure> {
    let mut compiled = Vec::with_capacity(descriptor.styles.len());
    for style in &descriptor.styles {
        if style.src.is_some() || style.lang.as_deref().is_some_and(|lang| lang != "css") {
            return Err(file_failure(
                source,
                filename,
                "SYRINX_RSX_UNSUPPORTED_STYLE_SOURCE",
                "External and preprocessed styles must be resolved before RSX compilation.",
                style.loc.tag_start,
                style.loc.tag_end,
            ));
        }
        let css = compile_style(
            style,
            &StyleCompileOptions {
                id: scope_token.unwrap_or("data-v-syrinx").to_owned().into(),
                scoped: style.scoped,
                trim: true,
                source_map: true,
                ..Default::default()
            },
        )
        .map_err(|error| {
            file_failure(
                source,
                filename,
                error
                    .code
                    .as_deref()
                    .unwrap_or("SYRINX_RSX_STYLE_COMPILE_ERROR"),
                error.message.as_str(),
                style.loc.start,
                style.loc.end,
            )
        })?;
        let css = if style.scoped {
            rewrite_scoped_keyframes(css.trim(), scope_token.expect("scoped styles have a token"))
                .map_err(|message| {
                file_failure(
                    source,
                    filename,
                    "SYRINX_RSX_SCOPED_KEYFRAMES_UNSUPPORTED",
                    message,
                    style.loc.start,
                    style.loc.end,
                )
            })?
        } else {
            css.trim().to_owned()
        };
        compiled.push(css);
    }
    Ok(compiled.join("\n"))
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug)]
struct CssReplacement {
    start: usize,
    end: usize,
    value: String,
}

fn rewrite_scoped_keyframes(css: &str, scope_token: &str) -> Result<String, &'static str> {
    let bytes = css.as_bytes();
    let suffix = scope_token.strip_prefix("data-v-").unwrap_or(scope_token);
    let mut names = BTreeMap::new();
    let mut replacements = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = css_comment_end(bytes, index) {
            index = end;
            continue;
        }
        if let Some(end) = css_string_end(bytes, index) {
            index = end;
            continue;
        }
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }
        let at_rule_start = index;
        index += 1;
        while index < bytes.len() && is_css_identifier_byte(bytes[index]) {
            index += 1;
        }
        let at_rule = css[at_rule_start + 1..index].to_ascii_lowercase();
        let is_keyframes =
            at_rule == "keyframes" || (at_rule.starts_with('-') && at_rule.ends_with("-keyframes"));
        if !is_keyframes {
            continue;
        }
        index = skip_css_trivia(bytes, index);
        let name_start = index;
        while index < bytes.len() && is_css_identifier_byte(bytes[index]) {
            index += 1;
        }
        if name_start == index {
            return Err("Scoped keyframe names must be static CSS identifiers.");
        }
        let name = &css[name_start..index];
        let body_start = skip_css_trivia(bytes, index);
        if bytes.get(body_start) != Some(&b'{') {
            return Err("Escaped and quoted scoped keyframe names are not supported.");
        }
        let scoped_name = format!("{name}-{suffix}");
        names.insert(name.to_owned(), scoped_name.clone());
        replacements.push(CssReplacement {
            start: name_start,
            end: index,
            value: scoped_name,
        });
    }
    if names.is_empty() {
        return Ok(css.to_owned());
    }

    let mut declaration_start = 0;
    let mut animation_value = false;
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    index = 0;
    while index < bytes.len() {
        if let Some(end) = css_comment_end(bytes, index) {
            index = end;
            continue;
        }
        if let Some(end) = css_string_end(bytes, index) {
            index = end;
            continue;
        }
        match bytes[index] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' | b';' | b'}' if paren_depth == 0 && bracket_depth == 0 => {
                declaration_start = index + 1;
                animation_value = false;
            }
            b':' if paren_depth == 0 && bracket_depth == 0 && !animation_value => {
                let property = css[declaration_start..index].trim().to_ascii_lowercase();
                animation_value = property == "animation"
                    || property == "animation-name"
                    || (property.starts_with('-')
                        && (property.ends_with("-animation")
                            || property.ends_with("-animation-name")));
            }
            byte if animation_value && is_css_identifier_byte(byte) => {
                let name_start = index;
                while index < bytes.len() && is_css_identifier_byte(bytes[index]) {
                    index += 1;
                }
                let name = &css[name_start..index];
                if let Some(scoped_name) = names.get(name) {
                    replacements.push(CssReplacement {
                        start: name_start,
                        end: index,
                        value: scoped_name.clone(),
                    });
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    replacements.sort_by_key(|replacement| replacement.start);
    replacements.dedup_by_key(|replacement| replacement.start);
    let mut output = String::with_capacity(css.len() + replacements.len() * suffix.len());
    let mut cursor = 0;
    for replacement in replacements {
        if replacement.start < cursor {
            continue;
        }
        output.push_str(&css[cursor..replacement.start]);
        output.push_str(&replacement.value);
        cursor = replacement.end;
    }
    output.push_str(&css[cursor..]);
    Ok(output)
}

fn css_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start..start + 2) != Some(&b"/*"[..]) {
        return None;
    }
    let mut index = start + 2;
    while index + 1 < bytes.len() {
        if bytes[index..index + 2] == *b"*/" {
            return Some(index + 2);
        }
        index += 1;
    }
    Some(bytes.len())
}

fn css_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return Some(index + 1);
        } else {
            index += 1;
        }
    }
    Some(bytes.len())
}

fn skip_css_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let Some(end) = css_comment_end(bytes, index) else {
            return index;
        };
        index = end;
    }
}

fn is_css_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn sfc_failure(
    source: &str,
    filename: &str,
    error: vize_atelier_sfc::SfcError,
) -> SyrinxCompileFailure {
    let (start, end) = error
        .loc
        .as_ref()
        .map_or((0, source.len()), |loc| (loc.start, loc.end));
    file_failure(
        source,
        filename,
        error.code.as_deref().unwrap_or("SYRINX_RSX_SFC_PARSE"),
        &error.message,
        start,
        end,
    )
}

fn file_failure(
    source: &str,
    filename: &str,
    code: &str,
    message: &str,
    start: usize,
    end: usize,
) -> SyrinxCompileFailure {
    SyrinxCompileFailure {
        diagnostics: vec![source_diagnostic(
            source, filename, code, message, start, end,
        )],
    }
}

fn source_diagnostic(
    source: &str,
    filename: &str,
    code: &str,
    message: &str,
    start: usize,
    end: usize,
) -> SyrinxDiagnostic {
    let start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    let (start_line, start_column) = line_column(source, start);
    let (end_line, end_column) = line_column(source, end);
    SyrinxDiagnostic {
        code: code.to_owned(),
        message: message.to_owned(),
        source: filename.to_owned(),
        start_byte: start as u32,
        end_byte: end as u32,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn line_column(source: &str, offset: usize) -> (u32, u32) {
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1) as u32
        + 1;
    (line, column)
}

impl RsxEmitter {
    fn new(component_name: String, script: ScriptLowering, scope_token: Option<String>) -> Self {
        let component_name = rust_type_name(&component_name);
        Self {
            model_name: format!("{component_name}Model"),
            component_name,
            root_fields: Vec::new(),
            lists: Vec::new(),
            script,
            scope_token,
            used_bindings: BTreeSet::new(),
            hooks: Vec::new(),
            next_field: 1,
            indent: 2,
            body: String::new(),
        }
    }

    fn finish(self, css: String) -> SyrinxRsxArtifact {
        let mut source = String::new();
        writeln!(source, "// @generated by vize_atelier_syrinx; do not edit.").unwrap();
        writeln!(source, "use dioxus::prelude::*;").unwrap();
        if self
            .script
            .refs
            .values()
            .any(|binding| binding.kind == RefKind::MountedData)
        {
            writeln!(source, "use std::rc::Rc;").unwrap();
        }
        writeln!(source).unwrap();
        for list in &self.lists {
            writeln!(source, "/// Render data for one `{}` item.", list.source).unwrap();
            writeln!(source, "#[derive(Clone, PartialEq)]").unwrap();
            writeln!(source, "pub struct {} {{", list.item_type).unwrap();
            for field in &list.data_fields {
                writeln!(source, "    pub {field}: String,").unwrap();
            }
            for field in &list.fields {
                writeln!(
                    source,
                    "    /// Vue expression: `{}`\n    pub {}: {},",
                    doc_expression(&field.expression),
                    field.name,
                    field.kind.rust_type()
                )
                .unwrap();
            }
            writeln!(source, "}}\n").unwrap();
        }
        writeln!(source, "#[derive(Clone, Props, PartialEq)]").unwrap();
        writeln!(source, "pub struct {} {{", self.model_name).unwrap();
        for field in &self.root_fields {
            writeln!(
                source,
                "    /// Vue expression: `{}`\n    pub {}: {},",
                doc_expression(&field.expression),
                field.name,
                field.kind.rust_type()
            )
            .unwrap();
        }
        writeln!(source, "}}\n").unwrap();
        writeln!(
            source,
            "#[allow(non_snake_case)]\npub fn {}(model: {}Model) -> Element {{",
            self.component_name, self.component_name
        )
        .unwrap();
        for (name, binding) in &self.script.refs {
            if self.used_bindings.contains(name) {
                writeln!(
                    source,
                    "    let mut {name} = use_signal(|| {}); // {}",
                    binding.initial,
                    binding.kind.rust_type()
                )
                .unwrap();
            }
        }
        for (name, expression) in &self.script.computed {
            if self.used_bindings.contains(name) {
                let rust = computed_rust_expression(expression, &self.script.refs)
                    .expect("only supported computed bindings are marked as compiled");
                writeln!(source, "    let {name} = {rust};").unwrap();
            }
        }
        writeln!(source, "    rsx! {{").unwrap();
        source.push_str(&self.body);
        writeln!(source, "    }}\n}}").unwrap();
        SyrinxRsxArtifact {
            component_name: self.component_name,
            rust_source: source,
            css,
            compiled_bindings: self.used_bindings.into_iter().collect(),
            expression_hooks: self.hooks,
        }
    }

    fn emit_children(&mut self, children: &[TemplateChildNode<'_>], scope: Scope) {
        for child in children {
            self.emit_child(child, scope);
        }
    }

    fn emit_child(&mut self, child: &TemplateChildNode<'_>, scope: Scope) {
        match child {
            TemplateChildNode::Element(element) => self.emit_element(element, scope),
            TemplateChildNode::Text(text) => {
                if !text.content.trim().is_empty() {
                    self.line(&format!("{:?}", text.content.as_str()));
                }
            }
            TemplateChildNode::Comment(_) => {}
            TemplateChildNode::Interpolation(interpolation) => {
                let expression = expression_content(&interpolation.content);
                let value = self.add_field(
                    scope,
                    FieldKind::Text,
                    "text",
                    expression,
                    "text",
                    interpolation.content.loc(),
                );
                self.line(&format!("{{{}}}", owned_render_value(&value)));
            }
            TemplateChildNode::If(node) => {
                for (index, branch) in node.branches.iter().enumerate() {
                    if let Some(condition) = branch.condition.as_ref() {
                        let expression = expression_content(condition);
                        let value = self.add_field(
                            scope,
                            FieldKind::Condition,
                            "condition",
                            expression,
                            "condition",
                            condition.loc(),
                        );
                        if index == 0 {
                            self.line(&format!("if {value} {{"));
                        } else {
                            self.line(&format!("else if {value} {{"));
                        }
                    } else {
                        self.line("else {");
                    }
                    self.indent += 1;
                    self.emit_children(&branch.children, scope);
                    self.indent -= 1;
                    self.line("}");
                }
            }
            TemplateChildNode::IfBranch(branch) => self.emit_children(&branch.children, scope),
            TemplateChildNode::For(node) => self.emit_for(node, scope),
            TemplateChildNode::TextCall(call) => match &call.content {
                TextCallContent::Text(text) => {
                    if !text.content.trim().is_empty() {
                        self.line(&format!("{:?}", text.content.as_str()));
                    }
                }
                TextCallContent::Interpolation(interpolation) => {
                    let expression = expression_content(&interpolation.content);
                    let value = self.add_field(
                        scope,
                        FieldKind::Text,
                        "text",
                        expression,
                        "text",
                        interpolation.content.loc(),
                    );
                    self.line(&format!("{{{}}}", owned_render_value(&value)));
                }
                TextCallContent::Compound(compound) => {
                    let expression = compound.loc.source.to_string();
                    let value = self.add_field(
                        scope,
                        FieldKind::Text,
                        "text",
                        expression,
                        "text",
                        &compound.loc,
                    );
                    self.line(&format!("{{{}}}", owned_render_value(&value)));
                }
            },
            TemplateChildNode::CompoundExpression(compound) => {
                let expression = compound.loc.source.to_string();
                let value = self.add_field(
                    scope,
                    FieldKind::Text,
                    "text",
                    expression,
                    "text",
                    &compound.loc,
                );
                self.line(&format!("{{{}}}", owned_render_value(&value)));
            }
            TemplateChildNode::Hoisted(_) => {
                self.line("// Static hoist is materialized by the RSX frontend.");
            }
        }
    }

    fn emit_element(&mut self, element: &ElementNode<'_>, scope: Scope) {
        if element.tag_type == ElementType::Template {
            self.emit_children(&element.children, scope);
            return;
        }
        if element.tag_type == ElementType::Slot || element.tag.as_str() == "slot" {
            let value = self.add_field(
                scope,
                FieldKind::Slot,
                "slot",
                slot_name(element),
                "slot",
                &element.loc,
            );
            self.line(&format!("{{{}}}", owned_render_value(&value)));
            return;
        }

        let tag = if element.tag_type == ElementType::Component {
            rust_type_name(element.tag.as_str())
        } else {
            rust_ident(element.tag.as_str())
        };
        self.line(&format!("{tag} {{"));
        self.indent += 1;
        if element.tag_type != ElementType::Component
            && let Some(scope_token) = self.scope_token.clone()
        {
            self.line(&format!("{scope_token:?}: \"true\","));
        }
        if let Some(template_ref) = element_template_ref(element) {
            self.used_bindings.insert(template_ref.clone());
            self.line(&format!(
                "onmounted: move |event| {template_ref}.set(Some(event.data())),"
            ));
        }
        let mut dynamic_attribute_spreads = Vec::new();
        for prop in element.props.iter() {
            if is_template_ref_prop(prop) {
                continue;
            }
            match prop {
                PropNode::Attribute(attribute) => {
                    let name = rsx_attribute_name(attribute.name.as_str());
                    let value = attribute
                        .value
                        .as_ref()
                        .map(|value| value.content.as_str())
                        .unwrap_or("true");
                    self.line(&format!("{name}: {value:?},"));
                }
                PropNode::Directive(directive) if directive.name.as_str() == "bind" => {
                    let Some(argument) = directive.arg.as_ref() else {
                        let expression = directive
                            .exp
                            .as_ref()
                            .map(expression_content)
                            .unwrap_or_default();
                        let dynamic = self.add_field(
                            scope,
                            FieldKind::DynamicAttributes,
                            "dynamic_attributes",
                            expression,
                            "dynamic-attributes",
                            directive
                                .exp
                                .as_ref()
                                .map(ExpressionNode::loc)
                                .unwrap_or(&directive.loc),
                        );
                        dynamic_attribute_spreads.push(dynamic);
                        continue;
                    };
                    let name = expression_content(argument);
                    let expression = directive
                        .exp
                        .as_ref()
                        .map(expression_content)
                        .unwrap_or_else(|| name.clone());
                    if !expression_is_static(argument) {
                        let dynamic = self.add_field(
                            scope,
                            FieldKind::DynamicAttributes,
                            "dynamic_attributes",
                            format!("[{{ name: {name}, value: {expression} }}]"),
                            "dynamic-attributes",
                            &directive.loc,
                        );
                        dynamic_attribute_spreads.push(dynamic);
                        continue;
                    }
                    let role = if name == "style" {
                        "style".to_owned()
                    } else {
                        format!("attribute:{name}")
                    };
                    let value = self.add_field(
                        scope,
                        FieldKind::Attribute,
                        match name.as_str() {
                            "key" => "key",
                            "style" => "style",
                            _ => "attribute",
                        },
                        expression,
                        &role,
                        directive
                            .exp
                            .as_ref()
                            .map(ExpressionNode::loc)
                            .unwrap_or(&directive.loc),
                    );
                    let name = rsx_attribute_name(&name);
                    if name == "key" {
                        // Dioxus requires keys to be formatted strings. Avoid
                        // cloning inside the interpolation: the loop item is
                        // already borrowed and Display only needs a reference.
                        let value = value.strip_suffix(".clone()").unwrap_or(&value);
                        self.line(&format!("key: \"{{{value}}}\","));
                    } else {
                        self.line(&format!("{name}: {},", owned_render_value(&value)));
                    }
                }
                PropNode::Directive(directive) if directive.name.as_str() == "on" => {
                    let event = directive
                        .arg
                        .as_ref()
                        .map(expression_content)
                        .unwrap_or_else(|| "click".to_owned());
                    let expression = directive
                        .exp
                        .as_ref()
                        .map(expression_content)
                        .unwrap_or_default();
                    let handler_event_type = if element.tag_type == ElementType::Component {
                        "()"
                    } else {
                        event_type(&event).expect("event names are validated before RSX emission")
                    };
                    let value = self.add_field(
                        scope,
                        FieldKind::Event(handler_event_type),
                        "event",
                        expression,
                        &format!("event:{event}"),
                        directive
                            .exp
                            .as_ref()
                            .map(ExpressionNode::loc)
                            .unwrap_or(&directive.loc),
                    );
                    let stop = directive
                        .modifiers
                        .iter()
                        .any(|modifier| modifier.content.as_str() == "stop");
                    let prevent = directive
                        .modifiers
                        .iter()
                        .any(|modifier| modifier.content.as_str() == "prevent");
                    if stop || prevent {
                        self.line(&format!("on{}: {{", rust_ident(&event)));
                        self.indent += 1;
                        self.line(&format!("let handler = {value};"));
                        self.line("move |event| {");
                        self.indent += 1;
                        if stop {
                            self.line("event.stop_propagation();");
                        }
                        if prevent {
                            self.line("event.prevent_default();");
                        }
                        self.line("handler(event);");
                        self.indent -= 1;
                        self.line("}");
                        self.indent -= 1;
                        self.line("},");
                    } else {
                        self.line(&format!("on{}: {value},", rust_ident(&event)));
                    }
                }
                PropNode::Directive(_) => {}
            }
        }
        for dynamic in dynamic_attribute_spreads {
            self.line(&format!("..{dynamic}.clone(),"));
        }
        self.emit_children(&element.children, scope);
        self.indent -= 1;
        self.line("}");
    }

    fn emit_for(&mut self, node: &vize_atelier_core::ForNode<'_>, parent: Scope) {
        let id = self.lists.len() as u32 + 1;
        let source = expression_content(&node.source);
        let component = self.component_name.clone();
        let index = self.lists.len();
        let field = format!("list_{id}");
        let item_type = format!("{component}List{id}Item");
        self.lists.push(ListModel {
            id,
            item_type: item_type.clone(),
            source: source.clone(),
            value_alias: node.value_alias.as_ref().map(expression_content),
            index_alias: node.key_alias.as_ref().map(expression_content),
            uses_index: false,
            data_fields: BTreeSet::new(),
            fields: Vec::new(),
        });
        let model_field = ModelField {
            name: field.clone(),
            kind: FieldKind::List(item_type),
            expression: source.clone(),
        };
        let reference = match parent {
            Scope::Root => {
                self.root_fields.push(model_field);
                format!("model.{field}")
            }
            Scope::List(parent_index) => {
                let parent_id = self.lists[parent_index].id;
                self.lists[parent_index].fields.push(model_field);
                format!("item_{parent_id}.{field}")
            }
        };
        self.hooks.push(RsxExpressionHook {
            field,
            role: "list".to_owned(),
            expression: source,
            start_byte: node.source.loc().start.offset,
            end_byte: node.source.loc().end.offset,
        });
        let marker = format!("__SYRINX_FOR_{id}__");
        self.line(&marker);
        self.indent += 1;
        self.emit_children(&node.children, Scope::List(index));
        self.indent -= 1;
        self.line("}");
        let header = if self.lists[index].uses_index {
            format!("for (_index_{id}, item_{id}) in {reference}.iter().enumerate() {{")
        } else {
            format!("for item_{id} in {reference}.iter() {{")
        };
        self.body = self.body.replacen(&marker, &header, 1);
    }

    fn add_field(
        &mut self,
        scope: Scope,
        kind: FieldKind,
        prefix: &str,
        expression: String,
        role: &str,
        location: &SourceLocation,
    ) -> String {
        if let Some(reference) = self.try_native_expression(scope, &kind, &expression) {
            let id = self.next_field;
            self.next_field += 1;
            self.hooks.push(RsxExpressionHook {
                field: format!("native_{id}"),
                role: role.to_owned(),
                expression,
                start_byte: location.start.offset,
                end_byte: location.end.offset,
            });
            return reference;
        }
        let id = self.next_field;
        self.next_field += 1;
        let name = format!("{prefix}_{id}");
        let field = ModelField {
            name: name.clone(),
            kind,
            expression: expression.clone(),
        };
        let reference = match scope {
            Scope::Root => {
                self.root_fields.push(field);
                format!("model.{name}")
            }
            Scope::List(index) => {
                let list_id = self.lists[index].id;
                self.lists[index].fields.push(field);
                format!("item_{list_id}.{name}")
            }
        };
        self.hooks.push(RsxExpressionHook {
            field: name,
            role: role.to_owned(),
            expression,
            start_byte: location.start.offset,
            end_byte: location.end.offset,
        });
        reference
    }

    fn try_native_expression(
        &mut self,
        scope: Scope,
        kind: &FieldKind,
        expression: &str,
    ) -> Option<String> {
        let expression = expression.trim();
        for (name, binding) in &self.script.refs {
            if expression == name || expression == format!("{name}.value") {
                self.used_bindings.insert(name.clone());
                return match (binding.kind, kind) {
                    (RefKind::OptionalString, FieldKind::Condition) => {
                        Some(format!("{name}.read().is_some()"))
                    }
                    (RefKind::OptionalString, FieldKind::Text | FieldKind::Attribute) => {
                        Some(format!("{name}.read().clone().unwrap_or_default()"))
                    }
                    (RefKind::Bool, FieldKind::Condition) => Some(format!("*{name}.read()")),
                    (RefKind::Bool, FieldKind::Text | FieldKind::Attribute) => {
                        Some(format!("{name}.read().to_string()"))
                    }
                    (RefKind::String, FieldKind::Condition) => {
                        Some(format!("!{name}.read().is_empty()"))
                    }
                    (RefKind::String, FieldKind::Text | FieldKind::Attribute) => {
                        Some(format!("{name}.read().clone()"))
                    }
                    _ => None,
                };
            }
        }
        for (name, computed) in &self.script.computed {
            if (expression == name || expression == format!("{name}.value"))
                && computed_rust_expression(computed, &self.script.refs).is_some()
            {
                self.used_bindings.insert(name.clone());
                for dependency in self.script.refs.keys() {
                    if computed.contains(dependency) {
                        self.used_bindings.insert(dependency.clone());
                    }
                }
                return Some(format!("{name}.clone()"));
            }
        }

        let Scope::List(list_index) = scope else {
            return None;
        };
        let list_id = self.lists[list_index].id;
        let value_alias = self.lists[list_index].value_alias.clone()?;
        let index_alias = self.lists[list_index].index_alias.clone();
        let item = format!("item_{list_id}");

        if let Some(property) = expression.strip_prefix(&format!("{value_alias}."))
            && is_rust_identifier(property)
            && !matches!(kind, FieldKind::Condition)
        {
            self.lists[list_index]
                .data_fields
                .insert(property.to_owned());
            return Some(format!("{item}.{property}.clone()"));
        }

        for formatter in &self.script.formatters {
            let prefix = format!("{formatter}({value_alias},");
            if expression.starts_with(&prefix) && expression.ends_with(')') {
                self.lists[list_index]
                    .data_fields
                    .insert("label".to_owned());
                self.used_bindings.insert(formatter.clone());
                self.lists[list_index].uses_index = true;
                let index = index_alias.as_ref().map_or_else(
                    || format!("_index_{list_id}"),
                    |_| format!("_index_{list_id}"),
                );
                return Some(format!(
                    "format!(\"{{}}: {{}}\", {index} + 1, {item}.label)"
                ));
            }
        }

        if let Some((state, property, base, active)) =
            lower_list_class_expression(expression, &value_alias, &self.script.refs)
        {
            self.used_bindings.insert(state.clone());
            self.lists[list_index].data_fields.insert(property.clone());
            return Some(format!(
                "if {state}.read().as_deref() == Some({item}.{property}.as_str()) {{ {active:?}.to_owned() }} else {{ {base:?}.to_owned() }}"
            ));
        }

        if expression.contains("color:") && expression.contains(&format!("{value_alias}.id")) {
            let state = self
                .script
                .refs
                .iter()
                .find(|(name, binding)| {
                    binding.kind == RefKind::OptionalString && expression.contains(name.as_str())
                })?
                .0
                .clone();
            self.used_bindings.insert(state.clone());
            self.lists[list_index].data_fields.insert("id".to_owned());
            return Some(format!(
                "if {state}.read().as_deref() == Some({item}.id.as_str()) {{ \"color: gold;\".to_owned() }} else {{ \"color: inherit;\".to_owned() }}"
            ));
        }

        if matches!(kind, FieldKind::Event(_)) {
            let action = expression
                .strip_prefix("$event => (")
                .and_then(|value| value.strip_suffix(')'))
                .unwrap_or(expression)
                .trim();
            for (state, binding) in &self.script.refs {
                if binding.kind != RefKind::OptionalString {
                    continue;
                }
                let assignment = action
                    .strip_prefix(&format!("{state}.value = {value_alias}."))
                    .or_else(|| action.strip_prefix(&format!("{state} = {value_alias}.")));
                if let Some(property) = assignment.filter(|value| is_rust_identifier(value)) {
                    self.used_bindings.insert(state.clone());
                    self.lists[list_index]
                        .data_fields
                        .insert(property.to_owned());
                    return Some(format!(
                        "{{ let value = {item}.{property}.clone(); move |_| {state}.set(Some(value.clone())) }}"
                    ));
                }
            }
            for (setter, state) in &self.script.setters {
                let property = action
                    .strip_prefix(&format!("{setter}({value_alias}."))
                    .and_then(|value| value.strip_suffix(')'))
                    .filter(|value| is_rust_identifier(value));
                if let Some(property) = property
                    && self
                        .script
                        .refs
                        .get(state)
                        .is_some_and(|binding| binding.kind == RefKind::OptionalString)
                {
                    let state = state.clone();
                    self.used_bindings.insert(setter.clone());
                    self.used_bindings.insert(state.clone());
                    self.lists[list_index]
                        .data_fields
                        .insert(property.to_owned());
                    return Some(format!(
                        "{{ let value = {item}.{property}.clone(); move |_| {state}.set(Some(value.clone())) }}"
                    ));
                }
            }
        }
        None
    }

    fn line(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.body.push_str("    ");
        }
        self.body.push_str(line);
        self.body.push('\n');
    }
}

fn validate_event_modifiers(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    collect_event_modifier_diagnostics(
        source,
        filename,
        template_start,
        children,
        &mut diagnostics,
    );
    diagnostics
}

fn prepare_template_refs(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
    in_for: bool,
    script: &mut ScriptLowering,
) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen_bindings = BTreeSet::new();
    collect_template_ref_diagnostics(
        source,
        filename,
        template_start,
        children,
        in_for,
        false,
        script,
        &mut seen_bindings,
        &mut diagnostics,
    );
    diagnostics
}

fn validate_scoped_component_boundaries(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    collect_scoped_component_diagnostics(
        source,
        filename,
        template_start,
        children,
        &mut diagnostics,
    );
    diagnostics
}

fn validate_dynamic_component_props(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
) -> Vec<SyrinxDiagnostic> {
    let mut diagnostics = Vec::new();
    collect_dynamic_component_prop_diagnostics(
        source,
        filename,
        template_start,
        children,
        &mut diagnostics,
    );
    diagnostics
}

fn collect_dynamic_component_prop_diagnostics(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                if element.tag_type == ElementType::Component {
                    for directive in element.props.iter().filter_map(|prop| match prop {
                        PropNode::Directive(directive) if directive.name.as_str() == "bind" => {
                            Some(directive)
                        }
                        _ => None,
                    }) {
                        if directive
                            .arg
                            .as_ref()
                            .is_none_or(|argument| !expression_is_static(argument))
                        {
                            diagnostics.push(source_diagnostic(
                                source,
                                filename,
                                "SYRINX_RSX_DYNAMIC_COMPONENT_PROP_UNSUPPORTED",
                                "Dynamic component prop names cannot be proven against a static Rust Props type.",
                                template_start + directive.loc.start.offset as usize,
                                template_start + directive.loc.end.offset as usize,
                            ));
                        }
                    }
                }
                collect_dynamic_component_prop_diagnostics(
                    source,
                    filename,
                    template_start,
                    &element.children,
                    diagnostics,
                );
            }
            TemplateChildNode::If(node) => {
                for branch in &node.branches {
                    collect_dynamic_component_prop_diagnostics(
                        source,
                        filename,
                        template_start,
                        &branch.children,
                        diagnostics,
                    );
                }
            }
            TemplateChildNode::For(node) => collect_dynamic_component_prop_diagnostics(
                source,
                filename,
                template_start,
                &node.children,
                diagnostics,
            ),
            _ => {}
        }
    }
}

fn collect_scoped_component_diagnostics(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                if element.tag_type == ElementType::Component {
                    diagnostics.push(source_diagnostic(
                        source,
                        filename,
                        "SYRINX_RSX_SCOPED_COMPONENT_BOUNDARY",
                        "Scoped parent styles require forwarding the scope attribute to the child component root.",
                        template_start + element.loc.start.offset as usize,
                        template_start + element.loc.end.offset as usize,
                    ));
                }
                collect_scoped_component_diagnostics(
                    source,
                    filename,
                    template_start,
                    &element.children,
                    diagnostics,
                );
            }
            TemplateChildNode::If(node) => {
                for branch in &node.branches {
                    collect_scoped_component_diagnostics(
                        source,
                        filename,
                        template_start,
                        &branch.children,
                        diagnostics,
                    );
                }
            }
            TemplateChildNode::For(node) => collect_scoped_component_diagnostics(
                source,
                filename,
                template_start,
                &node.children,
                diagnostics,
            ),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_template_ref_diagnostics(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
    in_for: bool,
    conditional: bool,
    script: &mut ScriptLowering,
    seen_bindings: &mut BTreeSet<String>,
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                let refs = element
                    .props
                    .iter()
                    .filter_map(template_ref_binding)
                    .collect::<Vec<_>>();
                for (name, location) in &refs {
                    let absolute_start = template_start + location.start.offset as usize;
                    let absolute_end = template_start + location.end.offset as usize;
                    if element.tag_type == ElementType::Component {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_COMPONENT_REF_UNSUPPORTED",
                            "Component refs expose a component instance rather than renderer-neutral mounted element data.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if in_for {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_FOR_UNSUPPORTED",
                            "Template refs inside v-for require an ordered mounted-node collection.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if conditional {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_CONDITIONAL_UNSUPPORTED",
                            "Conditional template refs require clearing mounted data when the element unmounts.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if refs.len() > 1 {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_DUPLICATE_TEMPLATE_REF",
                            "An element can lower exactly one Vue template ref.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if element.props.iter().any(is_mounted_event_prop) {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_MOUNT_CONFLICT",
                            "A template ref and an authored mounted handler cannot share one Dioxus onmounted slot.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if !is_rust_identifier(name) {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_BINDING_REQUIRED",
                            "A template ref must name one ref(null) setup binding.",
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    if !seen_bindings.insert(name.clone()) {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_DUPLICATE_TEMPLATE_REF_BINDING",
                            &format!("Template ref `{name}` is mounted by more than one element."),
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    let Some(binding) = script.refs.get_mut(name) else {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_BINDING_REQUIRED",
                            &format!(
                                "Template ref `{name}` must be declared as ref(null) in <script setup>."
                            ),
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    };
                    if binding.kind != RefKind::OptionalString
                        && binding.kind != RefKind::MountedData
                    {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_BINDING_REQUIRED",
                            &format!("Template ref `{name}` must be initialized with ref(null)."),
                            absolute_start,
                            absolute_end,
                        ));
                        continue;
                    }
                    binding.kind = RefKind::MountedData;
                    binding.initial = "None::<Rc<MountedData>>".to_owned();
                }
                for prop in &element.props {
                    if is_ref_collection_prop(prop) {
                        let location = prop.loc();
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_TEMPLATE_REF_FOR_UNSUPPORTED",
                            "ref_for/ref_key require an ordered mounted-node collection.",
                            template_start + location.start.offset as usize,
                            template_start + location.end.offset as usize,
                        ));
                    }
                }
                collect_template_ref_diagnostics(
                    source,
                    filename,
                    template_start,
                    &element.children,
                    in_for,
                    conditional,
                    script,
                    seen_bindings,
                    diagnostics,
                );
            }
            TemplateChildNode::If(node) => {
                for branch in &node.branches {
                    collect_template_ref_diagnostics(
                        source,
                        filename,
                        template_start,
                        &branch.children,
                        in_for,
                        true,
                        script,
                        seen_bindings,
                        diagnostics,
                    );
                }
            }
            TemplateChildNode::For(node) => collect_template_ref_diagnostics(
                source,
                filename,
                template_start,
                &node.children,
                true,
                conditional,
                script,
                seen_bindings,
                diagnostics,
            ),
            _ => {}
        }
    }
}

fn template_ref_binding(prop: &PropNode<'_>) -> Option<(String, SourceLocation)> {
    match prop {
        PropNode::Attribute(attribute) if attribute.name.as_str() == "ref" => attribute
            .value
            .as_ref()
            .map(|value| (value.content.to_string(), prop.loc().clone())),
        PropNode::Directive(directive)
            if directive.name.as_str() == "bind"
                && directive.arg.as_ref().is_some_and(|argument| {
                    expression_is_static(argument) && expression_content(argument) == "ref"
                }) =>
        {
            directive.exp.as_ref().map(|expression| {
                (
                    normalized_template_ref_binding(&expression_content(expression)),
                    prop.loc().clone(),
                )
            })
        }
        _ => None,
    }
}

fn normalized_template_ref_binding(expression: &str) -> String {
    let mut expression = expression.trim();
    if let Some(literal) = js_string_literal(expression) {
        return literal;
    }
    expression = expression
        .strip_prefix("$setup.")
        .or_else(|| expression.strip_prefix("_ctx."))
        .unwrap_or(expression);
    expression
        .strip_suffix(".value")
        .unwrap_or(expression)
        .to_owned()
}

fn element_template_ref(element: &ElementNode<'_>) -> Option<String> {
    element
        .props
        .iter()
        .find_map(template_ref_binding)
        .map(|(name, _)| name)
}

fn is_template_ref_prop(prop: &PropNode<'_>) -> bool {
    template_ref_binding(prop).is_some()
}

fn is_ref_collection_prop(prop: &PropNode<'_>) -> bool {
    match prop {
        PropNode::Attribute(attribute) => {
            matches!(attribute.name.as_str(), "ref_for" | "ref_key")
        }
        PropNode::Directive(directive) if directive.name.as_str() == "bind" => {
            directive.arg.as_ref().is_some_and(|argument| {
                expression_is_static(argument)
                    && matches!(expression_content(argument).as_str(), "ref_for" | "ref_key")
            })
        }
        _ => false,
    }
}

fn is_mounted_event_prop(prop: &PropNode<'_>) -> bool {
    matches!(
        prop,
        PropNode::Directive(directive)
            if directive.name.as_str() == "on"
                && directive.arg.as_ref().is_some_and(|argument| {
                    expression_is_static(argument) && expression_content(argument) == "mounted"
                })
    )
}

fn collect_event_modifier_diagnostics(
    source: &str,
    filename: &str,
    template_start: usize,
    children: &[TemplateChildNode<'_>],
    diagnostics: &mut Vec<SyrinxDiagnostic>,
) {
    for child in children {
        match child {
            TemplateChildNode::Element(element) => {
                for directive in element.props.iter().filter_map(|prop| match prop {
                    PropNode::Directive(directive) if directive.name.as_str() == "on" => {
                        Some(directive)
                    }
                    _ => None,
                }) {
                    let Some(argument) = directive.arg.as_ref() else {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_UNSUPPORTED_EVENT_NAME",
                            "Object-form v-on has no statically typed Dioxus event lowering.",
                            template_start + directive.loc.start.offset as usize,
                            template_start + directive.loc.end.offset as usize,
                        ));
                        continue;
                    };
                    if !expression_is_static(argument) {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_UNSUPPORTED_EVENT_NAME",
                            "Dynamic event names have no statically typed Dioxus event lowering.",
                            template_start + argument.loc().start.offset as usize,
                            template_start + argument.loc().end.offset as usize,
                        ));
                        continue;
                    }
                    let event = expression_content(argument);
                    if element.tag_type != ElementType::Component && event_type(&event).is_none() {
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_RSX_UNSUPPORTED_EVENT_NAME",
                            &format!(
                                "Event `{event}` is not exposed by the Dioxus HTML event surface."
                            ),
                            template_start + argument.loc().start.offset as usize,
                            template_start + argument.loc().end.offset as usize,
                        ));
                    }
                    for modifier in &directive.modifiers {
                        if element.tag_type == ElementType::Component {
                            let name = modifier.content.as_str();
                            diagnostics.push(source_diagnostic(
                                source,
                                filename,
                                "SYRINX_UNSUPPORTED_EVENT_MODIFIER",
                                &format!(
                                    "Event modifier .{name} has no typed custom-component event lowering."
                                ),
                                template_start + modifier.loc.start.offset as usize,
                                template_start + modifier.loc.end.offset as usize,
                            ));
                            continue;
                        }
                        if matches!(modifier.content.as_str(), "stop" | "prevent") {
                            continue;
                        }
                        let name = modifier.content.as_str();
                        let message = if name == "self" {
                            "Event modifier .self requires target/current-target identity, which renderer-neutral Dioxus events do not expose."
                                .to_owned()
                        } else {
                            format!(
                                "Event modifier .{name} has no renderer-neutral Dioxus RSX lowering."
                            )
                        };
                        diagnostics.push(source_diagnostic(
                            source,
                            filename,
                            "SYRINX_UNSUPPORTED_EVENT_MODIFIER",
                            &message,
                            template_start + modifier.loc.start.offset as usize,
                            template_start + modifier.loc.end.offset as usize,
                        ));
                    }
                }
                collect_event_modifier_diagnostics(
                    source,
                    filename,
                    template_start,
                    &element.children,
                    diagnostics,
                );
            }
            TemplateChildNode::If(node) => {
                for branch in &node.branches {
                    collect_event_modifier_diagnostics(
                        source,
                        filename,
                        template_start,
                        &branch.children,
                        diagnostics,
                    );
                }
            }
            TemplateChildNode::For(node) => collect_event_modifier_diagnostics(
                source,
                filename,
                template_start,
                &node.children,
                diagnostics,
            ),
            _ => {}
        }
    }
}

fn expression_content(expression: &ExpressionNode<'_>) -> String {
    match expression {
        ExpressionNode::Simple(simple) => simple.content.to_string(),
        ExpressionNode::Compound(compound) => compound.loc.source.to_string(),
    }
}

fn expression_is_static(expression: &ExpressionNode<'_>) -> bool {
    matches!(expression, ExpressionNode::Simple(simple) if simple.is_static)
}

fn slot_name(element: &ElementNode<'_>) -> String {
    element
        .props
        .iter()
        .find_map(|prop| match prop {
            PropNode::Attribute(attribute) if attribute.name.as_str() == "name" => attribute
                .value
                .as_ref()
                .map(|value| value.content.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "default".to_owned())
}

fn rsx_attribute_name(name: &str) -> String {
    match name {
        "class" | "id" | "style" | "key" | "title" | "value" | "name" | "role" => rust_ident(name),
        _ if name.contains('-') => format!("{name:?}"),
        _ => rust_ident(name),
    }
}

fn event_type(event: &str) -> Option<&'static str> {
    match event {
        "animationstart" | "animationend" | "animationiteration" => Some("AnimationEvent"),
        "beforeinput" => Some("BeforeInputEvent"),
        "cancel" => Some("CancelEvent"),
        "copy" | "cut" | "paste" => Some("ClipboardEvent"),
        "compositionstart" | "compositionend" | "compositionupdate" => Some("CompositionEvent"),
        "drag" | "dragend" | "dragenter" | "dragexit" | "dragleave" | "dragover" | "dragstart"
        | "drop" => Some("DragEvent"),
        "focus" | "blur" | "focusin" | "focusout" => Some("FocusEvent"),
        "input" | "change" | "submit" | "reset" | "invalid" => Some("FormEvent"),
        "error" | "load" => Some("ImageEvent"),
        "keydown" | "keypress" | "keyup" => Some("KeyboardEvent"),
        "abort" | "canplay" | "canplaythrough" | "durationchange" | "emptied" | "encrypted"
        | "ended" | "loadeddata" | "loadedmetadata" | "loadstart" | "pause" | "play"
        | "playing" | "progress" | "ratechange" | "seeked" | "seeking" | "stalled" | "suspend"
        | "timeupdate" | "volumechange" | "waiting" => Some("MediaEvent"),
        "mounted" => Some("MountedEvent"),
        "click" | "contextmenu" | "dblclick" | "doubleclick" | "mousedown" | "mouseenter"
        | "mouseleave" | "mousemove" | "mouseout" | "mouseover" | "mouseup" | "auxclick" => {
            Some("MouseEvent")
        }
        "pointercancel" | "pointerdown" | "pointerenter" | "pointerleave" | "pointermove"
        | "pointerout" | "pointerover" | "pointerup" | "gotpointercapture"
        | "lostpointercapture" => Some("PointerEvent"),
        "resize" => Some("ResizeEvent"),
        "scroll" | "scrollend" => Some("ScrollEvent"),
        "select" | "selectstart" | "selectionchange" => Some("SelectionEvent"),
        "toggle" | "beforetoggle" => Some("ToggleEvent"),
        "touchstart" | "touchmove" | "touchend" | "touchcancel" => Some("TouchEvent"),
        "transitionend" => Some("TransitionEvent"),
        "visible" => Some("VisibleEvent"),
        "wheel" => Some("WheelEvent"),
        _ => None,
    }
}

fn owned_render_value(value: &str) -> String {
    let value = value.trim();
    if value.ends_with(".clone()")
        || value.ends_with(".to_owned()")
        || value.ends_with(".to_string()")
        || value.ends_with(".unwrap_or_default()")
        || value.starts_with("format!(")
        || value.starts_with("if ")
        || value.starts_with("{ let ")
    {
        value.to_owned()
    } else {
        format!("({value}).clone()")
    }
}

fn rust_ident(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_alphanumeric() || character == '_' {
            if index == 0 && character.is_ascii_digit() {
                result.push('_');
            }
            result.push(character.to_ascii_lowercase());
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        "node".to_owned()
    } else {
        result
    }
}

fn rust_type_name(value: &str) -> String {
    let mut result = String::new();
    for part in value.split(|character: char| !character.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            result.push(first.to_ascii_uppercase());
            result.extend(characters);
        }
    }
    if result.is_empty() {
        "Component".to_owned()
    } else {
        result
    }
}

fn component_name_from_filename(filename: &str) -> String {
    filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .split('.')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("Component")
        .to_owned()
}

fn doc_expression(expression: &str) -> String {
    expression.replace('`', "\\`").replace(['\n', '\r'], " ")
}

fn js_string_literal(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && matches!(bytes[0], b'\'' | b'"')
        && bytes[0] == *bytes.last().unwrap_or(&0)
    {
        Some(value[1..value.len() - 1].to_owned())
    } else {
        None
    }
}

fn lower_list_class_expression(
    expression: &str,
    value_alias: &str,
    refs: &BTreeMap<String, RefBinding>,
) -> Option<(String, String, String, String)> {
    if let Some((condition, branches)) = expression.split_once(" ? ") {
        let (when_true, when_false) = branches.split_once(" : ")?;
        let (state, property) = list_optional_string_comparison(condition, value_alias, refs)?;
        return Some((
            state,
            property,
            js_string_literal(when_false.trim())?,
            js_string_literal(when_true.trim())?,
        ));
    }

    let object = expression.strip_prefix('{')?.strip_suffix('}')?.trim();
    let mut base = Vec::new();
    let mut conditional = None;
    for entry in object.split(',') {
        let (class, value) = entry.split_once(':')?;
        let class = js_string_literal(class.trim()).unwrap_or_else(|| class.trim().to_owned());
        if value.trim() == "true" {
            base.push(class);
        } else if let Some((state, property)) =
            list_optional_string_comparison(value.trim(), value_alias, refs)
        {
            if conditional.is_some() {
                return None;
            }
            conditional = Some((state, property, class));
        } else {
            return None;
        }
    }
    let (state, property, active_class) = conditional?;
    let base = base.join(" ");
    let active = if base.is_empty() {
        active_class
    } else {
        format!("{base} {active_class}")
    };
    Some((state, property, base, active))
}

fn list_optional_string_comparison(
    expression: &str,
    value_alias: &str,
    refs: &BTreeMap<String, RefBinding>,
) -> Option<(String, String)> {
    let (left, right) = expression
        .split_once(" === ")
        .or_else(|| expression.split_once(" == "))?;
    let item_prefix = format!("{value_alias}.");
    let (state_expression, property) =
        if let Some(property) = right.trim().strip_prefix(&item_prefix) {
            (left.trim(), property)
        } else {
            (right.trim(), left.trim().strip_prefix(&item_prefix)?)
        };
    if !is_rust_identifier(property) {
        return None;
    }
    let mut state = state_expression;
    while let Some(unwrapped) = state.strip_suffix(".value") {
        state = unwrapped;
    }
    refs.get(state)
        .filter(|binding| binding.kind == RefKind::OptionalString)?;
    Some((state.to_owned(), property.to_owned()))
}

fn rust_string(value: &str) -> String {
    format!("{value:?}")
}

fn is_rust_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn contains_identifier(source: &str, identifier: &str) -> bool {
    source.match_indices(identifier).any(|(start, _)| {
        let before = source[..start].chars().next_back();
        let end = start + identifier.len();
        let after = source[end..].chars().next();
        !before.is_some_and(|character| character == '_' || character.is_ascii_alphanumeric())
            && !after.is_some_and(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn computed_rust_expression(
    expression: &str,
    refs: &BTreeMap<String, RefBinding>,
) -> Option<String> {
    for (name, binding) in refs {
        if binding.kind != RefKind::OptionalString {
            continue;
        }
        let prefix = format!("{name}.value === null ? ");
        let Some(remainder) = expression.strip_prefix(&prefix) else {
            continue;
        };
        let (when_none, when_some) = remainder.split_once(" : ")?;
        let when_none = js_string_literal(when_none.trim())?;
        let when_some = js_string_literal(when_some.trim())?;
        return Some(format!(
            "if {name}.read().is_none() {{ {}.to_owned() }} else {{ {}.to_owned() }}",
            rust_string(&when_none),
            rust_string(&when_some)
        ));
    }
    None
}

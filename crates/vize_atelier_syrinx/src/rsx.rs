//! Dioxus RSX compiler target for Syrinx live v3b.
//!
//! The generated Rust owns the render tree. Vue expressions are represented by
//! typed, source-located render-model fields; later expression passes fill
//! those fields directly in Rust or with an explicitly classified pure
//! residual function. There is no mutation protocol, JavaScript guest, site
//! table, or renderer node identity in this artifact.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use vize_atelier_core::{
    ElementNode, ElementType, ExpressionNode, PropNode, SourceLocation, TemplateChildNode,
    TextCallContent,
};
use vize_atelier_sfc::{SfcParseOptions, parse_sfc};
use vize_atelier_vapor::{VaporCompilerOptions, compile_vapor_ir_with_template_syntax};
use vize_carton::Bump;

use crate::{SyrinxCompileFailure, SyrinxCompileOptions, compile_syrinx};

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
    pub expression_hooks: Vec<RsxExpressionHook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldKind {
    Text,
    Attribute,
    Condition,
    Event(&'static str),
    Slot,
    List(String),
}

impl FieldKind {
    fn rust_type(&self) -> String {
        match self {
            Self::Text | Self::Attribute => "String".to_owned(),
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
    fields: Vec<ModelField>,
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
    hooks: Vec<RsxExpressionHook>,
    next_field: u32,
    indent: usize,
    body: String,
}

/// Compile an ordinary Vue SFC to an explicit Dioxus `rsx!` render module.
///
/// The established Syrinx frontend is run first so this target inherits its
/// fail-closed SFC, template, and renderer-safety diagnostics. Only the
/// returned RSX artifact is part of the v3b target.
pub fn compile_syrinx_rsx(
    source: &str,
    options: SyrinxCompileOptions,
) -> Result<SyrinxRsxArtifact, SyrinxCompileFailure> {
    // Share the existing frontend contract while the old target still exists.
    // The v3b deletion slice removes this call together with the legacy ABI.
    compile_syrinx(source, options.clone())?;

    let descriptor = parse_sfc(
        source,
        SfcParseOptions {
            filename: options.filename.clone().into(),
            source_map: true,
            ..Default::default()
        },
    )
    .expect("the shared Syrinx frontend already accepted this SFC");
    let template = descriptor
        .template
        .as_ref()
        .expect("the shared Syrinx frontend requires a template");
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
    let component_name = options
        .component_name
        .clone()
        .unwrap_or_else(|| component_name_from_filename(&options.filename));
    let mut emitter = RsxEmitter::new(component_name);
    emitter.emit_children(&lowered.root.children, Scope::Root);
    let mut artifact = emitter.finish();
    for hook in &mut artifact.expression_hooks {
        hook.start_byte += template.loc.start as u32;
        hook.end_byte += template.loc.start as u32;
    }
    Ok(artifact)
}

impl RsxEmitter {
    fn new(component_name: String) -> Self {
        let component_name = rust_type_name(&component_name);
        Self {
            model_name: format!("{component_name}Model"),
            component_name,
            root_fields: Vec::new(),
            lists: Vec::new(),
            hooks: Vec::new(),
            next_field: 1,
            indent: 2,
            body: String::new(),
        }
    }

    fn finish(self) -> SyrinxRsxArtifact {
        let mut source = String::new();
        writeln!(source, "// @generated by vize_atelier_syrinx; do not edit.").unwrap();
        writeln!(source, "use dioxus::prelude::*;\n").unwrap();
        for list in &self.lists {
            writeln!(source, "/// Render data for one `{}` item.", list.source).unwrap();
            writeln!(source, "#[derive(Clone, PartialEq)]").unwrap();
            writeln!(source, "pub struct {} {{", list.item_type).unwrap();
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
        writeln!(source, "#[derive(Clone, PartialEq)]").unwrap();
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
        writeln!(source, "    rsx! {{").unwrap();
        source.push_str(&self.body);
        writeln!(source, "    }}\n}}").unwrap();
        SyrinxRsxArtifact {
            component_name: self.component_name,
            rust_source: source,
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
                self.line(&format!("{{{value}.clone()}}"));
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
                    self.line(&format!("{{{value}.clone()}}"));
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
                    self.line(&format!("{{{value}.clone()}}"));
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
                self.line(&format!("{{{value}.clone()}}"));
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
            self.line(&format!("{{{value}.clone()}}"));
            return;
        }

        let tag = if element.tag_type == ElementType::Component {
            rust_type_name(element.tag.as_str())
        } else {
            rust_ident(element.tag.as_str())
        };
        self.line(&format!("{tag} {{"));
        self.indent += 1;
        for prop in element.props.iter() {
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
                        continue;
                    };
                    let name = expression_content(argument);
                    let expression = directive
                        .exp
                        .as_ref()
                        .map(expression_content)
                        .unwrap_or_else(|| name.clone());
                    let value = self.add_field(
                        scope,
                        FieldKind::Attribute,
                        if name == "key" { "key" } else { "attribute" },
                        expression,
                        &format!("attribute:{name}"),
                        directive
                            .exp
                            .as_ref()
                            .map(ExpressionNode::loc)
                            .unwrap_or(&directive.loc),
                    );
                    let name = rsx_attribute_name(&name);
                    if name == "key" {
                        self.line(&format!("key: \"{{{value}}}\","));
                    } else {
                        self.line(&format!("{name}: {value}.clone(),"));
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
                    let value = self.add_field(
                        scope,
                        FieldKind::Event(event_type(&event)),
                        "event",
                        expression,
                        &format!("event:{event}"),
                        directive
                            .exp
                            .as_ref()
                            .map(ExpressionNode::loc)
                            .unwrap_or(&directive.loc),
                    );
                    self.line(&format!("on{}: {value},", rust_ident(&event)));
                }
                PropNode::Directive(_) => {}
            }
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
        self.line(&format!("for item_{id} in {reference}.iter() {{"));
        self.indent += 1;
        self.emit_children(&node.children, Scope::List(index));
        self.indent -= 1;
        self.line("}");
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

    fn line(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.body.push_str("    ");
        }
        self.body.push_str(line);
        self.body.push('\n');
    }
}

fn expression_content(expression: &ExpressionNode<'_>) -> String {
    match expression {
        ExpressionNode::Simple(simple) => simple.content.to_string(),
        ExpressionNode::Compound(compound) => compound.loc.source.to_string(),
    }
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

fn event_type(event: &str) -> &'static str {
    match event {
        "input" | "change" | "submit" | "reset" | "invalid" => "FormEvent",
        "keydown" | "keypress" | "keyup" => "KeyboardEvent",
        "focus" | "blur" | "focusin" | "focusout" => "FocusEvent",
        "scroll" => "ScrollEvent",
        "drag" | "dragend" | "dragenter" | "dragleave" | "dragover" | "dragstart" | "drop" => {
            "DragEvent"
        }
        "pointercancel" | "pointerdown" | "pointerenter" | "pointerleave" | "pointermove"
        | "pointerout" | "pointerover" | "pointerup" => "PointerEvent",
        _ => "MouseEvent",
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

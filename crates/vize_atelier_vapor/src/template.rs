//! Renderer-neutral serialization of Vapor's immutable template fragments.
//!
//! Stock Vapor code generation uses the unannotated path. Alternate backends
//! can attach compiler-reserved element attributes and canonical comment
//! anchors while sharing the exact same static-tree serializer.

use std::collections::BTreeMap;

use vize_atelier_core::{
    ElementNode, ElementType, ExpressionNode, PropNode, SourceLocation, TemplateChildNode,
};
use vize_carton::{String, append, cstr, ensure_sufficient_stack};

/// Stable byte range used to address a transformed template node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TemplateSourceRange {
    /// Inclusive byte offset within the template block.
    pub start: u32,
    /// Exclusive byte offset within the template block.
    pub end: u32,
}

impl From<&SourceLocation> for TemplateSourceRange {
    fn from(location: &SourceLocation) -> Self {
        Self {
            start: location.start.offset,
            end: location.end.offset,
        }
    }
}

/// One attribute injected onto an authored element by an alternate backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaporTemplateAttribute {
    /// Compiler-reserved attribute name.
    pub name: String,
    /// Attribute value. `None` emits a boolean attribute.
    pub value: Option<String>,
}

/// Optional renderer annotations layered over the stock static serializer.
#[derive(Debug, Clone, Default)]
pub struct VaporTemplateAnnotations {
    element_attributes: BTreeMap<TemplateSourceRange, std::vec::Vec<VaporTemplateAttribute>>,
    text_anchors: BTreeMap<TemplateSourceRange, String>,
    control_anchors: BTreeMap<TemplateSourceRange, String>,
    scope_attribute: Option<String>,
}

impl VaporTemplateAnnotations {
    /// Add a compiler-reserved attribute to one exact authored element.
    pub fn add_element_attribute(
        &mut self,
        location: &SourceLocation,
        name: impl Into<String>,
        value: Option<impl Into<String>>,
    ) {
        self.element_attributes
            .entry(location.into())
            .or_default()
            .push(VaporTemplateAttribute {
                name: name.into(),
                value: value.map(Into::into),
            });
    }

    /// Replace the containing text/interpolation run with a canonical comment anchor.
    pub fn add_text_anchor(&mut self, location: &SourceLocation, marker: impl Into<String>) {
        self.text_anchors.insert(location.into(), marker.into());
    }

    /// Emit a canonical comment anchor at one transformed `v-if` or `v-for` node.
    pub fn add_control_anchor(&mut self, location: &SourceLocation, marker: impl Into<String>) {
        self.control_anchors.insert(location.into(), marker.into());
    }

    /// Add one boolean scoped-style token to every emitted native element.
    pub fn set_scope_attribute(&mut self, attribute: impl Into<String>) {
        self.scope_attribute = Some(attribute.into());
    }

    fn attributes(&self, location: &SourceLocation) -> &[VaporTemplateAttribute] {
        self.element_attributes
            .get(&location.into())
            .map(std::vec::Vec::as_slice)
            .unwrap_or_default()
    }

    fn text_anchor(&self, location: &SourceLocation) -> Option<&str> {
        self.text_anchors.get(&location.into()).map(String::as_str)
    }

    fn control_anchor(&self, location: &SourceLocation) -> Option<&str> {
        self.control_anchors
            .get(&location.into())
            .map(String::as_str)
    }
}

/// Serialize one element with the stock Vapor static-tree rules and optional annotations.
pub fn generate_vapor_element_template(
    element: &ElementNode<'_>,
    annotations: &VaporTemplateAnnotations,
) -> String {
    let mut template = cstr!("<{}", element.tag);

    let dynamic_attrs: vize_carton::FxHashSet<&str> = element
        .props
        .iter()
        .filter_map(|prop| {
            if let PropNode::Directive(directive) = prop
                && directive.name.as_str() == "bind"
                && let Some(ExpressionNode::Simple(key)) = directive.arg.as_ref()
            {
                return Some(key.content.as_str());
            }
            None
        })
        .collect();

    for prop in element.props.iter() {
        if let PropNode::Attribute(attribute) = prop {
            if is_runtime_only_attr(attribute.name.as_str())
                || dynamic_attrs.contains(attribute.name.as_str())
            {
                continue;
            }
            if let Some(value) = attribute.value.as_ref() {
                append!(template, " {}=\"{}\"", attribute.name, value.content);
            } else {
                append!(template, " {}", attribute.name);
            }
        }
    }

    if let Some(scope_attribute) = annotations.scope_attribute.as_ref() {
        append!(template, " {}", scope_attribute);
    }
    for attribute in annotations.attributes(&element.loc) {
        match attribute.value.as_ref() {
            Some(value) => append!(
                template,
                " {}=\"{}\"",
                attribute.name,
                escape_html_attribute(value)
            ),
            None => append!(template, " {}", attribute.name),
        }
    }

    if is_void_element(element.tag.as_str()) {
        template.push('>');
    } else if element.is_self_closing {
        append!(template, "></{}>", element.tag);
    } else {
        template.push('>');
        append_fragment(&mut template, &element.children, annotations);
        append!(template, "</{}>", element.tag);
    }

    template
}

/// Serialize an arbitrary Vapor fragment, including alternate-backend anchors.
pub fn generate_vapor_fragment_template(
    children: &[TemplateChildNode<'_>],
    annotations: &VaporTemplateAnnotations,
) -> String {
    let mut template = String::default();
    append_fragment(&mut template, children, annotations);
    template
}

fn append_fragment(
    template: &mut String,
    children: &[TemplateChildNode<'_>],
    annotations: &VaporTemplateAnnotations,
) {
    let mut index = 0usize;
    while index < children.len() {
        match &children[index] {
            TemplateChildNode::Text(_) | TemplateChildNode::Interpolation(_) => {
                let start = index;
                while index < children.len()
                    && matches!(
                        children[index],
                        TemplateChildNode::Text(_) | TemplateChildNode::Interpolation(_)
                    )
                {
                    index += 1;
                }
                append_text_run(template, &children[start..index], annotations);
            }
            TemplateChildNode::Element(element) if element.tag_type == ElementType::Template => {
                ensure_sufficient_stack(|| {
                    append_fragment(template, &element.children, annotations)
                });
                index += 1;
            }
            TemplateChildNode::Element(element) if element.tag_type == ElementType::Element => {
                let child = ensure_sufficient_stack(|| {
                    generate_vapor_element_template(element, annotations)
                });
                template.push_str(&child);
                index += 1;
            }
            TemplateChildNode::Element(element)
                if matches!(element.tag_type, ElementType::Component | ElementType::Slot) =>
            {
                if let Some(marker) = annotations.control_anchor(&element.loc) {
                    template.push_str("<!--");
                    template.push_str(marker);
                    template.push_str("-->");
                }
                index += 1;
            }
            TemplateChildNode::If(node) => {
                if let Some(marker) = annotations.control_anchor(&node.loc) {
                    template.push_str("<!--");
                    template.push_str(marker);
                    template.push_str("-->");
                }
                index += 1;
            }
            TemplateChildNode::For(node) => {
                if let Some(marker) = annotations.control_anchor(&node.loc) {
                    template.push_str("<!--");
                    template.push_str(marker);
                    template.push_str("-->");
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
}

fn append_text_run(
    template: &mut String,
    children: &[TemplateChildNode<'_>],
    annotations: &VaporTemplateAnnotations,
) {
    let marker = children.iter().find_map(|child| match child {
        TemplateChildNode::Interpolation(interpolation) => annotations
            .text_anchor(interpolation.content.loc())
            .or_else(|| annotations.text_anchor(&interpolation.loc)),
        _ => None,
    });
    if let Some(marker) = marker {
        template.push_str("<!--");
        template.push_str(marker);
        template.push_str("-->");
        return;
    }

    for child in children {
        match child {
            TemplateChildNode::Text(text) => {
                template.push_str(&escape_html_text(&text.content));
            }
            TemplateChildNode::Interpolation(_) => template.push(' '),
            _ => {}
        }
    }
}

fn is_runtime_only_attr(name: &str) -> bool {
    matches!(name, "ref" | "ref_for" | "ref_key")
}

fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn escape_html_text(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    for character in source.chars() {
        match character {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            _ => result.push(character),
        }
    }
    result
}

fn escape_html_attribute(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    for character in source.chars() {
        match character {
            '&' => result.push_str("&amp;"),
            '"' => result.push_str("&quot;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            _ => result.push(character),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{VaporTemplateAnnotations, generate_vapor_fragment_template};
    use vize_atelier_core::parser::parse;
    use vize_carton::Bump;

    #[test]
    fn annotations_replace_a_text_run_and_control_node_without_dom_identity() {
        let allocator = Bump::new();
        let (mut root, errors) = parse(
            &allocator,
            "<div :class=\"kind\">hello {{ name }}<span>!</span><p v-if=\"open\">yes</p></div>",
        );
        assert!(errors.is_empty());
        vize_atelier_core::lane::transform(
            &allocator,
            &mut root,
            vize_atelier_core::options::TransformOptions {
                vapor: true,
                ..Default::default()
            },
            None,
        );
        let element = match &root.children[0] {
            vize_atelier_core::TemplateChildNode::Element(element) => element,
            _ => panic!("expected root element"),
        };
        let interpolation = match &element.children[1] {
            vize_atelier_core::TemplateChildNode::Interpolation(interpolation) => interpolation,
            _ => panic!("expected interpolation"),
        };
        let control = match &element.children[3] {
            vize_atelier_core::TemplateChildNode::If(control) => control,
            _ => panic!("expected transformed if"),
        };

        let mut annotations = VaporTemplateAnnotations::default();
        annotations.add_element_attribute(&element.loc, "data-site", Some("syrinx:v1:binding:1"));
        annotations.add_text_anchor(interpolation.content.loc(), "syrinx:v1:binding:2");
        annotations.add_control_anchor(&control.loc, "syrinx:v1:if:1");

        assert_eq!(
            generate_vapor_fragment_template(&root.children, &annotations),
            "<div data-site=\"syrinx:v1:binding:1\"><!--syrinx:v1:binding:2--><span>!</span><!--syrinx:v1:if:1--></div>"
        );
    }
}

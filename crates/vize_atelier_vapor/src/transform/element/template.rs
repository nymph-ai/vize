//! Template string construction, escaping, and template-ref extraction.

use super::{
    BlockIRNode, Box, ElementNode, ElementType, ExpressionNode, OperationNode, PropNode,
    SetTemplateRefIRNode, SimpleExpressionNode, String, TemplateChildNode, TransformContext,
};
use vize_carton::ensure_sufficient_stack;

/// Generate element template string (recursively includes static children)
pub(crate) fn generate_element_template(el: &ElementNode<'_>) -> String {
    crate::template::generate_vapor_element_template(
        el,
        &crate::template::VaporTemplateAnnotations::default(),
    )
}

/// Check if an element is static (no dynamic directives)
pub(crate) fn is_static_element(el: &ElementNode<'_>) -> bool {
    if !matches!(el.tag_type, ElementType::Element) {
        return false;
    }

    // Template refs require runtime child lookup even when the rest of the
    // subtree is static, so they must not be folded into a purely static path.
    for prop in el.props.iter() {
        match prop {
            PropNode::Directive(_) => return false,
            PropNode::Attribute(attr) if is_runtime_only_attr(attr.name.as_str()) => return false,
            _ => {}
        }
    }

    // Check if any child is dynamic
    for child in el.children.iter() {
        match child {
            TemplateChildNode::Interpolation(_) => return false,
            TemplateChildNode::Element(child_el) => {
                if !ensure_sufficient_stack(|| is_static_element(child_el)) {
                    return false;
                }
            }
            TemplateChildNode::If(_) | TemplateChildNode::For(_) => return false,
            _ => {}
        }
    }

    true
}

pub(super) fn is_template_backed_element(el: &ElementNode<'_>) -> bool {
    matches!(el.tag_type, ElementType::Element)
}

pub(super) fn transform_template_ref<'a>(
    ctx: &mut TransformContext<'a>,
    el: &ElementNode<'a>,
    element_id: usize,
    block: &mut BlockIRNode<'a>,
) {
    let Some(value) = extract_template_ref_value(ctx, el) else {
        return;
    };

    block
        .operation
        .push(OperationNode::SetTemplateRef(SetTemplateRefIRNode {
            element: element_id,
            value,
            ref_for: has_static_ref_for(el),
        }));
}

fn extract_template_ref_value<'a>(
    ctx: &mut TransformContext<'a>,
    el: &ElementNode<'a>,
) -> Option<Box<'a, SimpleExpressionNode<'a>>> {
    for prop in el.props.iter() {
        match prop {
            PropNode::Attribute(attr) if attr.name.as_str() == "ref" => {
                let value = attr.value.as_ref()?;
                let node =
                    SimpleExpressionNode::new(value.content.clone(), true, value.loc.clone());
                return Some(Box::new_in(node, ctx.allocator));
            }
            PropNode::Directive(dir) if dir.name.as_str() == "bind" => {
                let Some(ExpressionNode::Simple(arg)) = dir.arg.as_ref() else {
                    continue;
                };
                if arg.content.as_str() != "ref" {
                    continue;
                }

                let Some(ExpressionNode::Simple(exp)) = dir.exp.as_ref() else {
                    continue;
                };
                let node =
                    SimpleExpressionNode::new(exp.content.clone(), exp.is_static, exp.loc.clone());
                return Some(Box::new_in(node, ctx.allocator));
            }
            _ => {}
        }
    }

    None
}

fn has_static_ref_for(el: &ElementNode<'_>) -> bool {
    el.props.iter().any(|prop| {
        matches!(
            prop,
            PropNode::Attribute(attr) if attr.name.as_str() == "ref_for"
        )
    })
}

pub(super) fn is_runtime_only_attr(name: &str) -> bool {
    matches!(name, "ref" | "ref_for" | "ref_key")
}

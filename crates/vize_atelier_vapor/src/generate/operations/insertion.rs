use crate::ir::{BlockIRNode, OperationNode};
use vize_carton::cstr;

use super::super::context::GenerateContext;

pub(super) fn emit_insertion_state(
    ctx: &mut GenerateContext,
    parent: Option<usize>,
    anchor: Option<usize>,
    logical_index: Option<usize>,
) {
    let Some(parent_id) = parent else {
        return;
    };
    ctx.use_helper("setInsertionState");
    // The third argument is the logical child index used during hydration.
    // An appending block still needs an explicit null anchor when it carries
    // an index, otherwise the value would occupy the anchor position.
    match (anchor, logical_index) {
        (Some(anchor_id), Some(index)) => ctx.push_line(&cstr!(
            "_setInsertionState(n{}, n{}, {})",
            parent_id,
            anchor_id,
            index
        )),
        (Some(anchor_id), None) => {
            ctx.push_line(&cstr!("_setInsertionState(n{}, n{})", parent_id, anchor_id))
        }
        (None, Some(index)) => ctx.push_line(&cstr!(
            "_setInsertionState(n{}, null, {})",
            parent_id,
            index
        )),
        (None, None) => ctx.push_line(&cstr!("_setInsertionState(n{})", parent_id)),
    }
}

pub(super) fn block_requires_parent_insertion_state(block: &BlockIRNode<'_>) -> bool {
    block.operation.iter().any(|op| match op {
        OperationNode::If(if_node) => if_node.parent.is_none(),
        OperationNode::For(for_node) => for_node.parent.is_none(),
        OperationNode::CreateComponent(component) => component.parent.is_none(),
        OperationNode::SlotOutlet(_) => true,
        _ => false,
    })
}

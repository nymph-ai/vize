use crate::ir::{BlockIRNode, OperationNode};
use vize_carton::cstr;

use super::super::context::GenerateContext;

pub(super) fn emit_insertion_state(
    ctx: &mut GenerateContext,
    parent: Option<usize>,
    anchor: Option<usize>,
) {
    let Some(parent_id) = parent else {
        return;
    };
    ctx.use_helper("setInsertionState");
    // The runtime signature is (parent, anchor?). With an anchor the block is
    // inserted before that node; without one it is appended. The previous
    // `null, true` passed "no anchor" plus a third argument the runtime reads
    // as a logical index, so a block that should precede later static siblings
    // was appended after them.
    match anchor {
        Some(anchor_id) => ctx.push_line(&cstr!(
            "_setInsertionState(n{}, n{})",
            parent_id,
            anchor_id
        )),
        None => ctx.push_line(&cstr!("_setInsertionState(n{})", parent_id)),
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

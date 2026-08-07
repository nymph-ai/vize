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
    // Signature is (parent, anchor?). `null` means "no anchor" — the runtime
    // then appends — and the third argument is read as a logical index, not a
    // flag. Emit the two real forms.
    match anchor {
        Some(anchor_id) => {
            ctx.push_line(&cstr!("_setInsertionState(n{}, n{})", parent_id, anchor_id))
        }
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

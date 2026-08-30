//! I13(F13):workflow 内容身份摘要的编码必须防碰撞 ——
//! 字段值可含任意控制字符(含 NUL/0x01),裸分隔符编码会让
//! 「不同草稿」散列到同一 digest,复用判定误判为同一内容。
//!
//! 长度前缀(或等价规范编码)下,字段边界由长度唯一决定,
//! 控制字符不可能伪造边界/节点分界。

use mf_agent::workflow::{workflow_content_digest, WorkflowNodeDraft};

fn node(key: &str, title: &str, instructions: &str, deps: &[&str]) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.to_string(),
        title: title.to_string(),
        instructions: instructions.to_string(),
        agent_instance_id: "inst-1".to_string(),
        deps: deps.iter().map(|d| d.to_string()).collect(),
    }
}

/// 字段值内嵌 NUL 不得跨字段位移产生同一字节流:
/// (title="p\0q", instructions="r") 与 (title="p", instructions="q\0r")
/// 在裸 NUL 分隔编码下散列相同 —— 必须不同。
#[test]
fn nul_inside_field_cannot_shift_field_boundaries() {
    let a = vec![node("k", "p\0q", "r", &[])];
    let b = vec![node("k", "p", "q\0r", &[])];
    assert_ne!(
        workflow_content_digest(&a, false),
        workflow_content_digest(&b, false),
        "字段内 NUL 不得让不同内容碰撞出同一 digest"
    );
}

/// 0x01 不得伪造节点边界:单节点 title/instructions 内嵌 0x01 与
/// 「拆成两个节点」不得碰撞。
#[test]
fn control_char_cannot_forge_node_boundary() {
    let single = vec![node("a", "x\u{1}b", "instr", &[])];
    let two = vec![node("a", "x", "instr", &[]), node("b", "instr", "", &[])];
    assert_ne!(
        workflow_content_digest(&single, false),
        workflow_content_digest(&two, false),
        "0x01 不得伪造节点边界"
    );
}

/// deps 列表同样不得被字段内控制字符位移。
#[test]
fn nul_inside_dep_list_cannot_shift_entries() {
    let a = vec![node("k", "t", "i", &["d\0e"])];
    let b = vec![node("k", "t", "i", &["d", "e"])];
    assert_ne!(
        workflow_content_digest(&a, false),
        workflow_content_digest(&b, false),
        "deps 内 NUL 不得让不同依赖集碰撞"
    );
}

/// 编码保持内容等价性:完全相同(含顺序无关的节点排序/deps 排序)的
/// 草稿 digest 相等;任一字段不同则不同(基础回归)。
#[test]
fn digest_still_identifies_equal_content() {
    let a = vec![
        node("k1", "t", "i", &["x", "y"]),
        node("k2", "t2", "i2", &[]),
    ];
    let a_reordered = vec![
        node("k2", "t2", "i2", &[]),
        node("k1", "t", "i", &["y", "x"]),
    ];
    assert_eq!(
        workflow_content_digest(&a, true),
        workflow_content_digest(&a_reordered, true)
    );
    let c = vec![
        node("k1", "t", "i-changed", &["x", "y"]),
        node("k2", "t2", "i2", &[]),
    ];
    assert_ne!(
        workflow_content_digest(&a, true),
        workflow_content_digest(&c, true)
    );
    assert_ne!(
        workflow_content_digest(&a, true),
        workflow_content_digest(&a, false)
    );
}

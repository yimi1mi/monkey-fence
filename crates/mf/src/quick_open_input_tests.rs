//! QuickOpen 平台文本输入回归：`simulate_input` 必须经过 InputHandler seam。

use gpui::{AppContext, EntityInputHandler, TestAppContext};

use crate::quick_open::QuickOpen;

#[gpui::test]
fn quick_open_self_focuses_and_accepts_committed_text(cx: &mut TestAppContext) {
    let (quick_open, cx) = cx.add_window_view(|_window, cx| QuickOpen::commands(cx));

    cx.simulate_input("中文");

    cx.read_entity(&quick_open, |quick_open, _| {
        assert_eq!(quick_open.query_for_test(), "中文");
    });
}

#[gpui::test]
fn ime_marked_text_can_be_replaced_and_committed(cx: &mut TestAppContext) {
    let (quick_open, cx) = cx.add_window_view(|_window, cx| QuickOpen::commands(cx));
    cx.update(|window, cx| {
        quick_open.update(cx, |quick_open, cx| {
            quick_open.replace_and_mark_text_in_range(None, "zhong", Some(5..5), window, cx);
            quick_open.replace_and_mark_text_in_range(None, "中文", Some(2..2), window, cx);
        });
    });
    cx.read_entity(&quick_open, |quick_open, _| {
        assert_eq!(quick_open.query_for_test(), "中文");
        assert_eq!(quick_open.marked_range_for_test(), Some(0..6));
    });

    cx.update(|window, cx| {
        quick_open.update(cx, |quick_open, cx| {
            quick_open.replace_text_in_range(None, "中文", window, cx);
        });
    });
    cx.read_entity(&quick_open, |quick_open, _| {
        assert_eq!(quick_open.query_for_test(), "中文");
        assert_eq!(quick_open.marked_range_for_test(), None);
    });
}

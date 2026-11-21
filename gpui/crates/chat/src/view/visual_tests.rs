//! H10 on GPUI's test platform: a fake surface stands in for WebKit and the
//! platform opener records URLs instead of launching a browser.
use super::*;
use crate::events::parse_fixture;
use gpui::{Modifiers, TestAppContext, VisualTestContext};
use std::{cell::RefCell, collections::VecDeque};

#[derive(Default)]
struct Fake {
    created: Vec<(String, u32, bool)>,
    placed: Vec<VisualPlacement>,
    inbox: VecDeque<VisualMessage>,
}
struct FakeSurface(Rc<RefCell<Fake>>);
impl VisualSurface for FakeSurface {
    fn place(&mut self, placement: VisualPlacement) {
        self.0.borrow_mut().placed.push(placement);
    }
    fn drain(&mut self) -> Vec<VisualMessage> {
        self.0.borrow_mut().inbox.drain(..).collect()
    }
}

const HTML: &str = "<h1>Chart</h1><a href=\"https://example.com/a\">a</a>";

fn records(name: &str) -> Vec<Record> {
    let output = serde_json::json!({"html_render": {"html": HTML, "height": 240}}).to_string();
    parse_fixture(&format!(
        "{}\n{}",
        serde_json::json!({"type":"item_started","data":{"id":"v","kind":{"type":"tool_call","name":name,"input":{"html":HTML,"height":240}}}}),
        serde_json::json!({"type":"item_completed","data":{"id":"v","output":output,"is_error":false}}),
    ))
    .unwrap()
}

fn setup<'a>(
    host: Option<Rc<RefCell<Fake>>>,
    name: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<ChatView>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        init(cx);
        if let Some(fake) = host {
            cx.set_global(VisualHost(Rc::new(move |_, html, height, light| {
                fake.borrow_mut().created.push((html.into(), height, light));
                Ok(Box::new(FakeSurface(fake.clone())))
            })));
        }
    });
    let records = records(name);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| ChatView::new(records, window, cx));
        Root::new(chat, window, cx)
    });
    cx.simulate_resize(size(px(1000.), px(800.)));
    let chat = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<ChatView>().unwrap()
    });
    draw(cx);
    (chat, cx)
}
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    cx.run_until_parked();
}
fn shown(selector: String, cx: &mut VisualTestContext) -> bool {
    draw(cx);
    cx.debug_bounds(selector.leak()).is_some()
}
fn click(selector: String, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector.clone().leak())
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    draw(cx);
}
fn reply(
    chat: &Entity<ChatView>,
    cx: &mut VisualTestContext,
) -> (u64, Entity<visual::VisualReply>) {
    cx.read(|cx| {
        let chat = chat.read(cx);
        let index = chat
            .model
            .rows
            .iter()
            .position(|row| row.tool.is_some())
            .unwrap();
        let row = chat.row_views[index].as_ref().expect("row drawn").read(cx);
        (row.row.id, row.visual.clone().expect("visual reply"))
    })
}

#[gpui::test]
fn html_reply_expands_closes_and_esc_collapses_as_iced(cx: &mut TestAppContext) {
    let fake = Rc::new(RefCell::new(Fake::default()));
    let (chat, cx) = setup(Some(fake.clone()), "mcp__gitterm_tasks__html_render", cx);
    let (id, reply) = reply(&chat, cx);
    assert_eq!(fake.borrow().created, [(HTML.to_string(), 240, false)]);
    assert!(shown(format!("visual-{id}-Expand"), cx));
    let inline = fake.borrow().placed.last().copied().expect("placed");
    assert_eq!(inline.bounds.size.height, px(240.));

    click(format!("visual-{id}-Expand"), cx);
    assert!(shown(format!("visual-{id}-Close expanded view"), cx));
    assert!(!shown(format!("visual-{id}-Expand"), cx));
    cx.read(|cx| assert!(chat.read(cx).expanded_visual.is_some()));
    let expanded = fake.borrow().placed.last().copied().unwrap();
    assert!(expanded.bounds.size.height > px(600.), "{expanded:?}");
    assert!(expanded.bounds.size.width > inline.bounds.size.width);

    click(format!("visual-{id}-Close expanded view"), cx);
    assert!(shown(format!("visual-{id}-Expand"), cx));
    cx.read(|cx| assert!(chat.read(cx).expanded_visual.is_none()));
    let back = fake.borrow().placed.last().copied().unwrap();
    assert_eq!(back.bounds.size.height, px(240.));

    // Esc in the app collapses.
    click(format!("visual-{id}-Expand"), cx);
    cx.simulate_keystrokes("escape");
    assert!(shown(format!("visual-{id}-Expand"), cx));
    cx.read(|cx| assert!(chat.read(cx).expanded_visual.is_none()));

    // Esc inside the frame comes back as a bridge message.
    click(format!("visual-{id}-Expand"), cx);
    fake.borrow_mut().inbox.push_back(VisualMessage::Escape);
    cx.update(|_, cx| reply.update(cx, |reply, cx| reply.pump(cx)));
    assert!(shown(format!("visual-{id}-Expand"), cx));
    cx.read(|cx| assert!(chat.read(cx).expanded_visual.is_none()));
}

#[gpui::test]
fn frame_links_open_only_http_urls(cx: &mut TestAppContext) {
    let fake = Rc::new(RefCell::new(Fake::default()));
    let (chat, cx) = setup(Some(fake.clone()), "html_render", cx);
    let (_, reply) = reply(&chat, cx);
    for refused in [
        "javascript:alert(1)",
        "file:///etc/hosts",
        "data:text/html,x",
        "https://user:secret@example.com/",
        "https:///nohost",
        "gitterm://open",
    ] {
        fake.borrow_mut()
            .inbox
            .push_back(VisualMessage::Link(refused.into()));
        cx.update(|_, cx| reply.update(cx, |reply, cx| reply.pump(cx)));
        assert_eq!(cx.opened_url(), None, "{refused}");
    }
    fake.borrow_mut()
        .inbox
        .push_back(VisualMessage::Link("https://example.com/a".into()));
    cx.update(|_, cx| reply.update(cx, |reply, cx| reply.pump(cx)));
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/a"));
}

#[gpui::test]
fn a_result_arriving_after_the_tool_row_is_drawn_adds_the_reply(cx: &mut TestAppContext) {
    let fake = Rc::new(RefCell::new(Fake::default()));
    let (chat, cx) = setup(Some(fake.clone()), "Bash", cx);
    let started = serde_json::json!({"type":"item_started","data":{"id":"live","kind":{"type":"tool_call","name":"html_render","input":{}}}});
    let output = serde_json::json!({"html_render": {"html": HTML, "height": 300}}).to_string();
    let done = serde_json::json!({"type":"item_completed","data":{"id":"live","output":output,"is_error":false}});
    for record in parse_fixture(&started.to_string()).unwrap() {
        cx.update(|_, cx| chat.update(cx, |chat, cx| chat.append_record(&record, cx)));
    }
    draw(cx);
    assert!(
        fake.borrow().created.is_empty(),
        "no reply before the result"
    );
    for record in parse_fixture(&done.to_string()).unwrap() {
        cx.update(|_, cx| chat.update(cx, |chat, cx| chat.append_record(&record, cx)));
    }
    draw(cx);
    assert_eq!(fake.borrow().created, [(HTML.to_string(), 300, false)]);
    let id = cx.read(|cx| chat.read(cx).model.rows.last().unwrap().id);
    assert!(shown(format!("visual-{id}-Expand"), cx));
}

#[gpui::test]
fn without_a_native_host_the_reply_shows_the_iced_note(cx: &mut TestAppContext) {
    let (chat, cx) = setup(None, "html_render", cx);
    let (id, _) = reply(&chat, cx);
    assert!(shown(format!("visual-reply-{id}"), cx));
    assert!(!shown(format!("visual-{id}-Expand"), cx));
}

#[test]
fn only_valid_html_render_results_become_visual_replies() {
    let output =
        serde_json::json!({"html_render": {"html": "<p>x</p>", "height": 100}}).to_string();
    let tool = |name: &str, output: &str| Tool {
        name: name.into(),
        input: serde_json::Value::Null,
        partial_input: String::new(),
        output: Some(output.into()),
        is_error: false,
    };
    for name in [
        "html_render",
        "mcp__gitterm_tasks__html_render",
        "mcp_gitterm-tasks_html_render",
        "gitterm_tasks_html_render",
    ] {
        assert_eq!(
            visual::visual_request(&tool(name, &output)),
            Some(("<p>x</p>".into(), 100))
        );
    }
    assert_eq!(visual::visual_request(&tool("Bash", &output)), None);
    let tall = serde_json::json!({"html_render": {"html": "x", "height": 99}}).to_string();
    assert_eq!(visual::visual_request(&tool("html_render", &tall)), None);
    assert_eq!(
        visual::visual_request(&tool("html_render", "not json")),
        None
    );
    assert!(visual_link("http://example.com"));
    assert!(visual_link("HTTPS://example.com/path?q#f"));
    assert!(!visual_link("https://a@b.com"));
    assert!(!visual_link("ftp://example.com"));
}

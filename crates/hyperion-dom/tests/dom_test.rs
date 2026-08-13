//! DOM parsing and selector-matching tests.

use hyperion_dom::NodeData;
use hyperion_dom::PREVIEW_NODE_ID_ATTR;
use hyperion_dom::parse_html;

#[test]
fn parses_structure() {
    let dom = parse_html(
        "<!doctype html><html><head><title>T</title></head><body><p>hi</p></body></html>",
    );
    let doc = dom.document();
    let doctype = doc
        .children()
        .iter()
        .find(|c| matches!(c.data(), NodeData::DocumentType { .. }))
        .expect("doctype node");
    assert!(matches!(
        doctype.data(),
        NodeData::DocumentType { name, .. } if name == "html"
    ));

    let p = dom.query_selector("p").unwrap().expect("p element");
    assert!(matches!(p.data(), NodeData::Element(e) if e.name().as_ref() == "p"));
    let text = p.children()[0].data();
    assert!(matches!(text, NodeData::Text(t) if t == "hi"));
}

#[test]
fn query_selector_variants() {
    let html = r#"
        <html><body>
            <div id="a" class="box one"></div>
            <div id="b" class="box two"><span class="inner">x</span></div>
            <p class="box">text</p>
            <input type="text" disabled>
            <a href="/x">link</a>
        </body></html>
    "#;
    let dom = parse_html(html);

    // Type selector
    let divs = dom.query_selector_all("div").unwrap();
    assert_eq!(divs.len(), 2);

    // Class selector
    let boxes = dom.query_selector_all(".box").unwrap();
    assert_eq!(boxes.len(), 3);

    // ID selector
    let a = dom.query_selector("#a").unwrap().expect("#a");
    assert!(
        matches!(a.data(), NodeData::Element(e) if e.attrs.iter().any(|at| at.name.as_ref() == "id" && at.value == "a"))
    );

    // Descendant combinator + attribute selector
    let inner = dom.query_selector_all("div .inner").unwrap();
    assert_eq!(inner.len(), 1);
    let inner = inner[0];
    assert!(matches!(inner.data(), NodeData::Element(e) if e.name().as_ref() == "span"));

    // Attribute presence and value
    assert_eq!(dom.query_selector_all("[disabled]").unwrap().len(), 1);
    assert_eq!(
        dom.query_selector_all("input[type='text']").unwrap().len(),
        1
    );
    assert_eq!(
        dom.query_selector_all("input[type='button']")
            .unwrap()
            .len(),
        0
    );

    // Child combinator
    assert_eq!(dom.query_selector_all("div > span").unwrap().len(), 1);

    // Pseudo-classes: structural + :is
    let ul = parse_html("<ul><li>a</li><li>b</li><li>c</li></ul>");
    assert_eq!(ul.query_selector_all("li:first-child").unwrap().len(), 1);
    assert_eq!(ul.query_selector_all("li:last-child").unwrap().len(), 1);
    assert_eq!(ul.query_selector_all("li:nth-child(2)").unwrap().len(), 1);
    assert_eq!(ul.query_selector_all(":is(li, ul)").unwrap().len(), 4);

    // :is on the main fixture
    assert_eq!(dom.query_selector_all(":is(div, p)").unwrap().len(), 3);

    // :empty
    assert_eq!(dom.query_selector_all("div:empty").unwrap().len(), 1);

    // :root
    assert_eq!(dom.query_selector_all(":root").unwrap().len(), 1);
    let root = dom.query_selector_all(":root").unwrap();
    assert!(matches!(root[0].data(), NodeData::Element(e) if e.name().as_ref() == "html"));

    // querySelector returns the first in document order
    let first = dom.query_selector(".box").unwrap().expect("first box");
    assert!(
        matches!(first.data(), NodeData::Element(e) if e.attrs.iter().any(|at| at.value == "a"))
    );
}

#[test]
fn invalid_selector_errors() {
    let dom = parse_html("<html><body></body></html>");
    assert!(dom.query_selector_all("div >>>> span").is_err());
    assert!(dom.query_selector_all("").is_err());
}

#[test]
fn text_content_concatenation() {
    let dom = parse_html("<p>Hello <b>world</b>!</p>");
    let p = dom.query_selector("p").unwrap().unwrap();
    let mut text = String::new();
    fn walk(id: hyperion_dom::NodeId, out: &mut String) {
        for child in id.children() {
            match child.data() {
                NodeData::Text(t) => out.push_str(t),
                _ => walk(*child, out),
            }
        }
    }
    walk(p, &mut text);
    assert_eq!(text, "Hello world!");
}

#[test]
fn namespace_awareness() {
    let dom = parse_html("<svg><circle/></svg>");
    let circle = dom.query_selector("circle").unwrap().unwrap();
    let svg = circle.parent_element().expect("svg parent");
    assert!(matches!(svg.data(), NodeData::Element(e) if e.name().as_ref() == "svg"));
    assert!(
        matches!(circle.data(), NodeData::Element(e) if e.namespace().as_ref() == "http://www.w3.org/2000/svg")
    );
}

#[test]
fn outer_html_serializes_document() {
    let dom = parse_html(
        "<!doctype html><html><head><title>T</title></head><body><p class=\"x\">a & b</p><br><img src=\"/i.png\" alt=\"\"></body></html>",
    );
    let body = dom.query_selector("body").unwrap().unwrap();
    let html = dom.outer_html(body);
    assert!(html.contains("<p class=\"x\">a &amp; b</p>"), "got: {html}");
    assert!(html.contains("<br>"), "void element: {html}");
    assert!(
        html.contains("<img src=\"/i.png\" alt=\"\">"),
        "got: {html}"
    );
}

#[test]
fn mutations_change_dom() {
    let dom = parse_html("<html><body><div id=\"a\">old</div><span>x</span></body></html>");
    let div = dom.query_selector("#a").unwrap().unwrap();
    let span = dom.query_selector("span").unwrap().unwrap();

    dom.set_attribute(div, "class", "new");
    assert_eq!(dom.outer_html(div), "<div id=\"a\" class=\"new\">old</div>");
    dom.remove_attribute(div, "id");
    assert_eq!(dom.outer_html(div), "<div class=\"new\">old</div>");

    dom.set_text(div, "replaced".to_string());
    assert_eq!(dom.outer_html(div), "<div class=\"new\">replaced</div>");

    dom.remove_node(span);
    let body = dom.query_selector("body").unwrap().unwrap();
    let html = dom.outer_html(body);
    assert!(!html.contains("<span>"), "removed span: {html}");
}

#[test]
fn inner_html_parses_fragment() {
    let dom = parse_html("<html><body><div id=\"a\">old</div></body></html>");
    let div = dom.query_selector("#a").unwrap().unwrap();
    dom.inner_html(div, "<p>one</p><p>two <b>bold</b></p>");
    assert_eq!(
        dom.outer_html(div),
        "<div id=\"a\"><p>one</p><p>two <b>bold</b></p></div>"
    );
    let ps = dom.query_selector_all("p").unwrap();
    assert_eq!(ps.len(), 2);
    assert_eq!(dom.outer_html(ps[1]), "<p>two <b>bold</b></p>");
}

#[test]
fn inner_html_reparses_after_mutation() {
    let dom = parse_html("<html><body><ul><li>a</li></ul></body></html>");
    let ul = dom.query_selector("ul").unwrap().unwrap();
    dom.inner_html(ul, "<li>b</li><li>c</li>");
    assert_eq!(dom.outer_html(ul), "<ul><li>b</li><li>c</li></ul>");
    dom.inner_html(ul, "");
    assert_eq!(dom.outer_html(ul), "<ul></ul>");
}

#[test]
fn annotated_html_injects_stable_ids_without_mutating_dom() {
    let dom = parse_html("<html><body><p class=\"x\">a &amp; b</p><br></body></html>");
    let p = dom.query_selector("p").unwrap().unwrap();
    let body = dom.query_selector("body").unwrap().unwrap();
    let before = dom.outer_html(body);
    assert!(
        !before.contains(PREVIEW_NODE_ID_ATTR),
        "plain outer HTML must stay unannotated: {before}"
    );

    let html = dom.outer_html_annotated(body, |n| {
        if n == p {
            Some(7)
        } else if n == body {
            Some(3)
        } else {
            Some(1)
        }
    });
    assert!(
        html.contains(&format!(
            "<p class=\"x\" {PREVIEW_NODE_ID_ATTR}=\"7\">a &amp; b</p>"
        )),
        "annotated p: {html}"
    );
    assert!(
        html.contains(&format!("<br {PREVIEW_NODE_ID_ATTR}=\"1\">")),
        "void element should still receive an id: {html}"
    );
    assert_eq!(
        dom.outer_html(body),
        before,
        "annotated serialization must not mutate the live DOM"
    );
}

#[test]
fn annotated_html_does_not_override_reserved_attribute() {
    let dom = parse_html(&format!(
        "<html><body><div {PREVIEW_NODE_ID_ATTR}=\"page-owned\">x</div></body></html>"
    ));
    let div = dom.query_selector("div").unwrap().unwrap();
    let html = dom.outer_html_annotated(div, |_| Some(99));
    assert!(
        html.contains(&format!("{PREVIEW_NODE_ID_ATTR}=\"page-owned\"")),
        "page-provided attribute must win: {html}"
    );
    assert!(
        !html.contains(&format!("{PREVIEW_NODE_ID_ATTR}=\"99\"")),
        "must not override reserved attribute: {html}"
    );
}

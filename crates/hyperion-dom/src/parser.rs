//! HTML parsing via html5ever's tree builder, feeding into our arena DOM.

use markup5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use markup5ever::{Attribute, QualName};

use crate::dom::{Attribute as DomAttribute, Dom, NodeData, NodeId};

/// A wrapper implementing `TreeSink` over the arena-backed `Dom`.
pub struct DomSink(pub Dom);

impl TreeSink for DomSink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = &'a QualName;

    fn finish(self) -> Dom {
        self.0
    }

    fn parse_error(&self, _msg: std::borrow::Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        self.0.document()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> &'a QualName {
        match target.data() {
            NodeData::Element(e) => &e.qual_name,
            _ => panic!("elem_name called on a non-element"),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        let attrs = attrs
            .into_iter()
            .map(|a| DomAttribute {
                name: a.name.local,
                value: a.value.to_string(),
            })
            .collect();
        self.0.create_element(name, attrs)
    }

    fn create_comment(&self, text: tendril::StrTendril) -> NodeId {
        self.0.create_comment(text.to_string())
    }

    fn create_pi(&self, _target: tendril::StrTendril, data: tendril::StrTendril) -> NodeId {
        self.0.create_comment(data.to_string())
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node) => self.0.append_child(*parent, node),
            NodeOrText::AppendText(text) => self.0.append_text(*parent, text.to_string()),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if prev_element.node().parent.is_some() {
            self.append(prev_element, child);
        } else {
            self.append(element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: tendril::StrTendril,
        public_id: tendril::StrTendril,
        system_id: tendril::StrTendril,
    ) {
        let node = self.0.create_document_type(
            name.to_string(),
            public_id.to_string(),
            system_id.to_string(),
        );
        self.0.append_child(self.0.document(), node);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        *target
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let parent = match sibling.parent() {
            Some(p) => p,
            None => return,
        };
        let node = match new_node {
            NodeOrText::AppendNode(n) => n,
            NodeOrText::AppendText(t) => self.0.create_text(t.to_string()),
        };
        self.0.node_mut(node).parent = Some(parent);
        let siblings = &mut self.0.node_mut(parent).children;
        if let Some(idx) = siblings.iter().position(|&s| s == *sibling) {
            siblings.insert(idx, node);
        } else {
            siblings.push(node);
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let elem_mut = match &mut self.0.node_mut(*target).data {
            NodeData::Element(e) => e,
            _ => return,
        };
        for attr in attrs {
            let local = attr.name.local.clone();
            let value = attr.value.to_string();
            if !elem_mut.attrs.iter().any(|a| a.name == local) {
                elem_mut.attrs.push(DomAttribute { name: local, value });
            }
        }
    }

    fn associate_with_form(
        &self,
        _target: &NodeId,
        _form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
    }

    fn remove_from_parent(&self, target: &NodeId) {
        let Some(parent) = target.parent() else {
            return;
        };
        let node_mut = self.0.node_mut(*target);
        node_mut.parent = None;
        let children = &mut self.0.node_mut(parent).children;
        if let Some(idx) = children.iter().position(|&c| c == *target) {
            children.remove(idx);
        }
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut moving = std::mem::take(&mut self.0.node_mut(*node).children);
        for child in &mut moving {
            self.0.node_mut(*child).parent = Some(*new_parent);
        }
        let parent_children = &mut self.0.node_mut(*new_parent).children;
        parent_children.append(&mut moving);
    }
}

/// Parse an HTML document into a fresh `Dom`.
pub fn parse_html(html: &str) -> Dom {
    let sink = DomSink(Dom::new());
    let mut parser = html5ever::parse_document(sink, html5ever::ParseOpts::default());
    tendril::TendrilSink::process(&mut parser, html.into());
    tendril::TendrilSink::finish(parser)
}

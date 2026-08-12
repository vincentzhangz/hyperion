//! Arena-allocated DOM tree.
//!
//! Nodes live in a `typed_arena::Arena` (append-only, stable addresses) and
//! [`NodeId`] is a raw pointer into that arena. Navigation therefore works from
//! a `NodeId` alone, without holding a reference to the [`Dom`], which is what
//! the `selectors` engine needs.

use markup5ever::{LocalName, Namespace};
use std::cell::RefCell;

use crate::parser::DomSink;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(*const Node);

impl NodeId {
    pub(crate) fn node(self) -> &'static Node {
        // SAFETY: `NodeId` pointers come from the arena and nodes never move
        // or are deallocated for the lifetime of the owning `Dom`. `Dom` is
        // not `Sync`, so the pointer is never dereferenced concurrently with a
        // mutation.
        unsafe { &*self.0 }
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.node().parent
    }

    /// The nearest ancestor element, skipping non-element nodes.
    pub fn parent_element(&self) -> Option<NodeId> {
        self.parent().filter(|p| p.is_element())
    }

    pub fn children(&self) -> &'static [NodeId] {
        &self.node().children
    }

    pub fn data(&self) -> &'static NodeData {
        &self.node().data
    }

    pub fn is_element(&self) -> bool {
        matches!(self.node().data, NodeData::Element(_))
    }
}

#[derive(Debug)]
pub struct Node {
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) data: NodeData,
}

#[derive(Debug)]
pub enum NodeData {
    Document,
    DocumentType {
        name: String,
        public_id: String,
        system_id: String,
    },
    Text(String),
    Comment(String),
    Element(ElementData),
}

#[derive(Debug)]
pub struct ElementData {
    pub qual_name: markup5ever::QualName,
    pub attrs: Vec<Attribute>,
}

impl ElementData {
    pub fn name(&self) -> &LocalName {
        &self.qual_name.local
    }

    pub fn namespace(&self) -> &Namespace {
        &self.qual_name.ns
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: LocalName,
    pub value: String,
}

/// The arena-backed DOM. Node ids are only valid while this `Dom` is alive.
pub struct Dom {
    nodes: RefCell<typed_arena::Arena<Node>>,
    document: NodeId,
}

impl Dom {
    pub fn new() -> Self {
        let mut nodes = typed_arena::Arena::new();
        let document = Self::alloc_node(&mut nodes, NodeData::Document, None);
        Self {
            nodes: RefCell::new(nodes),
            document,
        }
    }

    pub fn document(&self) -> NodeId {
        self.document
    }

    pub fn node(&self, id: NodeId) -> &Node {
        id.node()
    }

    pub fn create_element(&self, name: markup5ever::QualName, attrs: Vec<Attribute>) -> NodeId {
        self.alloc(NodeData::Element(ElementData {
            qual_name: name,
            attrs,
        }))
    }

    pub fn create_text(&self, text: String) -> NodeId {
        self.alloc(NodeData::Text(text))
    }

    pub fn create_comment(&self, text: String) -> NodeId {
        self.alloc(NodeData::Comment(text))
    }

    pub fn create_document_type(
        &self,
        name: String,
        public_id: String,
        system_id: String,
    ) -> NodeId {
        self.alloc(NodeData::DocumentType {
            name,
            public_id,
            system_id,
        })
    }

    pub fn append_child(&self, parent: NodeId, child: NodeId) {
        let child_mut = self.node_mut(child);
        child_mut.parent = Some(parent);
        self.node_mut(parent).children.push(child);
    }

    pub fn append_text(&self, parent: NodeId, text: String) {
        self.append_child(parent, self.create_text(text));
    }

    fn alloc(&self, data: NodeData) -> NodeId {
        let mut nodes = self.nodes.borrow_mut();
        Self::alloc_node(&mut nodes, data, None)
    }

    fn alloc_node(
        nodes: &mut typed_arena::Arena<Node>,
        data: NodeData,
        parent: Option<NodeId>,
    ) -> NodeId {
        let node_ref = nodes.alloc(Node {
            parent,
            children: Vec::new(),
            data,
        });
        NodeId(node_ref as *const Node)
    }

    /// # Safety
    ///
    /// Returns a `&mut` from `&self`. Sound because access goes through the
    /// interior `RefCell<Arena<Node>>` and the arena never reallocates nodes;
    /// callers must not hold other borrows of the same node (e.g. from
    /// [`NodeId::data`]) at the same time. Required because the html5ever
    /// `TreeSink` trait only offers `&self`.
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn node_mut(&self, id: NodeId) -> &mut Node {
        // SAFETY: unique access to the arena via `&self` + interior mutability;
        // the arena never reallocates nodes.
        unsafe { &mut *(id.0 as *mut Node) }
    }

    // --- Mutation ---------------------------------------------------------

    /// Create an HTML element by local name (no attributes).
    pub fn create_element_by_name(&self, name: &str) -> NodeId {
        let qual = markup5ever::QualName {
            prefix: None,
            ns: markup5ever::Namespace::from("http://www.w3.org/1999/xhtml"),
            local: LocalName::from(name),
        };
        self.create_element(qual, Vec::new())
    }

    /// Read an attribute value from an element node.
    pub fn get_attribute(&self, id: NodeId, name: &str) -> Option<String> {
        match id.data() {
            NodeData::Element(e) => {
                let local = LocalName::from(name);
                e.attrs
                    .iter()
                    .find(|a| a.name == local)
                    .map(|a| a.value.clone())
            }
            _ => None,
        }
    }

    /// All attribute (name, value) pairs on an element.
    pub fn attributes(&self, id: NodeId) -> Vec<(String, String)> {
        match id.data() {
            NodeData::Element(e) => e
                .attrs
                .iter()
                .map(|a| (a.name.as_ref().to_owned(), a.value.clone()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Collect descendant text in document order.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        fn walk(id: NodeId, out: &mut String) {
            if let NodeData::Text(t) = id.data() {
                out.push_str(t);
            }
            for child in id.children() {
                walk(*child, out);
            }
        }
        walk(id, &mut out);
        out
    }

    /// Set (or add) an attribute on an element node.
    pub fn set_attribute(&self, id: NodeId, name: &str, value: impl Into<String>) {
        let value = value.into();
        if let NodeData::Element(e) = &mut self.node_mut(id).data {
            let local = LocalName::from(name);
            match e.attrs.iter_mut().find(|a| a.name == local) {
                Some(attr) => attr.value = value,
                None => e.attrs.push(Attribute { name: local, value }),
            }
        }
    }

    /// Remove an attribute from an element node.
    pub fn remove_attribute(&self, id: NodeId, name: &str) {
        if let NodeData::Element(e) = &mut self.node_mut(id).data {
            let local = LocalName::from(name);
            e.attrs.retain(|a| a.name != local);
        }
    }

    /// Replace the node's outer HTML by parsing `html` as a fragment and
    /// swapping the node in its parent for the resulting nodes.
    pub fn set_outer_html(&self, id: NodeId, html: &str) -> Result<(), String> {
        let Some(parent) = id.parent() else {
            return Err("cannot set outerHTML on the document root".to_owned());
        };
        let context = match parent.data() {
            NodeData::Element(e) => e.qual_name.clone(),
            _ => html_context(),
        };
        let sink = DomSink(Dom::new());
        let mut parser =
            html5ever::parse_fragment(sink, html5ever::ParseOpts::default(), context, Vec::new());
        tendril::TendrilSink::process(&mut parser, html.into());
        let parsed = tendril::TendrilSink::finish(parser);
        let root = parsed
            .document()
            .children()
            .first()
            .copied()
            .ok_or_else(|| "empty fragment".to_owned())?;

        let children = parent.children().to_vec();
        let idx = children
            .iter()
            .position(|&c| c == id)
            .ok_or_else(|| "node not found in parent".to_owned())?;
        self.remove_node(id);

        let mut insert_at = idx;
        for child in root.children() {
            if let Some(copy) = self.deep_copy(*child) {
                // Insert at the original position by rebuilding children.
                let parent_mut = self.node_mut(parent);
                if insert_at >= parent_mut.children.len() {
                    parent_mut.children.push(copy);
                } else {
                    parent_mut.children.insert(insert_at, copy);
                }
                self.node_mut(copy).parent = Some(parent);
                insert_at += 1;
            }
        }
        Ok(())
    }

    /// Replace a node's children with a single text node.
    pub fn set_text(&self, id: NodeId, text: String) {
        self.clear_children(id);
        self.append_text(id, text);
    }

    /// Replace a text or comment node's character data.
    pub fn set_character_data(&self, id: NodeId, text: impl Into<String>) {
        let text = text.into();
        match &mut self.node_mut(id).data {
            NodeData::Text(t) | NodeData::Comment(t) => *t = text,
            _ => {}
        }
    }

    /// Detach a node (and its subtree) from its parent. The node itself stays
    /// allocated in the arena; it is simply no longer reachable from the root.
    pub fn remove_node(&self, id: NodeId) {
        let Some(parent) = id.parent() else {
            return;
        };
        self.node_mut(id).parent = None;
        let children = &mut self.node_mut(parent).children;
        if let Some(idx) = children.iter().position(|&c| c == id) {
            children.remove(idx);
        }
    }

    /// Parse `html` as a fragment and replace the node's children with the
    /// resulting tree.
    pub fn inner_html(&self, id: NodeId, html: &str) {
        let context = match id.data() {
            NodeData::Element(e) => e.qual_name.clone(),
            _ => html_context(),
        };
        let sink = DomSink(Dom::new());
        let mut parser =
            html5ever::parse_fragment(sink, html5ever::ParseOpts::default(), context, Vec::new());
        tendril::TendrilSink::process(&mut parser, html.into());
        let parsed = tendril::TendrilSink::finish(parser);

        let root = parsed.document().children().first().copied();
        self.clear_children(id);
        if let Some(root) = root {
            for child in root.children() {
                if let Some(copy) = self.deep_copy(*child) {
                    self.append_child(id, copy);
                }
            }
        }
    }

    // --- Serialization -----------------------------------------------------

    /// Serialize a node and its descendants to HTML (outerHTML).
    pub fn outer_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node(id, &mut out);
        out
    }

    /// Serialize a node's children to HTML (innerHTML).
    pub fn inner_html_str(&self, id: NodeId) -> String {
        let mut out = String::new();
        for child in id.children() {
            self.serialize_node(*child, &mut out);
        }
        out
    }

    fn clear_children(&self, id: NodeId) {
        let children = std::mem::take(&mut self.node_mut(id).children);
        for child in children {
            self.node_mut(child).parent = None;
        }
    }

    /// Deep-copy a subtree from another `Dom` into this one. Used by
    /// [`inner_html`](Dom::inner_html), whose fragment parse produces a
    /// separate arena whose node ids are not valid here.
    fn deep_copy(&self, src: NodeId) -> Option<NodeId> {
        match src.data() {
            NodeData::Element(e) => {
                let node = self.create_element(e.qual_name.clone(), e.attrs.clone());
                for child in src.children() {
                    if let Some(copy) = self.deep_copy(*child) {
                        self.append_child(node, copy);
                    }
                }
                Some(node)
            }
            NodeData::Text(t) => Some(self.create_text(t.clone())),
            NodeData::Comment(c) => Some(self.create_comment(c.clone())),
            _ => None,
        }
    }

    fn serialize_node(&self, id: NodeId, out: &mut String) {
        match id.data() {
            NodeData::Document => {
                for child in id.children() {
                    self.serialize_node(*child, out);
                }
            }
            NodeData::DocumentType { name, .. } => {
                out.push_str("<!DOCTYPE ");
                out.push_str(name);
                out.push('>');
            }
            NodeData::Text(t) => escape_text(t, out),
            NodeData::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            NodeData::Element(e) => {
                let name = e.name().as_ref();
                out.push('<');
                out.push_str(name);
                for attr in &e.attrs {
                    out.push(' ');
                    out.push_str(attr.name.as_ref());
                    out.push_str("=\"");
                    escape_attr(&attr.value, out);
                    out.push('"');
                }
                if is_void_element(name) {
                    out.push('>');
                    return;
                }
                out.push('>');
                for child in id.children() {
                    self.serialize_node(*child, out);
                }
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
        }
    }
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
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

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn html_context() -> markup5ever::QualName {
    markup5ever::QualName {
        prefix: None,
        ns: markup5ever::Namespace::from("http://www.w3.org/1999/xhtml"),
        local: markup5ever::LocalName::from("div"),
    }
}

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

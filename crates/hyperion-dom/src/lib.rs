//! hyperion-dom: arena-allocated DOM tree, HTML parsing and CSS selectors.
//!
//! An append-only arena of [`Node`]s parsed with html5ever and queried with
//! the servo `selectors` engine.

pub mod dom;
pub mod parser;
pub mod selectors;

pub use dom::{Attribute, Dom, ElementData, Node, NodeData, NodeId, PREVIEW_NODE_ID_ATTR};
pub use parser::parse_html;

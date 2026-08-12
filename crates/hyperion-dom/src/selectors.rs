//! CSS selector matching on the arena DOM via the `selectors` crate.

use cssparser::{Parser as CssParser, ParserInput, ToCss};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{
    AttrSelectorOperation, AttrSelectorOperator, CaseSensitivity, NamespaceConstraint,
};
use selectors::bloom::BloomFilter;
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::{ElementSelectorFlags, matches_selector_list};
use selectors::parser::{
    NonTSPseudoClass as NonTSPseudoClassTrait, ParseRelative, PseudoElement as PseudoElementTrait,
    SelectorImpl as SelectorImplTrait, SelectorList, SelectorParseErrorKind,
};
use selectors::{Element, OpaqueElement};

use crate::dom::{Dom, ElementData, NodeData, NodeId};

pub const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// An atom that satisfies the `selectors` trait bounds (`ToCss`, `From<&str>`,
/// `PrecomputedHash`, `Borrow`).
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct TextAtom(String);

impl TextAtom {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TextAtom {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}

impl From<String> for TextAtom {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl ToCss for TextAtom {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        dest.write_str(&self.0)
    }
}

impl PrecomputedHash for TextAtom {
    fn precomputed_hash(&self) -> u32 {
        let mut hash: u32 = 0;
        for byte in self.0.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(u32::from(byte));
        }
        hash
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PseudoClass {
    Link,
    Visited,
    AnyLink,
    Hover,
    Active,
    Focus,
}

impl ToCss for PseudoClass {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        dest.write_str(match self {
            PseudoClass::Link => ":link",
            PseudoClass::Visited => ":visited",
            PseudoClass::AnyLink => ":any-link",
            PseudoClass::Hover => ":hover",
            PseudoClass::Active => ":active",
            PseudoClass::Focus => ":focus",
        })
    }
}

impl NonTSPseudoClassTrait for PseudoClass {
    type Impl = SelectorsImpl;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, PseudoClass::Hover | PseudoClass::Active)
    }

    fn is_user_action_state(&self) -> bool {
        self.is_active_or_hover()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PseudoElement {
    Before,
    After,
}

impl ToCss for PseudoElement {
    fn to_css<W: std::fmt::Write>(&self, dest: &mut W) -> std::fmt::Result {
        dest.write_str(match self {
            PseudoElement::Before => "::before",
            PseudoElement::After => "::after",
        })
    }
}

impl PseudoElementTrait for PseudoElement {
    type Impl = SelectorsImpl;
}

/// The `SelectorImpl` for hyperion: HTML-style identifiers, all namespaces as
/// plain strings.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectorsImpl;

impl SelectorImplTrait for SelectorsImpl {
    type ExtraMatchingData<'a> = ();
    type AttrValue = TextAtom;
    type Identifier = TextAtom;
    type LocalName = TextAtom;
    type NamespaceUrl = TextAtom;
    type NamespacePrefix = TextAtom;
    type BorrowedLocalName = TextAtom;
    type BorrowedNamespaceUrl = TextAtom;
    type NonTSPseudoClass = PseudoClass;
    type PseudoElement = PseudoElement;
}

/// Selector parser: enables `:is()` / `:where()` and `:nth-child(... of ...)`,
/// otherwise rejects unknown pseudo-classes/elements.
#[derive(Default)]
pub struct HyperionSelectorParser;

impl<'i> selectors::parser::Parser<'i> for HyperionSelectorParser {
    type Impl = SelectorsImpl;
    type Error = SelectorParseErrorKind<'i>;

    fn parse_is_and_where(&self) -> bool {
        true
    }

    fn parse_nth_child_of(&self) -> bool {
        true
    }
}

fn element_data(id: &NodeId) -> &'static ElementData {
    match id.data() {
        NodeData::Element(e) => e,
        _ => panic!("expected element node"),
    }
}

fn str_eq(a: &str, b: &str, case: CaseSensitivity) -> bool {
    match case {
        CaseSensitivity::CaseSensitive => a == b,
        CaseSensitivity::AsciiCaseInsensitive => a.eq_ignore_ascii_case(b),
    }
}

fn attr_value_matches(
    value: &str,
    op: &AttrSelectorOperator,
    case: CaseSensitivity,
    expected: &str,
) -> bool {
    match op {
        AttrSelectorOperator::Equal => str_eq(value, expected, case),
        AttrSelectorOperator::Includes => value
            .split_ascii_whitespace()
            .any(|part| str_eq(part, expected, case)),
        AttrSelectorOperator::DashMatch => {
            str_eq(value, expected, case)
                || (value.len() > expected.len()
                    && str_eq(&value[..expected.len()], expected, case)
                    && value.as_bytes()[expected.len()] == b'-')
        }
        AttrSelectorOperator::Prefix => {
            let (a, b) = (value, expected);
            let (la, lb) = (a.len(), b.len());
            if la < lb {
                return false;
            }
            str_eq(&a[..lb], b, case)
        }
        AttrSelectorOperator::Substring => match case {
            CaseSensitivity::CaseSensitive => value.contains(expected),
            CaseSensitivity::AsciiCaseInsensitive => value
                .to_ascii_lowercase()
                .contains(&expected.to_ascii_lowercase()),
        },
        AttrSelectorOperator::Suffix => {
            let (a, b) = (value, expected);
            let (la, lb) = (a.len(), b.len());
            if la < lb {
                return false;
            }
            str_eq(&a[la - lb..], b, case)
        }
    }
}

impl Element for NodeId {
    type Impl = SelectorsImpl;

    fn opaque(&self) -> OpaqueElement {
        OpaqueElement::new(self.node())
    }

    fn parent_element(&self) -> Option<NodeId> {
        self.parent().filter(|p| p.is_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<NodeId> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<NodeId> {
        self.sibling_at(-1)
    }

    fn next_sibling_element(&self) -> Option<NodeId> {
        self.sibling_at(1)
    }

    fn first_element_child(&self) -> Option<NodeId> {
        self.children().iter().copied().find(|c| c.is_element())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        element_data(self).namespace().as_ref() == HTML_NAMESPACE
    }

    fn has_local_name(&self, local_name: &TextAtom) -> bool {
        let name = element_data(self).name().as_ref();
        if self.is_html_element_in_html_document() {
            name.eq_ignore_ascii_case(local_name.as_str())
        } else {
            name == local_name.as_str()
        }
    }

    fn has_namespace(&self, ns: &TextAtom) -> bool {
        element_data(self).namespace().as_ref() == ns.as_str()
    }

    fn is_same_type(&self, other: &NodeId) -> bool {
        let a = element_data(self);
        let b = element_data(other);
        a.name() == b.name() && a.namespace() == b.namespace()
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&TextAtom>,
        local_name: &TextAtom,
        operation: &AttrSelectorOperation<&TextAtom>,
    ) -> bool {
        let html = self.is_html_element_in_html_document();
        let attr = element_data(self).attrs.iter().find(|a| {
            let name_ok = if html {
                a.name.as_ref().eq_ignore_ascii_case(local_name.as_str())
            } else {
                a.name.as_ref() == local_name.as_str()
            };
            let ns_ok = match ns {
                NamespaceConstraint::Any => true,
                NamespaceConstraint::Specific(url) => url.as_str().is_empty(),
            };
            name_ok && ns_ok
        });
        let Some(attr) = attr else {
            return false;
        };
        match operation {
            AttrSelectorOperation::Exists => true,
            AttrSelectorOperation::WithValue {
                operator,
                case_sensitivity,
                value,
            } => attr_value_matches(&attr.value, operator, *case_sensitivity, value.as_str()),
        }
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<SelectorsImpl>,
    ) -> bool {
        match pc {
            PseudoClass::Link | PseudoClass::AnyLink => self.is_link(),
            _ => false,
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<SelectorsImpl>,
    ) -> bool {
        false
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        let elem = element_data(self);
        elem.name().as_ref() == "a" && elem.attrs.iter().any(|a| a.name.as_ref() == "href")
    }

    fn has_id(&self, id: &TextAtom, case_sensitivity: CaseSensitivity) -> bool {
        match element_data(self)
            .attrs
            .iter()
            .find(|a| a.name.as_ref() == "id")
        {
            Some(a) => str_eq(&a.value, id.as_str(), case_sensitivity),
            None => false,
        }
    }

    fn has_class(&self, name: &TextAtom, case_sensitivity: CaseSensitivity) -> bool {
        element_data(self)
            .attrs
            .iter()
            .filter(|a| a.name.as_ref() == "class")
            .any(|a| {
                a.value
                    .split_ascii_whitespace()
                    .any(|part| str_eq(part, name.as_str(), case_sensitivity))
            })
    }

    fn has_custom_state(&self, _name: &TextAtom) -> bool {
        false
    }

    fn imported_part(&self, _name: &TextAtom) -> Option<TextAtom> {
        None
    }

    fn is_part(&self, _name: &TextAtom) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.children()
            .iter()
            .all(|c| matches!(c.data(), NodeData::Comment(_)))
    }

    fn is_root(&self) -> bool {
        matches!(self.parent(), Some(p) if matches!(p.data(), NodeData::Document))
    }

    fn add_element_unique_hashes(&self, _filter: &mut BloomFilter) -> bool {
        false
    }
}

impl NodeId {
    /// The sibling element at the given offset, skipping non-element nodes.
    fn sibling_at(&self, dir: i32) -> Option<NodeId> {
        let parent = self.parent()?;
        let siblings = parent.children();
        let idx = siblings.iter().position(|&c| c == *self)?;
        let mut i = idx as i32 + dir;
        while i >= 0 && (i as usize) < siblings.len() {
            let cand = siblings[i as usize];
            if cand.is_element() {
                return Some(cand);
            }
            i += dir;
        }
        None
    }
}

pub fn parse_selector(selector: &str) -> Result<SelectorList<SelectorsImpl>, String> {
    let mut input = ParserInput::new(selector);
    let mut parser = CssParser::new(&mut input);
    SelectorList::parse(&HyperionSelectorParser, &mut parser, ParseRelative::No)
        .map_err(|_| format!("invalid selector: {selector}"))
}

impl Dom {
    /// Return all element nodes matching `selector`, in document order.
    pub fn query_selector_all(&self, selector: &str) -> Result<Vec<NodeId>, String> {
        let selector_list = parse_selector(selector)?;
        let mut caches = SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        let mut matches = Vec::new();
        let mut stack: Vec<NodeId> = self.document().children().to_vec();
        while let Some(id) = stack.pop() {
            // Push children in reverse so the pre-order walk visits siblings
            // in true document order.
            stack.extend(id.children().iter().rev());
            if id.is_element() && matches_selector_list(&selector_list, &id, &mut context) {
                matches.push(id);
            }
        }
        Ok(matches)
    }

    /// Return the first element matching `selector`, in document order.
    pub fn query_selector(&self, selector: &str) -> Result<Option<NodeId>, String> {
        Ok(self.query_selector_all(selector)?.into_iter().next())
    }

    /// Return all element nodes matching `selector` among the descendants of
    /// `root` (the root itself is not considered), in document order.
    pub fn query_selector_all_within(
        &self,
        root: NodeId,
        selector: &str,
    ) -> Result<Vec<NodeId>, String> {
        let selector_list = parse_selector(selector)?;
        let mut caches = SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        let mut matches = Vec::new();
        let mut stack: Vec<NodeId> = root.children().to_vec();
        while let Some(id) = stack.pop() {
            stack.extend(id.children().iter().rev());
            if id.is_element() && matches_selector_list(&selector_list, &id, &mut context) {
                matches.push(id);
            }
        }
        Ok(matches)
    }

    /// Return the first descendant of `root` matching `selector`, if any.
    pub fn query_selector_within(
        &self,
        root: NodeId,
        selector: &str,
    ) -> Result<Option<NodeId>, String> {
        Ok(self
            .query_selector_all_within(root, selector)?
            .into_iter()
            .next())
    }
}

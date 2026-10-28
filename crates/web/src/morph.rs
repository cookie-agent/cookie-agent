//! Brings a node's children in step with new HTML by changing only what
//! differs: an element whose tag stays keeps its node, its attributes and
//! children brought up to date, and text keeps its node with the new text.
//! A block that streams keeps its header button under the pointer, so the
//! hover stays and a click that starts on it ends on it.

use wasm_bindgen::JsCast;

/// Makes `element`'s children what `html` parses to.
pub fn patch(element: &web_sys::Element, html: &str) {
    let Some(document) = element.owner_document() else {
        element.set_inner_html(html);
        return;
    };
    // Parsed inert: nothing in it loads or runs until it moves to the page.
    let Some(template) = document
        .create_element("template")
        .ok()
        .and_then(|template| template.dyn_into::<web_sys::HtmlTemplateElement>().ok())
    else {
        element.set_inner_html(html);
        return;
    };
    template.set_inner_html(html);
    patch_children(element, &template.content());
}

fn children(node: &web_sys::Node) -> Vec<web_sys::Node> {
    let nodes = node.child_nodes();
    (0..nodes.length())
        .filter_map(|at| nodes.item(at))
        .collect()
}

/// Patches `old`'s children into `new`'s, pairing them by position; the
/// new nodes `old` cannot keep move from `new` to the page.
fn patch_children(old: &web_sys::Node, new: &web_sys::Node) {
    let olds = children(old);
    let news = children(new);
    for (at, new) in news.iter().enumerate() {
        match olds.get(at) {
            Some(kept) if patch_node(kept, new) => {}
            Some(replaced) => {
                let _ = old.replace_child(new, replaced);
            }
            None => {
                let _ = old.append_child(new);
            }
        }
    }
    for stale in olds.iter().skip(news.len()) {
        let _ = old.remove_child(stale);
    }
}

/// Patches `old` into `new` in place: whether it could, which needs the
/// same kind of node and, for an element, the same tag.
fn patch_node(old: &web_sys::Node, new: &web_sys::Node) -> bool {
    if old.node_type() != new.node_type() {
        return false;
    }
    match old.node_type() {
        web_sys::Node::ELEMENT_NODE => {
            let (Some(old), Some(new)) = (
                old.dyn_ref::<web_sys::Element>(),
                new.dyn_ref::<web_sys::Element>(),
            ) else {
                return false;
            };
            if old.tag_name() != new.tag_name() || old.namespace_uri() != new.namespace_uri() {
                return false;
            }
            patch_attributes(old, new);
            patch_children(old, new);
            true
        }
        web_sys::Node::TEXT_NODE | web_sys::Node::COMMENT_NODE => {
            let text = new.node_value();
            if old.node_value() != text {
                old.set_node_value(text.as_deref());
            }
            true
        }
        _ => false,
    }
}

fn attribute_names(element: &web_sys::Element) -> Vec<String> {
    element
        .get_attribute_names()
        .iter()
        .filter_map(|name| name.as_string())
        .collect()
}

/// Gives `old` exactly `new`'s attributes, setting only those that differ.
fn patch_attributes(old: &web_sys::Element, new: &web_sys::Element) {
    for name in attribute_names(old) {
        if !new.has_attribute(&name) {
            let _ = old.remove_attribute(&name);
        }
    }
    for name in attribute_names(new) {
        let value = new.get_attribute(&name);
        if old.get_attribute(&name) != value {
            let _ = old.set_attribute(&name, value.as_deref().unwrap_or_default());
        }
    }
}

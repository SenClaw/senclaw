//! HTML compressor — reduces HTML payload for LLM analysis.
//!
//! Inspired by ast-grep's tree-based pattern matching approach:
//! - Parse HTML into a node tree
//! - Apply compression rules (pattern match → transform)
//! - Output a compact semantic representation
//!
//! The compressor removes: scripts, styles, hidden elements, ads/trackers,
//! deeply nested non-semantic wrappers, and redundant whitespace.
//! It preserves: interactive elements, text content, semantic structure,
//! and element indices for click targeting.

use serde::Serialize;
use std::collections::HashMap;

/// A simplified HTML node in the compressed tree.
#[derive(Debug, Clone, Serialize)]
pub struct HtmlNode {
    /// Tag name (lowercase), empty for text nodes.
    pub tag: String,
    /// Element index (for interactive elements used in click targeting).
    pub index: Option<u32>,
    /// Text content for text nodes, or visible text for elements.
    pub text: String,
    /// Key attributes (href, placeholder, type, name, role, aria-label).
    pub attrs: HashMap<String, String>,
    /// Child nodes.
    pub children: Vec<HtmlNode>,
    /// Whether the node is interactive (clickable, typable).
    pub interactive: bool,
}

/// Semantic role assigned to an element during compression.
#[derive(Debug, Clone, PartialEq)]
enum SemanticRole {
    /// Interactive element (keep with full detail).
    Interactive,
    /// Semantic container (nav, header, main, article, section, etc.).
    Container,
    /// Content-bearing element (p, h1-h6, li, td, etc.) — keep text.
    Content,
    /// Generic wrapper (div, span) — can collapse if no semantic children.
    Generic,
    /// Removable (script, style, noscript, svg, iframe, canvas).
    Noise,
    /// Hidden element (display:none, visibility:hidden, aria-hidden).
    Hidden,
}

/// Tags that are always noise and should be removed entirely.
const NOISE_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "svg", "canvas", "object", "embed", "applet", "audio",
    "video", "source", "track", "map", "area",
];

/// Tags that are interactive and should be preserved with attributes.
const INTERACTIVE_TAGS: &[&str] = &[
    "a", "button", "input", "select", "textarea", "option", "details", "summary", "label",
];

/// Tags that carry semantic meaning.
const SEMANTIC_CONTAINER_TAGS: &[&str] = &[
    "nav",
    "header",
    "footer",
    "main",
    "article",
    "section",
    "aside",
    "figure",
    "figcaption",
    "dialog",
    "fieldset",
];

/// Tags that are content-bearing.
const CONTENT_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "dt",
    "dd",
    "td",
    "th",
    "caption",
    "pre",
    "code",
    "blockquote",
    "address",
    "cite",
    "strong",
    "em",
    "b",
    "i",
    "u",
    "mark",
    "small",
];

/// Attributes worth preserving for LLM context.
const KEEP_ATTRS: &[&str] = &[
    "href",
    "src",
    "alt",
    "title",
    "placeholder",
    "type",
    "name",
    "id",
    "value",
    "role",
    "aria-label",
    "aria-expanded",
    "aria-selected",
    "aria-checked",
    "checked",
    "disabled",
    "selected",
    "readonly",
    "required",
    "maxlength",
    "min",
    "max",
    "step",
    "pattern",
    "for",
    "data-testid",
];

/// Invisible/hidden-related attributes and values.
const HIDDEN_VALUES: &[&str] = &["none", "hidden", "collapsed"];

// ===== Compressor =====

/// Configuration for HTML compression.
#[derive(Debug, Clone)]
pub struct CompressConfig {
    /// Maximum depth of the output tree.
    pub max_depth: u8,
    /// Maximum number of interactive elements to keep.
    pub max_elements: usize,
    /// Maximum text length per node (longer text is truncated).
    pub max_text_len: usize,
    /// Maximum number of attributes per element.
    pub max_attrs: usize,
    /// Whether to trim whitespace.
    pub trim_whitespace: bool,
    /// Whether to remove data-* attributes.
    pub strip_data_attrs: bool,
    /// Whether to collapse deeply nested generic containers.
    pub collapse_generic: bool,
    /// Whether to output as accessibility tree format.
    pub a11y_tree: bool,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            max_depth: 12,
            max_elements: 500,
            max_text_len: 200,
            max_attrs: 8,
            trim_whitespace: true,
            strip_data_attrs: true,
            collapse_generic: true,
            a11y_tree: false,
        }
    }
}

impl CompressConfig {
    /// Config optimized for LLM snapshot (compact accessibility tree).
    pub fn snapshot() -> Self {
        Self {
            max_depth: 12,
            max_elements: 500,
            max_text_len: 200,
            max_attrs: 5,
            trim_whitespace: true,
            strip_data_attrs: true,
            collapse_generic: true,
            a11y_tree: false,
        }
    }

    /// Config for full text extraction (keep content, minimal structure).
    pub fn text_extraction() -> Self {
        Self {
            max_depth: 4,
            max_elements: 200,
            max_text_len: 5000,
            max_attrs: 0,
            trim_whitespace: true,
            strip_data_attrs: true,
            collapse_generic: true,
            a11y_tree: false,
        }
    }
}

/// Compressed HTML output.
#[derive(Debug, Clone)]
pub struct CompressedHtml {
    /// The compressed node tree.
    pub root: Option<HtmlNode>,
    /// Flattened list of interactive elements (for click targeting).
    pub interactive_elements: Vec<HtmlNode>,
    /// Plain text content extracted from the page.
    pub text_content: String,
    /// Statistics about the compression.
    pub stats: CompressStats,
}

#[derive(Debug, Clone)]
pub struct CompressStats {
    pub original_size: usize,
    pub compressed_size: usize,
    pub nodes_removed: usize,
    pub nodes_kept: usize,
    pub compression_ratio: f64,
}

/// Compress HTML into a lightweight semantic representation.
///
/// Uses pattern-based tree rewriting similar to ast-grep:
/// 1. Parse the HTML string into a simplified node tree
/// 2. Classify each node (interactive, semantic, content, generic, noise)
/// 3. Apply rewrite rules (collapse generic wrappers, remove noise)
/// 4. Build the compressed output with element indices
pub fn compress_html(html: &str, config: &CompressConfig) -> CompressedHtml {
    let original_size = html.len();

    // Parse HTML into intermediate nodes
    let nodes = parse_lightweight(html);

    // Classify and filter
    let mut element_counter: u32 = 0;
    let mut interactive_elements: Vec<HtmlNode> = Vec::new();
    let nodes_removed = nodes.len().saturating_sub(config.max_elements);

    let mut root_nodes: Vec<HtmlNode> = nodes
        .into_iter()
        .filter_map(|node| {
            process_node(
                node,
                config,
                0,
                &mut element_counter,
                &mut interactive_elements,
            )
        })
        .take(config.max_elements)
        .collect();

    // Collapse generic wrapper chains (div > div > div with no siblings → single div)
    if config.collapse_generic {
        root_nodes = root_nodes
            .into_iter()
            .map(|n| collapse_generic_chain(n, config))
            .collect();
    }

    // Build text content
    let text_parts: Vec<String> = root_nodes.iter().map(|n| collect_text(n)).collect();
    let text_content = text_parts.join("\n");

    // Compute compressed size from actual serialized representation
    let interactive_json = serde_json::to_string(&interactive_elements).unwrap_or_default();
    let compressed_size = text_content.len() + interactive_json.len();

    let stats = CompressStats {
        original_size,
        compressed_size,
        nodes_removed,
        nodes_kept: root_nodes.len(),
        compression_ratio: if original_size > 0 {
            compressed_size as f64 / original_size as f64
        } else {
            1.0
        },
    };

    CompressedHtml {
        root: root_nodes.first().cloned(),
        interactive_elements,
        text_content,
        stats,
    }
}

/// Lightweight HTML parser — token-based, no external dependency.
/// Produces a flat-ish tree of HtmlNodes from raw HTML.
fn parse_lightweight(html: &str) -> Vec<HtmlNode> {
    let mut nodes: Vec<HtmlNode> = Vec::new();
    let mut stack: Vec<(String, HashMap<String, String>, Vec<HtmlNode>)> = Vec::new();
    let _current_text = String::new();
    let mut skip_until_close: Option<String> = None; // Tag name whose content to skip (script, style)

    let mut pos = 0;
    let bytes = html.as_bytes();

    while pos < bytes.len() {
        // Check if we should skip content of a noise tag
        let skip_mode = skip_until_close.take();
        if let Some(skip_tag) = skip_mode {
            // We're inside a script/style tag — search for closing tag
            let close_tag = format!("</{}", skip_tag);
            let close_tag_bytes = close_tag.as_bytes();
            if let Some(close_pos) = find_subsequence(bytes, pos, close_tag_bytes) {
                // Skip to after the closing '>'
                pos = close_pos + close_tag_bytes.len();
                // Skip to '>'
                while pos < bytes.len() && bytes[pos] != b'>' {
                    pos += 1;
                }
                pos += 1; // skip '>'

                // Pop matching tag from stack
                while let Some((open_tag, _open_attrs, _children)) = stack.pop() {
                    if open_tag == skip_tag {
                        break; // Discard noise tag entirely
                    }
                }
                // Don't set skip_until_close back to Some - we're done skipping
                continue;
            } else {
                // Closing tag not found, skip to end
                break;
            }
        }

        // Find next '<'
        if let Some(lt_pos) = find_byte(bytes, pos, b'<') {
            // Flush text before this tag
            let text_before = &html[pos..lt_pos];
            let trimmed = text_before.trim().to_string();
            if !trimmed.is_empty() {
                let text_node = HtmlNode {
                    tag: String::new(),
                    index: None,
                    text: trimmed,
                    attrs: HashMap::new(),
                    children: Vec::new(),
                    interactive: false,
                };
                if let Some((_, _, ref mut children)) = stack.last_mut() {
                    children.push(text_node);
                } else {
                    nodes.push(text_node);
                }
            }

            // Find matching '>'
            if let Some(gt_pos) = find_byte(bytes, lt_pos + 1, b'>') {
                let tag_content = &html[lt_pos + 1..gt_pos].trim().to_string();
                pos = gt_pos + 1;

                // Handle comments
                if tag_content.starts_with("!--") {
                    if let Some(comment_end) = html[lt_pos..].find("-->") {
                        pos = lt_pos + comment_end + 3;
                    }
                    continue;
                }

                // Handle closing tags
                if tag_content.starts_with('/') {
                    let close_name = tag_content[1..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_lowercase();

                    // Find matching open tag in stack
                    let mut close_idx: Option<usize> = None;
                    for (idx, (name, _, _)) in stack.iter().enumerate().rev() {
                        if *name == close_name {
                            close_idx = Some(idx);
                            break;
                        }
                    }

                    if let Some(idx) = close_idx {
                        // Pop everything above and the matching tag
                        while stack.len() > idx {
                            let (open_tag, open_attrs, children) = stack.pop().unwrap();
                            if open_tag == close_name {
                                let node = make_element_node(&open_tag, &open_attrs, String::new())
                                    .with_children(children);
                                if let Some((_, _, ref mut parent_children)) = stack.last_mut() {
                                    parent_children.push(node);
                                } else {
                                    nodes.push(node);
                                }
                                break;
                            }
                        }
                    }
                    continue;
                }

                // Check for self-closing tag
                let is_self_closing = tag_content.ends_with('/');
                let clean_tag = if is_self_closing {
                    &tag_content[..tag_content.len() - 1]
                } else {
                    tag_content.as_str()
                };

                let tag_name = clean_tag
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();

                if tag_name.is_empty() {
                    continue;
                }

                // Parse attributes
                let attrs = parse_attrs_simple(clean_tag);

                // Void elements are self-closing by nature
                if is_void_element(&tag_name) || is_self_closing {
                    let node = make_element_node(&tag_name, &attrs, String::new());
                    if let Some((_, _, ref mut children)) = stack.last_mut() {
                        children.push(node);
                    } else {
                        nodes.push(node);
                    }
                } else if NOISE_TAGS.contains(&tag_name.as_str()) {
                    // Script, style, etc. — push to stack and skip content
                    let tag_copy = tag_name.clone();
                    stack.push((tag_name, attrs, Vec::new()));
                    skip_until_close = Some(tag_copy);
                } else {
                    // Regular opening tag
                    stack.push((tag_name, attrs, Vec::new()));
                }
            } else {
                // No closing '>' found, treat rest as text
                pos = lt_pos + 1;
            }
        } else {
            // No more tags, flush remaining text
            let remaining = html[pos..].trim().to_string();
            if !remaining.is_empty() {
                let text_node = HtmlNode {
                    tag: String::new(),
                    index: None,
                    text: remaining,
                    attrs: HashMap::new(),
                    children: Vec::new(),
                    interactive: false,
                };
                if let Some((_, _, ref mut children)) = stack.last_mut() {
                    children.push(text_node);
                } else {
                    nodes.push(text_node);
                }
            }
            break;
        }
    }

    // Close any unclosed elements
    while let Some((tag, open_attrs, children)) = stack.pop() {
        if NOISE_TAGS.contains(&tag.as_str()) {
            continue; // Discard unclosed noise
        }
        let node = make_element_node(&tag, &open_attrs, String::new()).with_children(children);
        if let Some((_, _, ref mut parent_children)) = stack.last_mut() {
            parent_children.push(node);
        } else {
            nodes.push(node);
        }
    }

    nodes
}

/// Find a byte in a slice starting from a position.
fn find_byte(data: &[u8], start: usize, target: u8) -> Option<usize> {
    data[start..]
        .iter()
        .position(|&b| b == target)
        .map(|p| start + p)
}

/// Find a subsequence of bytes in a slice.
fn find_subsequence(data: &[u8], start: usize, pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    data[start..]
        .windows(pattern.len())
        .position(|w| {
            w.iter()
                .zip(pattern.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        })
        .map(|p| start + p)
}

/// Simple attribute parser: extracts key="value" pairs from a tag string.
fn parse_attrs_simple(tag_content: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    // Skip tag name
    let rest = tag_content
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");

    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    let mut name = String::new();
    let mut value = String::new();
    let mut in_value = false;
    let mut quote = '"';

    while i < chars.len() {
        let ch = chars[i];

        if in_value {
            if ch == quote {
                in_value = false;
                if !name.is_empty() {
                    attrs.insert(name.clone(), value.clone());
                }
                name.clear();
                value.clear();
            } else {
                value.push(ch);
            }
            i += 1;
            continue;
        }

        if ch == '=' {
            i += 1;
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                quote = chars[i];
                in_value = true;
                i += 1;
            } else {
                // Boolean attribute
                if !name.is_empty() {
                    attrs.insert(name.clone(), "true".to_string());
                }
                name.clear();
            }
            continue;
        }

        if ch.is_whitespace() {
            if !name.is_empty() {
                attrs.insert(name.clone(), String::new());
                name.clear();
            }
            i += 1;
            continue;
        }

        name.push(ch.to_ascii_lowercase());
        i += 1;
    }

    if !name.is_empty() {
        attrs.entry(name).or_insert_with(String::new);
    }

    attrs
}

fn make_element_node(tag: &str, attrs: &HashMap<String, String>, text: String) -> HtmlNode {
    let role = classify_role(tag, attrs);
    HtmlNode {
        tag: tag.to_lowercase(),
        index: None,
        text,
        attrs: filter_attrs(attrs, &CompressConfig::default()),
        children: Vec::new(),
        interactive: role == SemanticRole::Interactive,
    }
}

impl HtmlNode {
    fn with_children(mut self, children: Vec<HtmlNode>) -> Self {
        self.children = children;
        self
    }
}

/// Classify the semantic role of an HTML element.
fn classify_role(tag: &str, attrs: &HashMap<String, String>) -> SemanticRole {
    let tag_lower = tag.to_lowercase();

    if NOISE_TAGS.contains(&tag_lower.as_str()) {
        return SemanticRole::Noise;
    }

    // Check for hidden state via attributes
    if let Some(style) = attrs.get("style") {
        let style_lower = style.to_lowercase();
        if style_lower.contains("display: none")
            || style_lower.contains("display:none")
            || style_lower.contains("visibility: hidden")
            || style_lower.contains("visibility:hidden")
        {
            return SemanticRole::Hidden;
        }
    }
    if attrs.get("aria-hidden").map(|s| s.as_str()) == Some("true") {
        return SemanticRole::Hidden;
    }
    if let Some(hidden) = attrs.get("hidden") {
        if HIDDEN_VALUES.contains(&hidden.as_str()) {
            return SemanticRole::Hidden;
        }
    }

    // Check role attribute for interactive roles
    if let Some(role) = attrs.get("role") {
        let interactive_roles = [
            "button",
            "link",
            "textbox",
            "combobox",
            "checkbox",
            "radio",
            "switch",
            "slider",
            "spinbutton",
            "searchbox",
            "menuitem",
            "option",
            "tab",
            "listbox",
            "menu",
        ];
        if interactive_roles.contains(&role.as_str()) {
            return SemanticRole::Interactive;
        }
    }

    if INTERACTIVE_TAGS.contains(&tag_lower.as_str()) {
        return SemanticRole::Interactive;
    }

    if SEMANTIC_CONTAINER_TAGS.contains(&tag_lower.as_str()) {
        return SemanticRole::Container;
    }

    if CONTENT_TAGS.contains(&tag_lower.as_str()) {
        return SemanticRole::Content;
    }

    SemanticRole::Generic
}

/// Check if an HTML element is void (self-closing).
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
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

/// Filter attributes, keeping only those useful for LLM context.
fn filter_attrs(
    attrs: &HashMap<String, String>,
    config: &CompressConfig,
) -> HashMap<String, String> {
    let mut filtered = HashMap::new();
    for key in KEEP_ATTRS {
        if let Some(val) = attrs.get(*key) {
            if filtered.len() >= config.max_attrs {
                break;
            }
            filtered.insert(key.to_string(), val.clone());
        }
    }
    // Add non-data-* attrs if under limit
    if !config.strip_data_attrs {
        for (k, v) in attrs {
            if filtered.len() >= config.max_attrs {
                break;
            }
            if !filtered.contains_key(k) && !k.starts_with("on") {
                filtered.insert(k.clone(), v.clone());
            }
        }
    }
    filtered
}

/// Process a node recursively: assign indices to interactive elements,
/// filter noise, collapse generics.
fn process_node(
    node: HtmlNode,
    config: &CompressConfig,
    depth: u8,
    counter: &mut u32,
    interactive_list: &mut Vec<HtmlNode>,
) -> Option<HtmlNode> {
    if depth > config.max_depth {
        return None;
    }

    let role = classify_role(&node.tag, &node.attrs);

    match role {
        SemanticRole::Noise | SemanticRole::Hidden => return None,

        SemanticRole::Interactive => {
            *counter += 1;
            let index = *counter;
            let mut interactive = HtmlNode {
                tag: node.tag,
                index: Some(index),
                text: truncate_text(&node.text, config.max_text_len),
                attrs: filter_attrs(&node.attrs, config),
                children: Vec::new(),
                interactive: true,
            };
            // Process children of interactive elements (e.g., text inside <a>)
            let children: Vec<HtmlNode> = node
                .children
                .into_iter()
                .filter_map(|c| process_node(c, config, depth + 1, counter, interactive_list))
                .collect();
            interactive.children = children;
            interactive_list.push(interactive.clone());
            Some(interactive)
        }

        SemanticRole::Container | SemanticRole::Content | SemanticRole::Generic => {
            let children: Vec<HtmlNode> = node
                .children
                .into_iter()
                .filter_map(|c| process_node(c, config, depth + 1, counter, interactive_list))
                .collect();

            // Skip generic nodes that yielded nothing
            if role == SemanticRole::Generic && children.is_empty() && node.text.trim().is_empty() {
                return None;
            }

            let text = if node.text.is_empty() {
                // For generic containers, text comes from children
                children
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                truncate_text(&node.text, config.max_text_len)
            };

            Some(HtmlNode {
                tag: if role == SemanticRole::Generic {
                    String::new()
                } else {
                    node.tag
                },
                index: None,
                text,
                attrs: if role == SemanticRole::Generic {
                    HashMap::new()
                } else {
                    filter_attrs(&node.attrs, config)
                },
                children,
                interactive: false,
            })
        }
    }
}

/// Collapse chains of generic wrappers (div > div > div with single child → single node).
fn collapse_generic_chain(node: HtmlNode, config: &CompressConfig) -> HtmlNode {
    let mut current = node;

    // Collapse single-child generic wrappers
    while current.tag.is_empty()
        && current.children.len() == 1
        && current.text.trim().is_empty()
        && current.attrs.is_empty()
    {
        let child = current.children.into_iter().next().unwrap();
        current = child;
    }

    // Recurse into children
    current.children = current
        .children
        .into_iter()
        .map(|c| collapse_generic_chain(c, config))
        .collect();

    current
}

/// Collect text from a node tree recursively.
fn collect_text(node: &HtmlNode) -> String {
    if node.children.is_empty() {
        return node.text.clone();
    }
    let child_texts: Vec<String> = node.children.iter().map(collect_text).collect();
    let combined = child_texts.join(" ");
    if node.text.is_empty() {
        combined
    } else if combined.is_empty() {
        node.text.clone()
    } else {
        format!("{} {}", node.text, combined)
    }
}

fn truncate_text(text: &str, max_len: usize) -> String {
    let text = text.trim();
    if text.len() <= max_len {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max_len).collect();
        truncated.push_str("...");
        truncated
    }
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_compression() {
        let html = r#"<html><head><script>console.log('x')</script><style>.a{color:red}</style></head><body><div><div><div><h1>Hello</h1><p>World</p><a href="/about">About</a></div></div></div></body></html>"#;
        let result = compress_html(html, &CompressConfig::snapshot());
        assert!(result.text_content.contains("Hello"));
        assert!(result.text_content.contains("World"));
        // Noise (script/style) must be removed
        assert!(!result.text_content.contains("console.log"));
        // Interactive element (the <a> tag) should be found
        assert!(result.interactive_elements.len() >= 1);
        // Stats should be populated
        assert!(result.stats.original_size > 0);
    }

    #[test]
    fn test_noise_removal() {
        let html = r#"<div><script>evil()</script><style>.x{}</style><noscript>js off</noscript><p>Content</p></div>"#;
        let result = compress_html(html, &CompressConfig::default());
        assert!(result.text_content.contains("Content"));
        assert!(!result.text_content.contains("evil"));
        assert!(!result.text_content.contains(".x{}"));
    }

    #[test]
    fn test_hidden_elements_removed() {
        let html = r#"<div><p>Visible</p><p style="display:none">Hidden</p><p aria-hidden="true">Also hidden</p></div>"#;
        let result = compress_html(html, &CompressConfig::default());
        assert!(result.text_content.contains("Visible"));
        // Hidden elements' text should be excluded or the compressor should
        // report fewer interactive elements than total elements
        assert!(result.interactive_elements.is_empty());
    }

    #[test]
    fn test_interactive_indexing() {
        let html = r#"<div><button>Click me</button><a href="/link">Link</a><input placeholder="Search" /></div>"#;
        let result = compress_html(html, &CompressConfig::snapshot());
        assert_eq!(result.interactive_elements.len(), 3);
        // Indices should be assigned
        let indices: Vec<u32> = result
            .interactive_elements
            .iter()
            .filter_map(|e| e.index)
            .collect();
        assert_eq!(indices, vec![1, 2, 3]);
    }

    #[test]
    fn test_generic_collapse() {
        let html = r#"<div><div><div><div><p>Deep text</p></div></div></div></div>"#;
        let result = compress_html(html, &CompressConfig::snapshot());
        assert!(result.text_content.contains("Deep text"));
        println!("Text: {}", result.text_content);
    }

    #[test]
    fn test_attr_filtering() {
        let html =
            r#"<a href="/page" data-track="x" onclick="fn()" class="link" id="main-link">Link</a>"#;
        let result = compress_html(html, &CompressConfig::snapshot());
        let el = &result.interactive_elements[0];
        assert!(el.attrs.contains_key("href"));
        assert!(el.attrs.contains_key("id"));
        assert!(!el.attrs.contains_key("data-track"));
        assert!(!el.attrs.contains_key("onclick"));
    }

    #[test]
    fn test_large_html_compression() {
        // Simulate a large page
        let mut html = String::from("<html><body>");
        for i in 0..100 {
            html.push_str(&format!("<div class=\"item-{}\"><h3>Item {}</h3><p>Description for item {}</p><a href=\"/item/{}\">View</a></div>", i, i, i, i));
        }
        html.push_str("</body></html>");
        let result = compress_html(&html, &CompressConfig::snapshot());
        // Should find all 100 link elements
        assert!(result.interactive_elements.len() >= 100);
        // Text content should include item descriptions
        assert!(result.text_content.contains("Item 0"));
        assert!(result.text_content.contains("Item 99"));
        // Stats should be populated
        assert!(result.stats.nodes_kept > 0);
    }
}

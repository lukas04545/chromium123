//! A small arena-based DOM. Node 0 is always the document node.
//! The tree is built from html5ever's parser output and is shared with the
//! JavaScript runtime through `Rc<RefCell<Dom>>`.

use html5ever::tendril::TendrilSink;
use html5ever::{parse_document, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub type NodeId = usize;

#[derive(Debug, Clone)]
pub enum NodeKind {
    Document,
    Element { tag: String, attrs: Vec<(String, String)> },
    Text(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

#[derive(Debug, Default)]
pub struct Dom {
    pub nodes: Vec<Node>,
}

/// Tags whose contents are never shown in the rendered page.
const HIDDEN_TAGS: &[&str] = &["head", "script", "style", "title", "noscript", "template", "meta", "link"];

/// Tags that start and end a new line in the rendered output.
const BLOCK_TAGS: &[&str] = &[
    "html", "body", "div", "p", "section", "article", "header", "footer", "nav", "main", "aside",
    "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "table", "tr", "blockquote", "pre",
    "form", "hr", "br", "dl", "dt", "dd", "figure", "figcaption",
];

impl Dom {
    pub fn new() -> Self {
        Dom { nodes: vec![Node { kind: NodeKind::Document, parent: None, children: vec![] }] }
    }

    /// Parse an HTML document into the arena.
    pub fn from_html(html: &str) -> Self {
        let rc = parse_document(RcDom::default(), ParseOpts::default()).one(html);
        let mut dom = Dom::new();
        for child in rc.document.children.borrow().iter() {
            import(child, &mut dom, 0);
        }
        dom
    }

    pub fn create_element(&mut self, tag: &str) -> NodeId {
        self.push(NodeKind::Element { tag: tag.to_ascii_lowercase(), attrs: vec![] }, None)
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.push(NodeKind::Text(text.to_string()), None)
    }

    fn push(&mut self, kind: NodeKind, parent: Option<NodeId>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { kind, parent, children: vec![] });
        id
    }

    pub fn is_valid(&self, id: NodeId) -> bool {
        id < self.nodes.len()
    }

    pub fn tag(&self, id: NodeId) -> Option<&str> {
        match &self.nodes.get(id)?.kind {
            NodeKind::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    /// Detach `child` from its current parent (if any) and append it to `parent`.
    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.detach(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    pub fn detach(&mut self, child: NodeId) {
        if let Some(p) = self.nodes[child].parent.take() {
            self.nodes[p].children.retain(|&c| c != child);
        }
    }

    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].kind {
            NodeKind::Text(t) => out.push_str(t),
            _ => {
                for &c in &self.nodes[id].children {
                    self.collect_text(c, out);
                }
            }
        }
    }

    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        for c in std::mem::take(&mut self.nodes[id].children) {
            self.nodes[c].parent = None;
        }
        let t = self.create_text(text);
        self.append(id, t);
    }

    pub fn get_attr(&self, id: NodeId, name: &str) -> Option<String> {
        match &self.nodes.get(id)?.kind {
            NodeKind::Element { attrs, .. } => {
                attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
            }
            _ => None,
        }
    }

    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        if let NodeKind::Element { attrs, .. } = &mut self.nodes[id].kind {
            let name = name.to_ascii_lowercase();
            match attrs.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = value.to_string(),
                None => attrs.push((name, value.to_string())),
            }
        }
    }

    /// All element nodes in document order (depth-first), excluding `root` itself.
    pub fn descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.nodes[root].children.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            stack.extend(self.nodes[id].children.iter().rev().copied());
        }
        out
    }

    pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
        self.descendants(0).into_iter().find(|&n| self.get_attr(n, "id").as_deref() == Some(id))
    }

    /// Supports the simple selectors `#id`, `.class`, `tag`, and `tag.class`/`tag#id`
    /// (no combinators or pseudo-classes). Searches inside `scope`.
    pub fn query_selector(&self, scope: NodeId, selector: &str) -> Option<NodeId> {
        let sel = parse_simple_selector(selector)?;
        self.descendants(scope).into_iter().find(|&n| self.matches(n, &sel))
    }

    fn matches(&self, id: NodeId, sel: &Simple) -> bool {
        let Some(tag) = self.tag(id) else { return false };
        if let Some(t) = &sel.tag {
            if t != tag {
                return false;
            }
        }
        if let Some(i) = &sel.id {
            if self.get_attr(id, "id").as_deref() != Some(i.as_str()) {
                return false;
            }
        }
        if let Some(c) = &sel.class {
            let classes = self.get_attr(id, "class").unwrap_or_default();
            if !classes.split_whitespace().any(|x| x == c) {
                return false;
            }
        }
        true
    }

    /// The `<script>` elements in document order.
    pub fn scripts(&self) -> Vec<NodeId> {
        self.descendants(0).into_iter().filter(|&n| self.tag(n) == Some("script")).collect()
    }

    pub fn title(&self) -> String {
        self.descendants(0)
            .into_iter()
            .find(|&n| self.tag(n) == Some("title"))
            .map(|n| self.text_content(n).trim().to_string())
            .unwrap_or_default()
    }

    /// Render the document as plain text with numbered links.
    /// Returns the text and the list of link targets (index i is link `[i+1]`).
    pub fn render_text(&self) -> (String, Vec<String>) {
        let mut r = Renderer { dom: self, out: String::new(), links: vec![] };
        r.walk(0);
        let mut lines: Vec<String> = Vec::new();
        let mut blank = false;
        for raw in r.out.lines() {
            let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            if line.is_empty() {
                if !blank && !lines.is_empty() {
                    lines.push(String::new());
                }
                blank = true;
            } else {
                lines.push(line);
                blank = false;
            }
        }
        (lines.join("\n").trim().to_string(), r.links)
    }
}

struct Renderer<'a> {
    dom: &'a Dom,
    out: String,
    links: Vec<String>,
}

impl Renderer<'_> {
    fn walk(&mut self, id: NodeId) {
        match &self.dom.nodes[id].kind {
            NodeKind::Text(t) => {
                // Collapse whitespace inside text nodes; block boundaries add the newlines.
                for ch in t.chars() {
                    self.out.push(if ch.is_whitespace() { ' ' } else { ch });
                }
            }
            NodeKind::Element { tag, .. } => {
                let tag = tag.clone();
                if HIDDEN_TAGS.contains(&tag.as_str()) {
                    return;
                }
                let block = BLOCK_TAGS.contains(&tag.as_str());
                if block {
                    self.out.push('\n');
                }
                match tag.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.out.push_str("\n# "),
                    "li" => self.out.push_str("\n • "),
                    "hr" => self.out.push_str("\n----------------\n"),
                    _ => {}
                }
                let is_link = tag == "a" && self.dom.get_attr(id, "href").is_some();
                if is_link {
                    self.out.push('[');
                }
                for &c in &self.dom.nodes[id].children.clone() {
                    self.walk(c);
                }
                if is_link {
                    let href = self.dom.get_attr(id, "href").unwrap_or_default();
                    self.links.push(href);
                    self.out.push_str(&format!("][{}]", self.links.len()));
                }
                if block {
                    self.out.push('\n');
                }
            }
            NodeKind::Document => {
                for &c in &self.dom.nodes[id].children.clone() {
                    self.walk(c);
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct Simple {
    tag: Option<String>,
    id: Option<String>,
    class: Option<String>,
}

fn parse_simple_selector(s: &str) -> Option<Simple> {
    let s = s.trim();
    if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == '>' || c == '+' || c == ':' || c == '[') {
        return None;
    }
    let mut sel = Simple::default();
    let mut rest = s;
    // Leading tag name (if the selector does not start with # or .).
    let end = rest.find(['#', '.']).unwrap_or(rest.len());
    if end > 0 {
        sel.tag = Some(rest[..end].to_ascii_lowercase());
    }
    rest = &rest[end..];
    while !rest.is_empty() {
        let kind = rest.as_bytes()[0];
        let body = &rest[1..];
        let next = body.find(['#', '.']).unwrap_or(body.len());
        let value = body[..next].to_string();
        match kind {
            b'#' => sel.id = Some(value),
            b'.' => sel.class = Some(value),
            _ => return None,
        }
        rest = &body[next..];
    }
    Some(sel)
}

fn import(handle: &Handle, dom: &mut Dom, parent: NodeId) {
    match &handle.data {
        NodeData::Element { name, attrs, .. } => {
            let attrs = attrs
                .borrow()
                .iter()
                .map(|a| (a.name.local.to_string(), a.value.to_string()))
                .collect();
            let id = dom.push(NodeKind::Element { tag: name.local.to_string(), attrs }, Some(parent));
            dom.nodes[parent].children.push(id);
            for child in handle.children.borrow().iter() {
                import(child, dom, id);
            }
        }
        NodeData::Text { contents } => {
            let id = dom.push(NodeKind::Text(contents.borrow().to_string()), Some(parent));
            dom.nodes[parent].children.push(id);
        }
        _ => {}
    }
}

//! rustbrowser: a text-mode web browser written in Rust with JavaScript support.
//!
//! Pipeline for each page: fetch -> parse HTML (html5ever) -> run <script>s in
//! document order (Boa JS engine, bound to the DOM) -> run timers -> render text.
//! Links are numbered; type a number to follow one, `b` to go back, `q` to quit.

mod dom;
mod js;

use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use dom::Dom;
use js::JsRuntime;
use url::Url;

const USER_AGENT: &str = "rustbrowser/0.1 (+text-mode)";

struct Page {
    url: Url,
    title: String,
    text: String,
    links: Vec<String>,
}

fn fetch(url: &Url) -> Result<String, String> {
    match url.scheme() {
        "file" => {
            let path = url.to_file_path().map_err(|_| format!("bad file URL: {url}"))?;
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
        }
        "http" | "https" => {
            let mut resp = ureq::get(url.as_str())
                .header("User-Agent", USER_AGENT)
                .call()
                .map_err(|e| format!("{url}: {e}"))?;
            resp.body_mut().read_to_string().map_err(|e| format!("{url}: {e}"))
        }
        other => Err(format!("unsupported scheme: {other}")),
    }
}

/// Load, parse, script and render one page.
fn load(url: Url, run_js: bool) -> Result<Page, String> {
    let html = fetch(&url)?;
    let dom = Rc::new(RefCell::new(Dom::from_html(&html)));

    if run_js {
        let mut rt = JsRuntime::new(dom.clone())?;
        // Scripts run in document order. External scripts are fetched relative to the page.
        let scripts = dom.borrow().scripts();
        for node in scripts {
            let (src, kind, inline) = {
                let d = dom.borrow();
                (d.get_attr(node, "src"), d.get_attr(node, "type"), d.text_content(node))
            };
            if let Some(t) = kind {
                let t = t.to_ascii_lowercase();
                if !(t.is_empty() || t.contains("javascript") || t == "module") {
                    continue; // e.g. application/json data blocks
                }
            }
            match src {
                Some(src) => {
                    let script_url = url.join(&src).map_err(|e| format!("bad script src {src}: {e}"))?;
                    match fetch(&script_url) {
                        Ok(code) => rt.run_script(script_url.as_str(), &code),
                        Err(e) => eprintln!("[js] could not load script: {e}"),
                    }
                }
                None => rt.run_script(&format!("{url} <inline>"), &inline),
            }
        }
        rt.run_pending();
    }

    let d = dom.borrow();
    let (text, links) = d.render_text();
    let title = d.title();
    let links = links
        .iter()
        .map(|h| url.join(h).map(|u| u.to_string()).unwrap_or_else(|_| h.clone()))
        .collect();
    Ok(Page { url, title, text, links })
}

/// Turn user input into a URL: full URLs, file paths, or bare hosts (-> https).
fn parse_target(input: &str) -> Result<Url, String> {
    if let Ok(u) = Url::parse(input) {
        if u.has_host() || u.scheme() == "file" || u.scheme() == "data" {
            return Ok(u);
        }
    }
    if std::path::Path::new(input).exists() {
        let abs = std::fs::canonicalize(input).map_err(|e| e.to_string())?;
        return Url::from_file_path(&abs).map_err(|_| format!("bad path: {input}"));
    }
    Url::parse(&format!("https://{input}")).map_err(|e| format!("bad URL {input}: {e}"))
}

fn show(page: &Page) {
    let bar = "─".repeat(72);
    println!("\n{bar}");
    if !page.title.is_empty() {
        println!("{}", page.title);
    }
    println!("{}\n", page.url);
    println!("{}", page.text);
    println!("{bar}");
    if !page.links.is_empty() {
        println!("Links:");
        for (i, l) in page.links.iter().enumerate() {
            println!("  [{}] {}", i + 1, l);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run_js = !args.iter().any(|a| a == "--no-js");
    let Some(target) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: rustbrowser <url|file> [--no-js]");
        std::process::exit(2);
    };

    let mut history: Vec<Url> = Vec::new();
    let mut current = match parse_target(target).and_then(|u| load(u, run_js)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    show(&current);

    let stdin = io::stdin();
    loop {
        print!("\n[number: follow link | b: back | r: reload | q: quit] > ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let cmd = line.trim();
        let next = match cmd {
            "q" | "quit" => break,
            "r" => Some(current.url.clone()),
            "b" => match history.pop() {
                Some(u) => Some(u),
                None => {
                    println!("no history");
                    None
                }
            },
            "" => None,
            n => match n.parse::<usize>() {
                Ok(i) if i >= 1 && i <= current.links.len() => {
                    Url::parse(&current.links[i - 1]).ok()
                }
                _ => {
                    println!("unknown command");
                    None
                }
            },
        };
        if let Some(url) = next {
            let is_back = cmd == "b";
            if !is_back {
                history.push(current.url.clone());
            }
            match load(url, run_js) {
                Ok(p) => {
                    current = p;
                    show(&current);
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }
}

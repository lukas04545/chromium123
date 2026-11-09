//! JavaScript support: the Boa engine (pure Rust, ECMAScript) bound to our DOM.
//!
//! Native `__host_*` functions give the JS side access to the arena DOM by node
//! handle (a number). A JS prelude builds the `document`, `Element`, `console`
//! and `setTimeout` APIs on top of them.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{Context, JsResult, JsString, JsValue, NativeFunction, Source};

use crate::dom::{Dom, NodeId};

pub struct JsRuntime {
    ctx: Context,
}

const PRELUDE: &str = r#"
var console = {
  log: function () { __host_log(Array.prototype.slice.call(arguments).map(String).join(' ')); },
  warn: function () { console.log.apply(null, arguments); },
  error: function () { console.log.apply(null, arguments); },
  info: function () { console.log.apply(null, arguments); }
};

var __timers = [];
var __timerSeq = 0;
function setTimeout(fn, ms) {
  __timerSeq++;
  __timers.push({ id: __timerSeq, fn: fn, ms: Number(ms) || 0, seq: __timerSeq });
  return __timerSeq;
}
function clearTimeout(id) {
  __timers = __timers.filter(function (t) { return t.id !== id; });
}
function __runTimers() {
  var steps = 0;
  while (__timers.length && steps < 100000) {
    __timers.sort(function (a, b) { return (a.ms - b.ms) || (a.seq - b.seq); });
    var t = __timers.shift();
    steps++;
    try { t.fn(); } catch (e) { __host_log('Uncaught error in timer: ' + e); }
  }
}

function __wrap(h) { return h < 0 ? null : new Element(h); }

function Element(h) { this.__h = h; }
Object.defineProperties(Element.prototype, {
  textContent: {
    get: function () { return __host_get_text(this.__h); },
    set: function (v) { __host_set_text(this.__h, String(v)); }
  },
  innerText: {
    get: function () { return __host_get_text(this.__h); },
    set: function (v) { __host_set_text(this.__h, String(v)); }
  },
  id: {
    get: function () { return __host_get_attr(this.__h, 'id') || ''; },
    set: function (v) { __host_set_attr(this.__h, 'id', String(v)); }
  },
  className: {
    get: function () { return __host_get_attr(this.__h, 'class') || ''; },
    set: function (v) { __host_set_attr(this.__h, 'class', String(v)); }
  },
  href: {
    get: function () { return __host_get_attr(this.__h, 'href') || ''; },
    set: function (v) { __host_set_attr(this.__h, 'href', String(v)); }
  },
  tagName: {
    get: function () { return String(__host_tag(this.__h)).toUpperCase(); }
  },
  nodeName: {
    get: function () { return String(__host_tag(this.__h)).toUpperCase(); }
  }
});
Element.prototype.getAttribute = function (name) {
  var v = __host_get_attr(this.__h, String(name));
  return v === null ? null : v;
};
Element.prototype.setAttribute = function (name, value) {
  __host_set_attr(this.__h, String(name), String(value));
};
Element.prototype.appendChild = function (child) {
  __host_append(this.__h, child.__h);
  return child;
};
Element.prototype.removeChild = function (child) {
  __host_remove(child.__h);
  return child;
};
Element.prototype.remove = function () { __host_remove(this.__h); };
Element.prototype.querySelector = function (sel) { return __wrap(__host_query(String(sel), this.__h)); };
Element.prototype.addEventListener = function () { /* events are not dispatched in this browser */ };
Element.prototype.removeEventListener = function () {};

var document = {
  getElementById: function (id) { return __wrap(__host_by_id(String(id))); },
  querySelector: function (sel) { return __wrap(__host_query(String(sel), 0)); },
  createElement: function (tag) { return __wrap(__host_create(String(tag))); },
  createTextNode: function (text) { return __wrap(__host_create_text(String(text))); },
  addEventListener: function () {},
  removeEventListener: function () {}
};
Object.defineProperties(document, {
  body: { get: function () { return __wrap(__host_query('body', 0)); } },
  documentElement: { get: function () { return __wrap(__host_query('html', 0)); } },
  title: {
    get: function () { var t = __host_query('title', 0); return t < 0 ? '' : __host_get_text(t); },
    set: function (v) { var t = __host_query('title', 0); if (t >= 0) __host_set_text(t, String(v)); }
  }
});

var window = this;
var globalThis = this;
window.document = document;
window.console = console;
window.setTimeout = setTimeout;
window.clearTimeout = clearTimeout;
"#;

fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args.get(i).cloned().unwrap_or_default().to_string(ctx)?.to_std_string_escaped())
}

fn arg_node(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Option<NodeId>> {
    let n = args.get(i).cloned().unwrap_or_default().to_number(ctx)?;
    Ok(if n.is_finite() && n >= 0.0 { Some(n as usize) } else { None })
}

fn handle_value(id: Option<NodeId>) -> JsValue {
    match id {
        Some(n) => JsValue::from(n as f64),
        None => JsValue::from(-1.0),
    }
}

impl JsRuntime {
    /// Create a JS context whose `document` is backed by `dom`.
    pub fn new(dom: Rc<RefCell<Dom>>) -> Result<Self, String> {
        let mut ctx = Context::default();
        register_host(&mut ctx, dom).map_err(|e| format!("host setup failed: {e}"))?;
        ctx.eval(Source::from_bytes(PRELUDE)).map_err(|e| format!("prelude failed: {e}"))?;
        Ok(JsRuntime { ctx })
    }

    /// Run a classic script. Errors are reported to stderr and do not abort the page.
    pub fn run_script(&mut self, name: &str, code: &str) {
        if let Err(e) = self.ctx.eval(Source::from_bytes(code)) {
            eprintln!("[js] {name}: uncaught exception: {e}");
        }
        self.drain_jobs();
    }

    /// Run pending `setTimeout` callbacks and promise jobs.
    pub fn run_pending(&mut self) {
        if let Err(e) = self.ctx.eval(Source::from_bytes("__runTimers()")) {
            eprintln!("[js] timers: {e}");
        }
        self.drain_jobs();
    }

    fn drain_jobs(&mut self) {
        if let Err(e) = self.ctx.run_jobs() {
            eprintln!("[js] promise job: {e}");
        }
    }
}

fn register_host(ctx: &mut Context, dom: Rc<RefCell<Dom>>) -> JsResult<()> {
    macro_rules! host {
        ($name:expr, $len:expr, $closure:expr) => {{
            // SAFETY: the captured `Rc<RefCell<Dom>>` holds no JavaScript objects,
            // so the garbage collector never needs to trace it.
            let f = unsafe { NativeFunction::from_closure($closure) };
            ctx.register_global_builtin_callable(JsString::from($name), $len, f)?;
        }};
    }

    host!("__host_log", 1, |_this, args, ctx| {
        println!("[js] {}", arg_str(args, 0, ctx)?);
        Ok(JsValue::undefined())
    });

    let d = dom.clone();
    host!("__host_by_id", 1, move |_this, args, ctx| {
        let id = arg_str(args, 0, ctx)?;
        Ok(handle_value(d.borrow().get_element_by_id(&id)))
    });

    let d = dom.clone();
    host!("__host_query", 2, move |_this, args, ctx| {
        let sel = arg_str(args, 0, ctx)?;
        let scope = arg_node(args, 1, ctx)?.unwrap_or(0);
        let dom = d.borrow();
        if !dom.is_valid(scope) {
            return Ok(handle_value(None));
        }
        Ok(handle_value(dom.query_selector(scope, &sel)))
    });

    let d = dom.clone();
    host!("__host_create", 1, move |_this, args, ctx| {
        let tag = arg_str(args, 0, ctx)?;
        Ok(handle_value(Some(d.borrow_mut().create_element(&tag))))
    });

    let d = dom.clone();
    host!("__host_create_text", 1, move |_this, args, ctx| {
        let text = arg_str(args, 0, ctx)?;
        Ok(handle_value(Some(d.borrow_mut().create_text(&text))))
    });

    let d = dom.clone();
    host!("__host_tag", 1, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::null()) };
        Ok(match d.borrow().tag(n) {
            Some(t) => JsValue::from(JsString::from(t)),
            None => JsValue::null(),
        })
    });

    let d = dom.clone();
    host!("__host_get_text", 1, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::null()) };
        let dom = d.borrow();
        if !dom.is_valid(n) {
            return Ok(JsValue::null());
        }
        Ok(JsValue::from(JsString::from(dom.text_content(n).as_str())))
    });

    let d = dom.clone();
    host!("__host_set_text", 2, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::undefined()) };
        let text = arg_str(args, 1, ctx)?;
        let mut dom = d.borrow_mut();
        if dom.is_valid(n) {
            dom.set_text_content(n, &text);
        }
        Ok(JsValue::undefined())
    });

    let d = dom.clone();
    host!("__host_get_attr", 2, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::null()) };
        let name = arg_str(args, 1, ctx)?;
        Ok(match d.borrow().get_attr(n, &name) {
            Some(v) => JsValue::from(JsString::from(v.as_str())),
            None => JsValue::null(),
        })
    });

    let d = dom.clone();
    host!("__host_set_attr", 3, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::undefined()) };
        let name = arg_str(args, 1, ctx)?;
        let value = arg_str(args, 2, ctx)?;
        let mut dom = d.borrow_mut();
        if dom.is_valid(n) {
            dom.set_attr(n, &name, &value);
        }
        Ok(JsValue::undefined())
    });

    let d = dom.clone();
    host!("__host_append", 2, move |_this, args, ctx| {
        let (Some(p), Some(c)) = (arg_node(args, 0, ctx)?, arg_node(args, 1, ctx)?) else {
            return Ok(JsValue::undefined());
        };
        let mut dom = d.borrow_mut();
        if dom.is_valid(p) && dom.is_valid(c) {
            dom.append(p, c);
        }
        Ok(JsValue::undefined())
    });

    let d = dom.clone();
    host!("__host_remove", 1, move |_this, args, ctx| {
        let Some(n) = arg_node(args, 0, ctx)? else { return Ok(JsValue::undefined()) };
        let mut dom = d.borrow_mut();
        if dom.is_valid(n) {
            dom.detach(n);
        }
        Ok(JsValue::undefined())
    });

    Ok(())
}

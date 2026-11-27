//! The Epic checkout step of `claim-promo`: a WebView on Fab's payment
//! page, where Epic's checkout runs with hCaptcha.
//!
//! **Safety invariant.** This is the only module that may make the
//! page place an order (Epic's `/v2/purchase/confirm-order`, sent when
//! the page's "Add to library" button is pressed). It does so only
//! when [`allow_confirm_order`] approves what Epic's own
//! `/v2/purchase/initialize` reported for this purchase: a zero total
//! for exactly the expected offer. There is no flag, env var or other
//! path around it, and a test fails if the place-order trigger or the
//! endpoint name appears anywhere else in `src/`.
//!
//! The window stays hidden unless Epic asks for a login or hCaptcha
//! shows a challenge. Never logs tokens, cookies or response bodies.

use crate::error::FabCliError;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Reports the checkout's own requests (`/v2/purchase/*`) to Rust:
/// path, status, and only the fields the guard and the outcome need.
const HOOK: &str = r#"
(function () {
  if (window.__fabcliHook) return; window.__fabcliHook = true;
  var send = function (o) { try { window.ipc.postMessage(JSON.stringify(o)); } catch (e) {} };
  var watch = function (url) { return /\/v2\/purchase\//.test(url || ''); };
  var report = function (url, status, text) {
    var o = { kind: 'purchase', path: (url || '').split('?')[0].replace(/^https?:\/\/[^/]+/, ''), status: status };
    try {
      var j = JSON.parse(text);
      if (j.purchasePricing) o.pricing = j.purchasePricing;
      if (j.lineOffers) o.lineOfferIds = j.lineOffers.map(function (l) { return l.offerId; });
      var r = j.orderResponse || j;
      if (r && typeof r.orderId === 'string') o.orderId = r.orderId;
      if (r && typeof r.entitlementId === 'string') o.entitlementId = r.entitlementId;
    } catch (e) {}
    send(o);
  };
  var of = window.fetch;
  window.fetch = function (input, init) {
    var url = typeof input === 'string' ? input : (input && input.url);
    return of.apply(this, arguments).then(function (r) {
      if (watch(url)) { r.clone().text().then(function (t) { report(url, r.status, t); }); }
      return r;
    });
  };
  var oo = XMLHttpRequest.prototype.open, os = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (m, url) { this.__u = url; return oo.apply(this, arguments); };
  XMLHttpRequest.prototype.send = function () {
    var x = this;
    if (watch(x.__u)) { x.addEventListener('load', function () { report(x.__u, x.status, x.responseText); }); }
    return os.apply(this, arguments);
  };
})();
"#;

/// Signs the page in to Fab: the in-page twin of
/// `fab_http::FabWebSession::mint_once` (CSRF cookie, Bearer token,
/// empty `refresh_token`), run by the page so the browser stores the
/// HttpOnly `fab_sessionid` itself. `__BEARER__` is replaced with the
/// JSON-quoted `Authorization` value.
const FAB_SIGNIN: &str = r#"
(function () {
  var m = document.cookie.match(/(?:^|;\s*)fab_csrftoken=([^;]+)/);
  var fd = new FormData(); fd.append('refresh_token', '');
  fetch('/p/authenticate', { method: 'POST', credentials: 'include',
    headers: { 'Authorization': __BEARER__, 'X-CsrfToken': m ? m[1] : '' }, body: fd })
    .then(function (r) { window.ipc.postMessage(JSON.stringify({ kind: 'fab_auth', status: r.status })); })
    .catch(function () { window.ipc.postMessage(JSON.stringify({ kind: 'fab_auth', status: 0 })); });
})();
"#;

/// Presses the payment page's "Add to library" button (retrying while
/// the page renders it). Only [`PaymentHost::checkout`] runs this, and
/// only after [`allow_confirm_order`].
const PLACE_ORDER: &str = r#"
(function () {
  var tries = 0;
  var t = setInterval(function () {
    var b = Array.prototype.slice.call(document.querySelectorAll('button')).find(function (x) {
      return (x.innerText || '').trim() === 'Add to library' && !x.disabled;
    });
    if (b) { clearInterval(t); b.click(); window.ipc.postMessage(JSON.stringify({ kind: 'clicked' })); }
    else if (++tries > 40) { clearInterval(t); window.ipc.postMessage(JSON.stringify({ kind: 'no_button' })); }
  }, 500);
})();
"#;

/// Reports whether an hCaptcha challenge is visible on the page.
const CAPTCHA_CHECK: &str = r#"
(function () {
  var visible = Array.prototype.slice.call(document.querySelectorAll('iframe[src*="hcaptcha.com"]')).some(function (f) {
    var r = f.getBoundingClientRect(); return r.width > 100 && r.height > 100;
  });
  window.ipc.postMessage(JSON.stringify({ kind: 'captcha', visible: visible }));
})();
"#;

/// What Epic's `/v2/purchase/initialize` said about this purchase.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Initialize {
    pub is_total_price_zero: Option<bool>,
    pub discount_total_price: Option<f64>,
    pub line_offer_ids: Vec<String>,
}

impl Initialize {
    fn from_event(e: &Value) -> Self {
        let pricing = e.get("pricing");
        Initialize {
            is_total_price_zero: pricing
                .and_then(|p| p.get("isTotalPriceZero"))
                .and_then(Value::as_bool),
            discount_total_price: pricing
                .and_then(|p| p.get("discountTotalPrice"))
                .and_then(Value::as_f64),
            line_offer_ids: e
                .get("lineOfferIds")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// THE zero-total guard: the only gate to placing an order. True only
/// when Epic's `initialize` (if it was observed at all) reports a zero
/// total for exactly the offer this claim selected.
pub fn allow_confirm_order(init: Option<&Initialize>, expected_offer: &str) -> bool {
    let Some(init) = init else { return false };
    init.is_total_price_zero == Some(true)
        && init.discount_total_price == Some(0.0)
        && init.line_offer_ids.len() == 1
        && init.line_offer_ids[0] == expected_offer
}

/// How one checkout ended.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutOutcome {
    /// Epic answered the order; ids when it reported them.
    Completed {
        order_id: Option<String>,
        entitlement_id: Option<String>,
    },
    /// The guard refused (or no `initialize` was seen): nothing ordered.
    NonZeroTotal { total: Option<f64> },
    /// No outcome in time (after showing a captcha, if one appeared).
    Timeout,
    /// The page failed in a way that isn't a refusal.
    Failed(String),
}

const SEED_TIMEOUT: Duration = Duration::from_secs(60);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const CAPTCHA_AFTER: Duration = Duration::from_secs(30);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(180);

type Events = Arc<Mutex<Vec<Value>>>;

/// One hidden WebView, signed in to Fab and Epic once, then reused for
/// every purchase of the invocation.
pub struct PaymentHost {
    // Field order is drop order: the WebView before its window and
    // data folder.
    webview: wry::WebView,
    window: crate::webview_host::HostWindow,
    _context: Box<wry::WebContext>,
    _data: tempfile::TempDir,
    _host: crate::webview_host::HostGuard,
    events: Events,
}

impl PaymentHost {
    /// Create the hidden window and sign it in: a Fab session minted
    /// in-page from `bearer`, then an Epic web session from a one-time
    /// `exchange_code` (design D3).
    pub fn open(bearer: &str, exchange_code: &str) -> Result<Self, FabCliError> {
        let host = crate::webview_host::init()?;
        let window = crate::webview_host::create_window(crate::webview_host::WindowOptions {
            title: "FabCLI — Fab checkout",
            visible: false,
            size: (1000, 800),
        })?;
        // A throwaway profile: nothing persists after the invocation.
        let data = tempfile::TempDir::new()?;
        let mut context = Box::new(wry::WebContext::new(Some(data.path().to_path_buf())));
        let events: Events = Arc::default();
        let (ipc_events, load_events) = (events.clone(), events.clone());
        let builder = wry::WebViewBuilder::with_web_context(&mut context)
            .with_url("https://www.fab.com/i/csrf")
            .with_initialization_script_for_main_only(HOOK, false)
            .with_ipc_handler(move |msg| {
                if let Ok(v) = serde_json::from_str::<Value>(msg.body()) {
                    ipc_events.lock().unwrap().push(v);
                }
            })
            .with_on_page_load_handler(move |ev, url| {
                if matches!(ev, wry::PageLoadEvent::Finished) {
                    let url = url.split('?').next().unwrap_or("").to_string();
                    load_events
                        .lock()
                        .unwrap()
                        .push(serde_json::json!({ "kind": "load", "url": url }));
                }
            });
        let webview = crate::webview_host::build_webview(builder, &window)
            .map_err(|e| FabCliError::Generic(format!("checkout window: {e}")))?;
        let mut host = PaymentHost {
            webview,
            window,
            _context: context,
            _data: data,
            _host: host,
            events,
        };
        host.seed(bearer, exchange_code)?;
        Ok(host)
    }

    fn seed(&mut self, bearer: &str, exchange_code: &str) -> Result<(), FabCliError> {
        self.wait_for(SEED_TIMEOUT, |e| is_load(e, "https://www.fab.com/i/csrf"))?
            .ok_or_else(|| FabCliError::Network("checkout window: fab.com did not load".into()))?;
        let auth = FAB_SIGNIN.replace("__BEARER__", &serde_json::to_string(bearer)?);
        self.eval(&auth)?;
        let status = self
            .wait_for(SEED_TIMEOUT, |e| kind(e) == "fab_auth")?
            .and_then(|e| e.get("status").and_then(Value::as_u64));
        if status != Some(200) {
            return Err(FabCliError::AuthRequired(format!(
                "Fab refused the checkout window's session (status {status:?}); re-run `fabcli auth login`"
            )));
        }

        // Epic ignores `redirectUrl` for fab.com, so wait for the
        // exchange to land anywhere on epicgames.com, then go on.
        let url = reqwest::Url::parse_with_params(
            "https://www.epicgames.com/id/exchange",
            &[("exchangeCode", exchange_code)],
        )
        .map_err(|e| FabCliError::Generic(format!("exchange url: {e}")))?;
        self.load(url.as_str())?;
        let landed = self.wait_for(SEED_TIMEOUT, |e| {
            kind(e) == "load"
                && url_of(e).contains("epicgames.com")
                && !url_of(e).contains("/id/exchange")
        })?;
        match landed {
            Some(e) if url_of(&e).contains("/id/login") => Err(FabCliError::AuthRequired(
                "Epic asked for a login in the checkout window; re-run `fabcli auth login`".into(),
            )),
            Some(_) => Ok(()),
            None => Err(FabCliError::Network(
                "checkout window: Epic sign-in did not complete".into(),
            )),
        }
    }

    /// Run one purchase: load the payment page for `purchase_token`,
    /// read Epic's `initialize`, and place the order only if
    /// [`allow_confirm_order`] approves it for `expected_offer`.
    pub fn checkout(
        &mut self,
        purchase_token: &str,
        expected_offer: &str,
    ) -> Result<CheckoutOutcome, FabCliError> {
        let started = Instant::now();
        self.load(&format!(
            "https://www.fab.com/payment/web/purchase?purchaseToken={purchase_token}"
        ))?;
        let init = self
            .wait_for(INITIALIZE_TIMEOUT, |e| {
                is_purchase(e, "/v2/purchase/initialize")
            })?
            .filter(|e| e.get("status").and_then(Value::as_u64) == Some(200))
            .map(|e| Initialize::from_event(&e));
        if !allow_confirm_order(init.as_ref(), expected_offer) {
            return Ok(CheckoutOutcome::NonZeroTotal {
                total: init.and_then(|i| i.discount_total_price),
            });
        }

        self.eval(PLACE_ORDER)?;
        let clicked_at = Instant::now();
        let mut captcha_checked = false;
        loop {
            if started.elapsed() > CHECKOUT_TIMEOUT {
                return Ok(CheckoutOutcome::Timeout);
            }
            for e in self.drain() {
                match after_click(&e) {
                    AfterClick::Done(outcome) => return Ok(outcome),
                    AfterClick::ShowWindow => {
                        self.window.set_visible(true);
                        eprintln!("fabcli: please solve the captcha in the open window");
                    }
                    AfterClick::Wait => {}
                }
            }
            if !captcha_checked && clicked_at.elapsed() > CAPTCHA_AFTER {
                captcha_checked = true;
                self.eval(CAPTCHA_CHECK)?;
            }
            self.pump()?;
        }
    }

    fn load(&mut self, url: &str) -> Result<(), FabCliError> {
        self.webview
            .load_url(url)
            .map_err(|e| FabCliError::Generic(format!("checkout window: {e}")))
    }

    fn eval(&self, js: &str) -> Result<(), FabCliError> {
        self.webview
            .evaluate_script(js)
            .map_err(|e| FabCliError::Generic(format!("checkout window: {e}")))
    }

    fn pump(&self) -> Result<(), FabCliError> {
        if self.window.was_closed() {
            return Err(FabCliError::Generic(
                "the checkout window was closed".into(),
            ));
        }
        crate::webview_host::pump_once();
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }

    fn drain(&mut self) -> Vec<Value> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    /// Pump until an event matches `pred` (returned) or `timeout`.
    fn wait_for(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<Option<Value>, FabCliError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(e) = self.drain().into_iter().find(|e| pred(e)) {
                return Ok(Some(e));
            }
            self.pump()?;
        }
        Ok(None)
    }
}

/// What one page event means once the order button was pressed.
#[derive(Debug, PartialEq)]
enum AfterClick {
    Done(CheckoutOutcome),
    /// hCaptcha shows a challenge: the user has to see the window.
    ShowWindow,
    Wait,
}

fn after_click(e: &Value) -> AfterClick {
    match kind(e) {
        "no_button" => AfterClick::Done(CheckoutOutcome::Failed(
            "the payment page showed no \"Add to library\" button".into(),
        )),
        "captcha" if e.get("visible").and_then(Value::as_bool) == Some(true) => {
            AfterClick::ShowWindow
        }
        "purchase" if path_of(e) == "/v2/purchase/confirm-order" => {
            let status = e.get("status").and_then(Value::as_u64).unwrap_or(0);
            if !(200..300).contains(&status) {
                return AfterClick::Done(CheckoutOutcome::Failed(format!(
                    "Epic's checkout answered HTTP {status}"
                )));
            }
            let id = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
            AfterClick::Done(CheckoutOutcome::Completed {
                order_id: id("orderId"),
                entitlement_id: id("entitlementId"),
            })
        }
        _ => AfterClick::Wait,
    }
}

fn kind(e: &Value) -> &str {
    e.get("kind").and_then(Value::as_str).unwrap_or("")
}

fn url_of(e: &Value) -> &str {
    e.get("url").and_then(Value::as_str).unwrap_or("")
}

fn path_of(e: &Value) -> &str {
    e.get("path").and_then(Value::as_str).unwrap_or("")
}

fn is_load(e: &Value, url: &str) -> bool {
    kind(e) == "load" && url_of(e).starts_with(url)
}

fn is_purchase(e: &Value, path: &str) -> bool {
    kind(e) == "purchase" && path_of(e) == path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn init(event: Value) -> Initialize {
        Initialize::from_event(&event)
    }

    /// The 2026-11-27 live `initialize` for a 100%-off promo.
    fn zero() -> Value {
        json!({"kind":"purchase","path":"/v2/purchase/initialize","status":200,
               "pricing":{"baseTotalPrice":6599,"discountTotalPrice":0,"isFreeToFree":false,"isDiscountToFree":true,"isTotalPriceZero":true},
               "lineOfferIds":["7e747aec"]})
    }

    #[test]
    fn guard_allows_only_a_zero_total_for_exactly_the_expected_offer() {
        assert!(allow_confirm_order(Some(&init(zero())), "7e747aec"));
        // Not observed at all.
        assert!(!allow_confirm_order(None, "7e747aec"));
        // Another offer, or an extra one, in the order.
        assert!(!allow_confirm_order(Some(&init(zero())), "other"));
        let mut two = zero();
        two["lineOfferIds"] = json!(["7e747aec", "extra"]);
        assert!(!allow_confirm_order(Some(&init(two)), "7e747aec"));
    }

    #[test]
    fn guard_refuses_any_non_zero_or_missing_pricing() {
        let mut paid = zero();
        paid["pricing"]["discountTotalPrice"] = json!(6599);
        assert!(!allow_confirm_order(Some(&init(paid)), "7e747aec"));
        let mut flag = zero();
        flag["pricing"]["isTotalPriceZero"] = json!(false);
        assert!(!allow_confirm_order(Some(&init(flag)), "7e747aec"));
        let mut none = zero();
        none.as_object_mut().unwrap().remove("pricing");
        assert!(!allow_confirm_order(Some(&init(none)), "7e747aec"));
    }

    /// Synthetic event streams after the order button was pressed.
    #[test]
    fn after_click_reads_the_checkout_outcome() {
        let first_decision = |events: Vec<Value>| {
            events
                .iter()
                .map(after_click)
                .find(|a| *a != AfterClick::Wait)
        };
        // Zero-total success (the 2026-11-27 live shape).
        assert_eq!(
            first_decision(vec![
                json!({"kind":"clicked"}),
                json!({"kind":"purchase","path":"/v2/purchase/confirm-order","status":200,"orderId":"F26"}),
            ]),
            Some(AfterClick::Done(CheckoutOutcome::Completed {
                order_id: Some("F26".into()),
                entitlement_id: None
            }))
        );
        // Epic refuses the order.
        assert!(matches!(
            first_decision(vec![
                json!({"kind":"purchase","path":"/v2/purchase/confirm-order","status":409})
            ]),
            Some(AfterClick::Done(CheckoutOutcome::Failed(_)))
        ));
        // A captcha challenge surfaces the window, then the order lands.
        let stream = [
            json!({"kind":"captcha","visible":true}),
            json!({"kind":"purchase","path":"/v2/purchase/confirm-order","status":200}),
        ];
        assert_eq!(after_click(&stream[0]), AfterClick::ShowWindow);
        assert!(matches!(
            after_click(&stream[1]),
            AfterClick::Done(CheckoutOutcome::Completed { .. })
        ));
        // No button on the page.
        assert!(matches!(
            first_decision(vec![json!({"kind":"no_button"})]),
            Some(AfterClick::Done(CheckoutOutcome::Failed(_)))
        ));
        // Unrelated traffic and an invisible captcha: keep waiting.
        assert_eq!(
            first_decision(vec![
                json!({"kind":"purchase","path":"/v2/purchase/get-earn-reward","status":200}),
                json!({"kind":"captcha","visible":false}),
            ]),
            None
        );
    }

    /// The safety invariant, checked over the source tree: the
    /// place-order trigger and Epic's order endpoint appear only in
    /// this module.
    #[test]
    fn place_order_lives_only_in_this_module() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") || path.ends_with("promo_checkout.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for needle in ["confirm-order", "PLACE_ORDER", "'Add to library'"] {
                    if text.contains(needle) {
                        offenders.push(format!("{} mentions {needle}", path.display()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:?}");
    }
}

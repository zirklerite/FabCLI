//! `fabcli claim-promo` / `claim-promo-batch`: claim 100%-off promo
//! ("Limited Time Free") assets, which Fab's `add-to-library` refuses,
//! through Fab's cart and Epic's checkout at a total of exactly 0.
//!
//! Fab-side steps (cart, payment, finalize, ownership) are plain HTTP
//! on the token-minted Fab session (`FabGate`). Only Epic's checkout
//! runs in a WebView (`promo_checkout`), which alone decides whether an
//! order may be placed.

use super::claim_batch::{error_entry, read_uid_input};
use super::fab::{is_acquired, listing_and_prices, listing_url};
use crate::error::FabCliError;
use crate::fab_http::FabGate;
use crate::fab_pricing::{classify, FreeKind};
use crate::output::print_json;
use crate::promo_checkout::{CheckoutOutcome, PaymentHost};
use crate::session::Session;
use clap::Args;
use egs_api::api::types::fab_search::FabPriceInfo;
use serde_json::{json, Value};

#[derive(Args, Debug)]
pub struct ClaimPromoBatchArgs {
    /// Comma-separated listing UIDs.
    #[arg(long, conflicts_with_all = ["stdin", "from_stdin_json"])]
    pub uids: Option<String>,
    /// Newline-delimited UIDs on stdin.
    #[arg(long, conflicts_with_all = ["uids", "from_stdin_json"])]
    pub stdin: bool,
    /// JSON on stdin: `fabcli search` output or `[{"uid":"…"}, …]`.
    #[arg(long, conflicts_with_all = ["uids", "stdin"])]
    pub from_stdin_json: bool,
}

/// One invocation's state: the Fab session, and the checkout window,
/// opened on the first purchase that needs it.
struct PromoRun {
    session: Session,
    gate: FabGate,
    host: Option<PaymentHost>,
    /// The cart was found empty before this run's first cart write.
    cart_checked: bool,
}

impl PromoRun {
    async fn start() -> Result<Self, FabCliError> {
        let mut session = Session::load().await?;
        let mut gate = FabGate::new(session.fab_auth_mode());
        gate.ensure_ready(&mut session).await?;
        Ok(PromoRun {
            session,
            gate,
            host: None,
            cart_checked: false,
        })
    }

    async fn call(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Value, FabCliError> {
        self.gate
            .call_json(&mut self.session, method, path, body)
            .await
    }

    async fn owned(&mut self, uid: &str) -> Result<bool, FabCliError> {
        is_acquired(&mut self.gate, &mut self.session, uid).await
    }

    /// Never add to (or clear) a cart the user filled on purpose: check
    /// it once per run, before the first cart write.
    async fn ensure_cart_empty(&mut self) -> Result<bool, FabCliError> {
        if !self.cart_checked {
            let cart = self.call("GET", "/i/cart", None).await?;
            let empty = cart
                .get("items")
                .and_then(Value::as_array)
                .is_none_or(|i| i.is_empty());
            if !empty {
                return Ok(false);
            }
            self.cart_checked = true;
        }
        Ok(true)
    }

    /// Empty the cart; a failure is only warned about, so the original
    /// outcome is what gets reported (design D8).
    async fn clear_cart(&mut self) {
        if let Err(e) = self.call("POST", "/i/cart/clear", Some("{}")).await {
            eprintln!("fabcli: warning: could not clear the Fab cart: {e}");
        }
    }

    /// The whole flow for one UID.
    async fn claim(&mut self, uid: &str) -> Result<Value, FabCliError> {
        let (mut listing, prices) = listing_and_prices(&self.session, uid).await?;
        let title = listing.title.take().unwrap_or_default();
        let offer = match classify(listing.is_free, &prices.offers, chrono::Utc::now()) {
            FreeKind::Promo(o) => o,
            other => {
                let mut out = json!({"ok": false, "reason": "not_promo", "uid": uid, "title": title, "purchase_url": listing_url(uid)});
                if other == FreeKind::Intrinsic {
                    out["hint"] =
                        "this asset is free without a promo; run 'fabcli claim <uid>'".into();
                }
                return Ok(out);
            }
        };
        if self.owned(uid).await? {
            return Ok(json!({"ok": true, "already_owned": true, "title": title, "uid": uid}));
        }
        let offer_id = offer.offer_id.as_deref().ok_or_else(|| {
            FabCliError::Generic(format!("{uid}: the promo offer has no offerId"))
        })?;
        let license = listing
            .licenses
            .iter()
            .flatten()
            .find(|l| l.offer_id.as_deref() == Some(offer_id))
            .and_then(|l| l.listing_license_id.as_deref())
            .ok_or_else(|| {
                FabCliError::Generic(format!("{uid}: no license matches the promo offer"))
            })?;
        if !self.ensure_cart_empty().await? {
            return Ok(json!({"ok": false, "reason": "cart_not_empty", "uid": uid}));
        }

        // From the first cart write on, the cart is cleared on every
        // exit, success included (step j).
        let result = self.checkout(uid, &title, license, offer).await;
        self.clear_cart().await;
        let out = result?;
        if out.get("claimed").and_then(Value::as_bool) == Some(true) {
            crate::library_cache::invalidate();
        }
        Ok(out)
    }

    async fn checkout(
        &mut self,
        uid: &str,
        title: &str,
        license: &str,
        offer: &FabPriceInfo,
    ) -> Result<Value, FabCliError> {
        let offer_id = offer.offer_id.as_deref().unwrap_or_default();
        self.call(
            "POST",
            "/i/cart/items",
            Some(&json!({ "listingLicense": license }).to_string()),
        )
        .await?;
        let payment = self.call("POST", "/i/payment", Some("{}")).await?;
        let token = payment
            .get("purchaseToken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FabCliError::Generic("Fab's payment step returned no purchaseToken".into())
            })?;

        if self.host.is_none() {
            let bearer = crate::fab_http::epic_bearer(&self.session)?;
            let exchange = self.session.epic.try_game_token().await?;
            self.host = Some(PaymentHost::open(&bearer, &exchange.code)?);
        }
        let host = self.host.as_mut().expect("opened above");
        match host.checkout(token, offer_id)? {
            CheckoutOutcome::Completed {
                order_id,
                entitlement_id,
            } => {
                if !self.owned(uid).await? {
                    // The page doesn't always tell Fab; finalize is the
                    // fallback (spec step i).
                    let body = json!({ "offer_ids": [offer_id] }).to_string();
                    let _ = self.call("POST", "/i/payment/finalize", Some(&body)).await;
                    if !self.owned(uid).await? {
                        return Err(FabCliError::Generic(format!(
                            "{uid}: the order went through but ownership could not be verified"
                        )));
                    }
                }
                let mut out = json!({"ok": true, "claimed": true, "title": title, "uid": uid});
                if let Some(id) = order_id {
                    out["orderId"] = id.into();
                }
                if let Some(id) = entitlement_id {
                    out["entitlementId"] = id.into();
                }
                Ok(out)
            }
            CheckoutOutcome::NonZeroTotal { total } => Ok(json!({
                "ok": false, "reason": "non_zero_total", "total": total,
                "currency": offer.currency_code, "uid": uid,
            })),
            CheckoutOutcome::Timeout => {
                Ok(json!({"ok": false, "reason": "claim_timeout", "uid": uid}))
            }
            CheckoutOutcome::Failed(why) => Err(FabCliError::Generic(format!("{uid}: {why}"))),
        }
    }
}

/// Exit 1 for the outcomes the spec calls failures, after printing.
fn outcome_error(out: &Value) -> Option<FabCliError> {
    let reason = out.get("reason").and_then(Value::as_str)?;
    matches!(
        reason,
        "non_zero_total" | "claim_timeout" | "cart_not_empty"
    )
    .then(|| FabCliError::Generic(format!("claim-promo: {reason}")))
}

pub async fn run(uid: Option<String>, use_stdin: bool, pretty: bool) -> Result<(), FabCliError> {
    let uid = super::read_id_or_stdin(uid, use_stdin)?;
    let mut run = PromoRun::start().await?;
    let out = run.claim(&uid).await?;
    print_json(&out, pretty);
    match outcome_error(&out) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub async fn run_batch(args: ClaimPromoBatchArgs, pretty: bool) -> Result<(), FabCliError> {
    let uids =
        read_uid_input(args.uids.as_deref(), args.stdin, args.from_stdin_json)?.unwrap_or_default();
    if uids.is_empty() {
        return Err(FabCliError::InvalidArgs(
            "no UIDs provided (use --uids, --stdin or --from-stdin-json)".into(),
        ));
    }
    let mut run = PromoRun::start().await?;
    // The spec checks the cart once, before the first UID.
    if !run.ensure_cart_empty().await? {
        print_json(&json!({"ok": false, "reason": "cart_not_empty"}), pretty);
        return Err(FabCliError::Generic("claim-promo: cart_not_empty".into()));
    }

    let mut results = Vec::with_capacity(uids.len());
    let mut meta = BatchMeta::default();
    for uid in &uids {
        let entry = run
            .claim(uid)
            .await
            .unwrap_or_else(|e| error_entry(uid, &e));
        meta.count(&entry);
        results.push(entry);
    }
    let failed = meta.non_zero_total + meta.failed > 0;
    print_json(
        &json!({"ok": !failed, "results": results, "meta": meta.to_json(uids.len())}),
        pretty,
    );
    if failed {
        return Err(FabCliError::Generic(
            "claim-promo-batch: some UIDs were refused or failed".into(),
        ));
    }
    Ok(())
}

/// `claim-promo-batch`'s `meta` counters.
#[derive(Default, Debug, PartialEq)]
struct BatchMeta {
    claimed: u32,
    already_owned: u32,
    skipped_not_promo: u32,
    non_zero_total: u32,
    failed: u32,
}

impl BatchMeta {
    fn count(&mut self, entry: &Value) {
        let flag = |k: &str| entry.get(k).and_then(Value::as_bool) == Some(true);
        if flag("claimed") {
            self.claimed += 1;
        } else if flag("already_owned") {
            self.already_owned += 1;
        } else {
            match entry.get("reason").and_then(Value::as_str) {
                Some("not_promo") => self.skipped_not_promo += 1,
                Some("non_zero_total") => self.non_zero_total += 1,
                _ => self.failed += 1,
            }
        }
    }

    fn to_json(&self, total: usize) -> Value {
        json!({
            "total": total,
            "claimed": self.claimed,
            "already_owned": self.already_owned,
            "skipped_not_promo": self.skipped_not_promo,
            "non_zero_total": self.non_zero_total,
            "failed": self.failed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_meta_counts_each_outcome() {
        let mut m = BatchMeta::default();
        for e in [
            json!({"ok": true, "claimed": true}),
            json!({"ok": true, "already_owned": true}),
            json!({"ok": false, "reason": "not_promo"}),
            json!({"ok": false, "reason": "non_zero_total"}),
            json!({"ok": false, "reason": "claim_timeout"}),
            json!({"ok": false, "error": {"kind": "network"}}),
        ] {
            m.count(&e);
        }
        assert_eq!(
            m,
            BatchMeta {
                claimed: 1,
                already_owned: 1,
                skipped_not_promo: 1,
                non_zero_total: 1,
                failed: 2
            }
        );
    }

    #[test]
    fn only_refusals_and_timeouts_exit_non_zero() {
        for r in ["non_zero_total", "claim_timeout", "cart_not_empty"] {
            assert!(outcome_error(&json!({"reason": r})).is_some(), "{r}");
        }
        for out in [
            json!({"ok": true, "claimed": true}),
            json!({"reason": "not_promo"}),
        ] {
            assert!(outcome_error(&out).is_none());
        }
    }
}

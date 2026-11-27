//! Which way, if any, a Fab listing can be had for free (design D1 of
//! `fabcli-claim-promo`). Read from `/i/listings/{uid}/prices-infos`
//! offers, because search-level `startingPrice` fields are unreliable
//! for promos in some regions.
//!
//! - **Intrinsic**: the listing is `isFree`, or some offer is priced 0.
//!   `add-to-library` accepts these (`claim`).
//! - **Promo**: not intrinsic, and some offer with `price > 0` is
//!   discounted to 0 (or 100%) inside its discount window. Fab answers
//!   `add-to-library` with "Not free." for these; they go through the
//!   cart and checkout (`claim-promo`).
//! - **Paid**: neither.
//!
//! `prices`' `limitedTimeFree` field keeps its own, narrower rule
//! (`price > 0 && discountedPrice == 0`, no window), which the
//! `fab-commands` spec promises exactly.

use chrono::{DateTime, Utc};
use egs_api::api::types::fab_search::FabPriceInfo;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FreeKind<'a> {
    Intrinsic,
    /// The promo offer (the first one found).
    Promo(&'a FabPriceInfo),
    Paid,
}

pub fn classify<'a>(
    is_free: Option<bool>,
    offers: &'a [FabPriceInfo],
    now: DateTime<Utc>,
) -> FreeKind<'a> {
    if is_free == Some(true) || offers.iter().any(|o| o.price == Some(0.0)) {
        return FreeKind::Intrinsic;
    }
    match offers.iter().find(|o| is_promo_offer(o, now)) {
        Some(o) => FreeKind::Promo(o),
        None => FreeKind::Paid,
    }
}

fn is_promo_offer(o: &FabPriceInfo, now: DateTime<Utc>) -> bool {
    let paid = o.price.is_some_and(|p| p > 0.0);
    let fully_discounted = o.discounted_price == Some(0.0) || o.discount_percentage == Some(100);
    paid && fully_discounted && in_window(o, now)
}

/// `now` within `[discountStartDate, discountEndDate]`. A missing or
/// unparseable bound doesn't restrict.
fn in_window(o: &FabPriceInfo, now: DateTime<Utc>) -> bool {
    let parse = |d: &Option<String>| {
        d.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    parse(&o.discount_start_date).is_none_or(|start| now >= start)
        && parse(&o.discount_end_date).is_none_or(|end| now <= end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn offer(id: &str, price: Option<f64>, discounted: Option<f64>) -> FabPriceInfo {
        FabPriceInfo {
            offer_id: Some(id.into()),
            price,
            discounted_price: discounted,
            ..Default::default()
        }
    }

    fn promo(id: &str, start: &str, end: &str) -> FabPriceInfo {
        FabPriceInfo {
            discount_percentage: Some(100),
            discount_start_date: Some(start.into()),
            discount_end_date: Some(end.into()),
            ..offer(id, Some(19.99), Some(0.0))
        }
    }

    #[test]
    fn intrinsic_by_flag_or_zero_priced_offer() {
        assert_eq!(classify(Some(true), &[], now()), FreeKind::Intrinsic);
        let single = [offer("a", Some(0.0), Some(0.0))];
        assert_eq!(classify(None, &single, now()), FreeKind::Intrinsic);
        // Tiered: paid Professional first, free Personal second.
        let tiered = [
            offer("pro", Some(348.28), Some(348.28)),
            offer("personal", Some(0.0), Some(0.0)),
        ];
        assert_eq!(classify(Some(false), &tiered, now()), FreeKind::Intrinsic);
    }

    #[test]
    fn promo_by_zero_discounted_price() {
        let offers = [promo("p", "2026-11-17T15:00:00Z", "2026-12-01T14:59:00Z")];
        assert!(
            matches!(classify(Some(false), &offers, now()), FreeKind::Promo(o) if o.offer_id.as_deref() == Some("p"))
        );
    }

    #[test]
    fn promo_by_percentage_when_discounted_price_is_null() {
        let o = FabPriceInfo {
            discounted_price: None,
            ..promo("p", "2026-11-17T15:00:00Z", "2026-12-01T14:59:00Z")
        };
        assert!(matches!(classify(None, &[o], now()), FreeKind::Promo(_)));
    }

    #[test]
    fn expired_or_future_promo_is_paid() {
        let ended = [promo("p", "2026-10-01T00:00:00Z", "2026-10-15T00:00:00Z")];
        assert_eq!(classify(None, &ended, now()), FreeKind::Paid);
        let later = [promo("p", "2026-12-01T00:00:00Z", "2026-12-15T00:00:00Z")];
        assert_eq!(classify(None, &later, now()), FreeKind::Paid);
    }

    #[test]
    fn paid_and_null_prices() {
        let paid = [offer("p", Some(19.99), Some(9.99))];
        assert_eq!(classify(Some(false), &paid, now()), FreeKind::Paid);
        let unpriced = [offer("u", None, None)];
        assert_eq!(classify(None, &unpriced, now()), FreeKind::Paid);
        assert_eq!(classify(None, &[], now()), FreeKind::Paid);
    }

    /// The live 2026-11-26 shape of `c0226253-…` (Post-Apocalyptic
    /// Factory): two promo tiers, both 100% off until 2026-12-01.
    #[test]
    fn fixture_from_a_live_promo_listing() {
        let body = r#"{"offers":[
            {"offerId":"o1","currencyCode":"TWD","price":1045.57,"discountedPrice":0.0,"discountPercentage":100,"discountStartDate":"2026-11-17T15:00:00.000Z","discountEndDate":"2026-12-01T14:59:00.000Z"},
            {"offerId":"o2","currencyCode":"TWD","price":2091.14,"discountedPrice":0.0,"discountPercentage":100,"discountStartDate":"2026-11-17T15:00:00.000Z","discountEndDate":"2026-12-01T14:59:00.000Z"}]}"#;
        let prices: egs_api::api::types::fab_search::FabBulkPricesResponse =
            serde_json::from_str(body).unwrap();
        match classify(Some(false), &prices.offers, now()) {
            FreeKind::Promo(o) => {
                assert_eq!(o.offer_id.as_deref(), Some("o1"));
                assert_eq!(
                    o.discount_end_date.as_deref(),
                    Some("2026-12-01T14:59:00.000Z")
                );
            }
            other => panic!("expected promo, got {other:?}"),
        }
    }
}

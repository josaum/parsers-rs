//! Amazon product-detail evidence. Scope offer fields to the new-item buy box
//! so duplicate IDs in resale offers cannot supply a different price or stock.
use crate::structured::{JsonSource, StructuredData, StructuredOptions};
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::sync::LazyLock;
static TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#centerCol span#productTitle").unwrap());
static BUYBOX: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#newAccordionRow_0").unwrap());
static SINGLE_BUYBOX: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#desktop_buybox #gsod_singleOfferDisplay_Desktop").unwrap());
static AMOUNT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("input[name='items[0.base][customerVisiblePrice][amount]']").unwrap()
});
static CURRENCY: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("input[name='items[0.base][customerVisiblePrice][currencyCode]']").unwrap()
});
static STOCK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#availability").unwrap());
static FEATURES: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#centerCol #feature-bullets li .a-list-item").unwrap());
static IMAGE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img#landingImage").unwrap());
fn text(e: ElementRef<'_>) -> String {
    e.descendants()
        .filter(|node| {
            !node
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|ancestor| {
                    matches!(
                        ancestor.value().name(),
                        "script" | "style" | "template" | "noscript"
                    ) || ancestor.value().attr("hidden").is_some()
                })
        })
        .filter_map(|node| node.value().as_text().map(|text| text.text.as_ref()))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}
fn decimal(s: &str) -> bool {
    let mut parts = s.split('.');
    let whole = parts.next().unwrap_or("");
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && parts
            .next()
            .is_none_or(|v| v.len() == 2 && v.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}
pub(crate) fn capture(
    data: &mut StructuredData,
    bytes: &[u8],
    source: &str,
    options: &StructuredOptions,
) {
    let Ok(url) = url::Url::parse(source) else {
        return;
    };
    if !matches!(url.host_str(), Some("amazon.com" | "www.amazon.com")) {
        return;
    }
    let Ok(html) = std::str::from_utf8(bytes) else {
        return;
    };
    if !html.contains("productTitle") {
        return;
    }
    let dom = Html::parse_document(html);
    let Some(title) = dom
        .select(&TITLE)
        .next()
        .map(text)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    if options.max_products == 0 {
        data.diagnostics.push("html_product_limit".into());
        return;
    }
    let new_buybox = dom.select(&BUYBOX).next();
    let buybox = new_buybox.or_else(|| dom.select(&SINGLE_BUYBOX).next());
    let (offer_selector, price_context) = if new_buybox.is_some() {
        ("#newAccordionRow_0", "new-item buy box")
    } else {
        (
            "#desktop_buybox #gsod_singleOfferDisplay_Desktop",
            "featured single-offer buy box; condition not inferred",
        )
    };
    let price = buybox
        .and_then(|b| b.select(&AMOUNT).next())
        .and_then(|e| e.value().attr("value"))
        .filter(|v| decimal(v));
    let currency = buybox
        .and_then(|b| b.select(&CURRENCY).next())
        .and_then(|e| e.value().attr("value"))
        .filter(|v| v.len() == 3 && v.bytes().all(|b| b.is_ascii_uppercase()));
    let availability = buybox
        .and_then(|b| b.select(&STOCK).next())
        .map(text)
        .filter(|s| !s.is_empty());
    let features = dom
        .select(&FEATURES)
        .map(text)
        .collect::<Vec<_>>()
        .join("\n");
    let words = features.split_whitespace().collect::<Vec<_>>();
    let description = (!words.is_empty()).then(|| {
        words
            .iter()
            .take(160)
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    });
    let image = dom
        .select(&IMAGE)
        .next()
        .and_then(|e| {
            e.value()
                .attr("data-old-hires")
                .filter(|s| !s.is_empty())
                .or_else(|| e.value().attr("src"))
        })
        .and_then(|s| url.join(s).ok())
        .and_then(|u| crate::normalize_url(u.as_str(), 8192).ok())
        .map(String::from);
    let asin = url
        .path_segments()
        .and_then(|mut segments| segments.find(|p| *p == "dp").and_then(|_| segments.next()))
        .filter(|s| s.len() == 10 && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    let value = json!({"id":asin,"name":title,"description":description,"url":source,"images":image.into_iter().collect::<Vec<_>>(),
        "offers":[{"price":price,"currency":currency,"availability":availability,"price_context":format!("{price_context}; taxes, shipping and account-specific promotions are not evaluated")}],
        "evidence":{"title_selector":"#centerCol span#productTitle","offer_selector":offer_selector,"price_input":"items[0.base][customerVisiblePrice][amount]","currency_input":"items[0.base][customerVisiblePrice][currencyCode]","description_selector":"#centerCol #feature-bullets li .a-list-item","description_kind":"feature_bullets_excerpt","description_truncated":words.len()>160}});
    data.add(
        JsonSource::new(
            "html_product_detail",
            source,
            "amazon:product-detail",
            value,
        ),
        options,
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resale_price_cannot_replace_new_item_price_and_description_is_retained() {
        let html=br#"<div id='usedAccordionRow'><input name='items[0.base][customerVisiblePrice][amount]' value='8.00'><div id='availability'>Only 1 left</div></div>
        <div id='newAccordionRow_0'><input name='items[0.base][customerVisiblePrice][amount]' value='15.28'><input name='items[0.base][customerVisiblePrice][currencyCode]' value='USD'><div id='availability'>In Stock<script type='application/json'>{"internal":"not stock"}</script><style>.hidden {display:none}</style><template>unavailable</template></div></div>
        <div id='centerCol'><span id='productTitle'>Wireless charging station</span><div id='feature-bullets'><ul><li><span class='a-list-item'>Charges three devices.</span></li></ul></div></div>"#;
        let mut data = StructuredData::default();
        let options = StructuredOptions::default();
        capture(
            &mut data,
            html,
            "https://www.amazon.com/dp/B012345678",
            &options,
        );
        data.normalize(&options);
        let p = &data.products[0];
        assert_eq!(p.id.as_deref(), Some("B012345678"));
        assert_eq!(p.description.as_deref(), Some("Charges three devices."));
        assert_eq!(p.offers[0].price, Some(json!("15.28")));
        assert_eq!(p.offers[0].currency.as_deref(), Some("USD"));
        assert_eq!(p.offers[0].availability.as_deref(), Some("In Stock"));
        assert_eq!(p.basis, "html_detail");
        assert_eq!(p.offers[0].price_unit, "major");
        let missing = std::str::from_utf8(html)
            .unwrap()
            .replace("newAccordionRow_0", "unrelated-offer");
        let mut data = StructuredData::default();
        capture(
            &mut data,
            missing.as_bytes(),
            "https://www.amazon.com/dp/B012345678",
            &options,
        );
        data.normalize(&options);
        assert!(data.products[0].offers[0].price.is_none());
        assert!(data.products[0].offers[0].availability.is_none());
        let single = std::str::from_utf8(html).unwrap().replace(
            "id='newAccordionRow_0'",
            "id='gsod_singleOfferDisplay_Desktop'",
        );
        let single = format!("<div id='desktop_buybox'>{single}</div>");
        let mut data = StructuredData::default();
        capture(
            &mut data,
            single.as_bytes(),
            "https://www.amazon.com/dp/B012345678",
            &options,
        );
        data.normalize(&options);
        assert_eq!(data.products[0].offers[0].price, Some(json!("15.28")));
        assert_eq!(
            data.products[0].offers[0].availability.as_deref(),
            Some("In Stock")
        );
        assert!(data.products[0].offers[0]
            .price_context
            .as_deref()
            .unwrap()
            .contains("condition not inferred"));
    }
}

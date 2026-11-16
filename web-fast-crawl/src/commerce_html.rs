//! Explicit Mercado Livre Brazil product-card adapter. Derived JSON is labeled
//! as HTML evidence, never represented as an API/JSON-LD response.
use crate::structured::{JsonSource, StructuredData, StructuredOptions};
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::sync::LazyLock;

static CARDS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".poly-card").unwrap());
static TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.poly-component__title").unwrap());
static PRICE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".poly-price__current .andes-money-amount[aria-label]").unwrap()
});
static FRACTION: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".andes-money-amount__fraction").unwrap());
static CENTS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".andes-money-amount__cents").unwrap());
static PRICE_CONTEXT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".poly-component__price").unwrap());
static IMAGES: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("img.poly-component__picture").unwrap());

fn text(element: ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_owned()
}
fn amount(price: ElementRef<'_>) -> Option<String> {
    let whole = text(price.select(&FRACTION).next()?);
    let groups = whole.split('.').collect::<Vec<_>>();
    if groups
        .iter()
        .any(|g| g.is_empty() || !g.bytes().all(|b| b.is_ascii_digit()))
        || (groups.len() > 1 && (groups[0].len() > 3 || groups[1..].iter().any(|g| g.len() != 3)))
    {
        return None;
    }
    let cents = price
        .select(&CENTS)
        .next()
        .map(text)
        .unwrap_or_else(|| "00".into());
    if cents.len() != 2 || !cents.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}.{cents}", groups.concat()))
}

pub(crate) fn capture(
    data: &mut StructuredData,
    bytes: &[u8],
    source: &str,
    options: &StructuredOptions,
) {
    let Ok(base) = url::Url::parse(source) else {
        return;
    };
    let Some(host) = base.host_str() else {
        return;
    };
    if matches!(host, "amazon.com" | "www.amazon.com") {
        crate::amazon_html::capture(data, bytes, source, options);
        return;
    }
    if host != "mercadolivre.com.br" && !host.ends_with(".mercadolivre.com.br") {
        return;
    }
    let Ok(html) = std::str::from_utf8(bytes) else {
        return;
    };
    if !html.contains("poly-card") {
        return;
    }
    let document = Html::parse_document(html);
    let mut products = Vec::new();
    let mut bytes_used = 0usize;
    for (ordinal, card) in document.select(&CARDS).enumerate() {
        if products.len() >= options.max_products {
            data.diagnostics.push("html_product_limit".into());
            break;
        }
        // Ignore inert/hidden card templates; price descendants may be aria-hidden
        // because the accessible amount is supplied by their parent's aria-label.
        if card
            .ancestors()
            .filter_map(ElementRef::wrap)
            .chain(std::iter::once(card))
            .any(|e| {
                matches!(e.value().name(), "template" | "noscript")
                    || e.value().attr("hidden").is_some()
                    || e.value()
                        .attr("aria-hidden")
                        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
            })
        {
            continue;
        }
        let Some(title) = card.select(&TITLE).next() else {
            continue;
        };
        let Some(url) = title
            .value()
            .attr("href")
            .and_then(|v| base.join(v).ok())
            .and_then(|u| crate::normalize_url(u.as_str(), 8192).ok())
        else {
            continue;
        };
        let name = text(title);
        if name.is_empty() {
            continue;
        }
        let Some(price) = card.select(&PRICE).next() else {
            continue;
        };
        let label = price
            .value()
            .attr("aria-label")
            .unwrap_or("")
            .to_lowercase();
        // The Brazilian-real currency name is explicit in the accessible label.
        if !label
            .split_whitespace()
            .any(|w| matches!(w, "real" | "reais"))
        {
            continue;
        }
        let Some(price_value) = amount(price) else {
            continue;
        };
        let images: Vec<_> = card
            .select(&IMAGES)
            .take(16)
            .filter_map(|e| e.value().attr("src").or_else(|| e.value().attr("data-src")))
            .filter_map(|s| base.join(s).ok())
            .filter_map(|u| crate::normalize_url(u.as_str(), 8192).ok())
            .map(String::from)
            .collect();
        let price_context = card.select(&PRICE_CONTEXT).next().map(|e| {
            e.text()
                .flat_map(str::split_whitespace)
                .collect::<Vec<_>>()
                .join(" ")
        });
        let value = json!({"name":name,"url":url.as_str(),"price":price_value,"currency":"BRL","price_context":price_context,"images":images,
            "evidence":{"card_selector":".poly-card","card_ordinal":ordinal,"price_selector":".poly-price__current .andes-money-amount[aria-label]","price_label":label}});
        let size = serde_json::to_vec(&value).expect("JSON value").len();
        if size > options.max_json_bytes.saturating_sub(bytes_used) {
            data.diagnostics.push("html_product_byte_limit".into());
            break;
        }
        bytes_used += size;
        products.push(value);
    }
    if !products.is_empty() {
        data.add(
            JsonSource::new(
                "html_product_cards",
                source,
                "mercadolivre:poly-card",
                json!({"products":products}),
            ),
            options,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_price_excludes_installments_and_original_price() {
        let html=br#"<div class='poly-card'><a class='poly-component__title' href='/phone/p/MLB1'>Phone</a>
        <s><span class='andes-money-amount__fraction'>9.999</span></s>
        <div class='poly-price__current'><span class='andes-money-amount' aria-label='1234 reais com 56 centavos'><span class='andes-money-amount__fraction' aria-hidden='true'>1.234</span><span class='andes-money-amount__cents' aria-hidden='true'>56</span></span></div>
        <span class='andes-money-amount' aria-label='123 reais'><span class='andes-money-amount__fraction'>123</span></span></div>"#;
        let mut data = StructuredData::default();
        let options = StructuredOptions::default();
        capture(
            &mut data,
            html,
            "https://www.mercadolivre.com.br/c/phones",
            &options,
        );
        data.normalize(&options);
        assert_eq!(data.products.len(), 1);
        assert_eq!(data.products[0].offers[0].price, Some(json!("1234.56")));
        assert_eq!(data.products[0].offers[0].currency.as_deref(), Some("BRL"));
        assert_eq!(data.products[0].offers[0].price_unit, "major");
        assert_eq!(data.products[0].basis, "html_card");
        assert!(data.products[0].description.is_none());
        assert!(data.products[0].offers[0].availability.is_none());
        let mut empty = StructuredData::default();
        capture(&mut empty, html, "https://example.com", &options);
        assert!(empty.sources.is_empty());
        let wrapped = format!(
            "<template>{}</template>",
            std::str::from_utf8(html).unwrap()
        );
        capture(
            &mut empty,
            wrapped.as_bytes(),
            "https://www.mercadolivre.com.br/",
            &options,
        );
        assert!(empty.sources.is_empty());
    }
}

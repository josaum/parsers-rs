use serde_json::json;
use web_fast_crawl::structured::{JsonSource, StructuredData, StructuredOptions};
#[test]
fn product_offers_variants_and_decimal_strings_retain_source_pointers() {
    let value = json!({"@type":"Product", "name":"Camera", "sku":"C-1", "brand":{"name":"Acme"}, "offers":{"@type":"AggregateOffer", "lowPrice":"120.30", "highPrice":"250.00", "priceCurrency":"BRL"}, "hasVariant":[{"@type":"Product", "sku":"C-RED", "name":"Red", "price":"129.90"}]});
    let mut data = StructuredData::default();
    data.add(
        JsonSource::new("json_ld", "https://e.test/camera", "script:0", value),
        &StructuredOptions::default(),
    );
    data.normalize(&StructuredOptions::default());
    assert_eq!(data.products[0].basis, "declared_type");
    assert_eq!(data.products[0].brand.as_deref(), Some("Acme"));
    assert_eq!(data.products[0].offers[0].low_price, Some(json!("120.30")));
    assert_eq!(data.products[0].offers[0].currency.as_deref(), Some("BRL"));
    assert_eq!(data.products[0].variants[0].sku.as_deref(), Some("C-RED"));
    for product in &data.products {
        assert!(data.sources[product.source]
            .value
            .pointer(&product.json_pointer)
            .is_some());
    }
}
#[test]
fn hydration_graphql_connections_and_provider_price_units_are_not_guessed() {
    let value = json!({"data":{"products":{"edges":[{"node":{"id":"p1", "title":"Boot", "variants":[{"sku":"b1", "price":12900, "selectedOptions":[{"name":"Size","value":"40"}]}]}}],"pageInfo":{"hasNextPage":true,"endCursor":"opaque"}}},"links":{"next":"?page=2"}});
    let mut data = StructuredData::default();
    data.add(
        JsonSource::new("network_json", "https://e.test/api/products", "GET", value),
        &StructuredOptions::default(),
    );
    data.normalize(&StructuredOptions::default());
    assert_eq!(data.products[0].basis, "field_shape");
    assert_eq!(data.products[0].variants[0].offer.price, Some(json!(12900)));
    assert_eq!(data.products[0].variants[0].offer.price_unit, "unspecified");
    assert!(data
        .pagination
        .iter()
        .any(|p| p.end_cursor.as_deref() == Some("opaque")));
    assert!(data
        .pagination
        .iter()
        .any(|p| p.next_url.as_deref() == Some("https://e.test/api/products?page=2")));
}
#[test]
fn normalization_and_capture_budgets_report_truncation() {
    let mut data = StructuredData::default();
    let options = StructuredOptions {
        max_sources: 1,
        max_products: 1,
        ..StructuredOptions::default()
    };
    data.add(
        JsonSource::new(
            "http_json",
            "https://e.test/a",
            "GET",
            json!([{"name":"A","sku":"a"},{"name":"B","sku":"b"}]),
        ),
        &options,
    );
    data.add(
        JsonSource::new("http_json", "https://e.test/b", "GET", json!({})),
        &options,
    );
    data.normalize(&options);
    assert_eq!(data.products.len(), 1);
    assert!(data.diagnostics.contains(&"json_source_limit".into()));
    assert!(data.diagnostics.contains(&"product_limit".into()));
    let mut data = StructuredData::default();
    data.add(
        JsonSource::new(
            "http_json",
            "https://e.test/a",
            "GET",
            json!({"large":"payload"}),
        ),
        &StructuredOptions {
            max_json_bytes: 1,
            ..StructuredOptions::default()
        },
    );
    assert!(data.sources.is_empty());
    assert_eq!(data.diagnostics, ["json_byte_limit"]);
}

#[test]
fn commerce_graphql_connections_and_price_ranges_are_normalized() {
    let value = json!({"title":"Boot", "priceRange":{"minVariantPrice":{"amount":"12.30","currencyCode":"USD"},"maxVariantPrice":{"amount":"18.50","currencyCode":"USD"}}, "variants":{"edges":[{"node":{"id":"v1","sku":"B1","title":"Small","price":{"amount":"12.30","currencyCode":"USD"}}}]}, "images":{"nodes":[{"url":"https://e.test/boot.png"}]}});
    let mut data = StructuredData::default();
    data.add(
        JsonSource::new("network_json", "https://e.test/graphql", "POST", value),
        &StructuredOptions::default(),
    );
    data.normalize(&StructuredOptions::default());
    let product = &data.products[0];
    assert_eq!(product.variants[0].sku.as_deref(), Some("B1"));
    assert_eq!(product.variants[0].offer.price, Some(json!("12.30")));
    assert_eq!(product.offers[0].low_price, Some(json!("12.30")));
    assert_eq!(product.offers[0].high_price, Some(json!("18.50")));
    assert_eq!(product.offers[0].currency.as_deref(), Some("USD"));
    assert_eq!(product.offers[0].price_unit, "unspecified");
    assert_eq!(product.images, ["https://e.test/boot.png"]);
}

#[test]
fn product_descriptions_and_marketplace_api_fields_are_retained() {
    let value = json!({"id":"MLB123", "title":"Coffee maker", "description":{"plain_text":"Capacity 1.2 L.\nRemovable filter."}, "price":"219.90", "currency_id":"BRL", "seller_id":1234, "permalink":"https://example.test/product", "pictures":[{"secure_url":"https://example.test/image.jpg"}], "condition":"new", "available_quantity":0});
    let mut data = StructuredData::default();
    data.add(
        JsonSource::new(
            "http_json",
            "https://api.example.test/items/123",
            "GET",
            value,
        ),
        &StructuredOptions::default(),
    );
    data.normalize(&StructuredOptions::default());
    let p = serde_json::to_value(&data.products[0]).unwrap();
    assert_eq!(p["description"], "Capacity 1.2 L.\nRemovable filter.");
    assert_eq!(p["offers"][0]["currency"], "BRL");
    assert_eq!(p["offers"][0]["seller"], "1234");
    assert_eq!(p["url"], "https://example.test/product");
    assert_eq!(p["images"][0], "https://example.test/image.jpg");
    assert_eq!(p["condition"], "new");
    assert_eq!(p["offers"][0]["available_quantity"], 0);
}

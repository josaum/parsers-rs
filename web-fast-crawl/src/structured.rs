//! JSON-first extraction for application state, APIs, products and pagination.
use crate::{
    graph::{GraphBuilder, GraphError, Term},
    normalize_url,
    semantic::RDF_TYPE,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use web_fast_core::WebDocument;

/// Bounds for captured JSON and commerce normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StructuredOptions {
    /// Maximum distinct JSON sources per page.
    pub max_sources: usize,
    /// Maximum serialized JSON payload bytes per page.
    pub max_json_bytes: usize,
    /// Maximum visited JSON values during normalization.
    pub max_json_nodes: usize,
    /// Maximum nesting inspected by the commerce normalizer.
    pub max_depth: usize,
    /// Maximum normalized products per page.
    pub max_products: usize,
    /// Maximum serialized commerce/pagination records per page.
    pub max_commerce_bytes: usize,
}
impl Default for StructuredOptions {
    fn default() -> Self {
        Self {
            max_sources: 64,
            max_json_bytes: 8 * 1024 * 1024,
            max_json_nodes: 100_000,
            max_depth: 64,
            max_products: 1_000,
            max_commerce_bytes: 4 * 1024 * 1024,
        }
    }
}
/// A captured JSON value and the location where it was observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSource {
    /// `json_ld`, `next_data`, `nuxt_data`, `application_json`, `initial_state`,
    /// `http_json`, `network_json`, `browser_state`, or derived `html_product_cards`.
    pub kind: String,
    /// Page/API URL.
    pub url: String,
    /// DOM path, script index, state variable, or request method.
    pub locator: String,
    /// SHA-256 of the serialized parsed JSON value, not the original wire bytes.
    pub value_sha256: String,
    /// Source JSON structure, or explicitly labeled JSON derived from HTML cards.
    pub value: Value,
    #[serde(skip)]
    serialized_bytes: usize,
}
impl JsonSource {
    /// Attach provenance to an already parsed JSON value.
    pub fn new(kind: &str, url: &str, locator: &str, value: Value) -> Self {
        let bytes = serde_json::to_vec(&value).expect("JSON values are serializable");
        Self {
            kind: kind.into(),
            url: url.into(),
            locator: locator.into(),
            value_sha256: format!("{:x}", Sha256::digest(&bytes)),
            serialized_bytes: bytes.len(),
            value,
        }
    }
}
/// A price/availability observation. Amounts remain JSON strings/numbers.
#[derive(Debug, Clone, Serialize)]
pub struct CommerceOffer {
    /// Provider offer ID.
    pub id: Option<String>,
    /// Price as represented by the source.
    pub price: Option<Value>,
    /// Aggregate lower bound.
    pub low_price: Option<Value>,
    /// Aggregate upper bound.
    pub high_price: Option<Value>,
    /// Explicit currency; never inferred from locale or symbols.
    pub currency: Option<String>,
    /// `major` for declared Schema.org offers or explicit HTML-card adapters; otherwise `unspecified`.
    pub price_unit: String,
    /// Source payment/price conditions; separate from the current numeric amount.
    pub price_context: Option<String>,
    /// Availability URI/string or explicit boolean rendered as text.
    pub availability: Option<String>,
    /// Explicit stock quantity, when supplied; zero is retained.
    pub available_quantity: Option<Value>,
    /// Merchant name or ID.
    pub seller: Option<String>,
    /// Offer URL.
    pub url: Option<String>,
}
/// A variant observation retaining provider option values.
#[derive(Debug, Clone, Serialize)]
pub struct CommerceVariant {
    /// Provider ID.
    pub id: Option<String>,
    /// Variant title.
    pub name: Option<String>,
    /// Stock keeping unit.
    pub sku: Option<String>,
    /// Selected options, if explicitly represented.
    pub options: Option<Value>,
    /// Price/availability observation.
    pub offer: CommerceOffer,
}
/// Product candidate with a JSON pointer back to the captured source.
#[derive(Debug, Clone, Serialize)]
pub struct CommerceProduct {
    /// `declared_type`, `field_shape`, `html_card`, or `html_detail`; not ontology assertions.
    pub basis: String,
    /// Index into `StructuredData.sources`.
    pub source: usize,
    /// RFC 6901 pointer in that source.
    pub json_pointer: String,
    /// Provider/product resource ID.
    pub id: Option<String>,
    /// Product name.
    pub name: Option<String>,
    /// Description as provided, including any markup; never inferred from a title.
    pub description: Option<String>,
    /// Explicit product condition URI or provider value.
    pub condition: Option<String>,
    /// Stock keeping unit.
    pub sku: Option<String>,
    /// Explicit GTIN.
    pub gtin: Option<String>,
    /// Brand name.
    pub brand: Option<String>,
    /// Canonical/product URL when explicitly present.
    pub url: Option<String>,
    /// Image URLs as supplied.
    pub images: Vec<String>,
    /// Offers and aggregate price ranges.
    pub offers: Vec<CommerceOffer>,
    /// Up to 100 directly represented variants; original JSON retains the rest.
    pub variants: Vec<CommerceVariant>,
}
/// A next-page URL or cursor observed in a JSON response.
#[derive(Debug, Clone, Serialize)]
pub struct PaginationHint {
    /// Source index.
    pub source: usize,
    /// JSON pointer to the containing object.
    pub json_pointer: String,
    /// Resolved next-page URL, if provided.
    pub next_url: Option<String>,
    /// Explicit GraphQL/connection cursor; no request is invented from it.
    pub end_cursor: Option<String>,
    /// Provider flag.
    pub has_next_page: Option<bool>,
}
/// Captured source data and normalized commerce observations.
#[derive(Debug, Default, Serialize)]
pub struct StructuredData {
    /// JSON sources, in observation order.
    pub sources: Vec<JsonSource>,
    /// Product candidates, with source pointers.
    pub products: Vec<CommerceProduct>,
    /// Pagination hints.
    pub pagination: Vec<PaginationHint>,
    /// Explicit truncation or malformed-source diagnostics.
    pub diagnostics: Vec<String>,
    #[serde(skip)]
    json_bytes: usize,
}
impl StructuredData {
    /// Capture JSON-LD and application JSON from an HTML document.
    pub fn from_document(document: &WebDocument, url: &str, options: &StructuredOptions) -> Self {
        let mut data = Self::default();
        for (i, value) in document.json_ld.iter().enumerate() {
            data.add(
                JsonSource::new("json_ld", url, &format!("script:{i}"), value.clone()),
                options,
            );
        }
        for item in &document.json_data {
            data.add(
                JsonSource::new(&item.kind, url, &item.dom_path, item.value.clone()),
                options,
            );
        }
        data
    }
    /// Add a source if it fits the source and aggregate byte budgets.
    pub fn add(&mut self, mut source: JsonSource, options: &StructuredOptions) {
        if self.sources.iter().any(|old| {
            old.kind == source.kind
                && old.url == source.url
                && old.locator == source.locator
                && old.value_sha256 == source.value_sha256
        }) {
            return;
        }
        if self.sources.len() >= options.max_sources {
            self.diagnostic("json_source_limit");
            return;
        }
        if source.serialized_bytes == 0 {
            source.serialized_bytes =
                serde_json::to_vec(&source.value).map_or(usize::MAX, |bytes| bytes.len());
        }
        if source.serialized_bytes > options.max_json_bytes.saturating_sub(self.json_bytes) {
            self.diagnostic("json_byte_limit");
            return;
        }
        self.json_bytes += source.serialized_bytes;
        self.sources.push(source);
    }
    fn diagnostic(&mut self, code: &str) {
        if !self.diagnostics.iter().any(|old| old == code) {
            self.diagnostics.push(code.into());
        }
    }
    /// Normalize products and pagination with bounded JSON traversal.
    pub fn normalize(&mut self, options: &StructuredOptions) {
        self.products.clear();
        self.pagination.clear();
        let mut walker = Walker {
            options,
            nodes: 0,
            bytes: 0,
            products: Vec::new(),
            pagination: Vec::new(),
            diagnostics: Vec::new(),
        };
        for (source, data) in self.sources.iter().enumerate() {
            walker.visit(&data.value, source, &data.url, &mut String::new(), 0);
        }
        self.products = walker.products;
        for product in &mut self.products {
            if matches!(
                self.sources[product.source].kind.as_str(),
                "html_product_cards" | "html_product_detail"
            ) {
                product.basis = if self.sources[product.source].kind == "html_product_cards" {
                    "html_card"
                } else {
                    "html_detail"
                }
                .into();
                for offer in &mut product.offers {
                    offer.price_unit = "major".into();
                }
            }
        }
        self.pagination = walker.pagination;
        for diagnostic in walker.diagnostics {
            self.diagnostic(diagnostic);
        }
    }
    /// Project product candidates into the graph without asserting heuristic RDF types.
    pub fn insert_products(
        &self,
        graph: &mut GraphBuilder,
        source_url: &str,
    ) -> Result<(), GraphError> {
        for product in &self.products {
            let source = &self.sources[product.source];
            let key = format!(
                "{:x}",
                Sha256::digest(format!(
                    "{}\0{}\0{}",
                    source.url, source.value_sha256, product.json_pointer
                ))
            );
            let node = Term::iri(format!("urn:web-fast:product:{key}"));
            let origin = Term::iri(source_url);
            graph.insert(
                origin.clone(),
                Term::iri("https://schema.org/mentions"),
                node.clone(),
                None,
                origin.clone(),
            )?;
            graph.insert(
                node.clone(),
                Term::iri(RDF_TYPE),
                Term::iri("urn:web-fast:ProductObservation"),
                None,
                origin.clone(),
            )?;
            for (predicate, value) in [
                ("urn:web-fast:basis", Some(product.basis.as_str())),
                (
                    "urn:web-fast:jsonPointer",
                    Some(product.json_pointer.as_str()),
                ),
                (
                    "urn:web-fast:jsonValueSha256",
                    Some(source.value_sha256.as_str()),
                ),
                ("https://schema.org/name", product.name.as_deref()),
                (
                    "https://schema.org/description",
                    product.description.as_deref(),
                ),
                (
                    "https://schema.org/itemCondition",
                    product.condition.as_deref(),
                ),
                ("https://schema.org/sku", product.sku.as_deref()),
                ("https://schema.org/gtin", product.gtin.as_deref()),
            ] {
                if let Some(value) = value {
                    graph.insert(
                        node.clone(),
                        Term::iri(predicate),
                        Term::text(value),
                        None,
                        origin.clone(),
                    )?;
                }
            }
            graph.insert(
                node.clone(),
                Term::iri("urn:web-fast:jsonSource"),
                Term::iri(&source.url),
                None,
                origin.clone(),
            )?;
            if let Some(url) = &product.url {
                graph.insert(
                    node,
                    Term::iri("https://schema.org/url"),
                    Term::iri(url),
                    None,
                    origin,
                )?;
            }
        }
        Ok(())
    }
}
struct Walker<'a> {
    options: &'a StructuredOptions,
    nodes: usize,
    bytes: usize,
    products: Vec<CommerceProduct>,
    pagination: Vec<PaginationHint>,
    diagnostics: Vec<&'static str>,
}
impl Walker<'_> {
    fn diagnostic(&mut self, code: &'static str) {
        if !self.diagnostics.contains(&code) {
            self.diagnostics.push(code);
        }
    }
    fn visit(&mut self, value: &Value, source: usize, url: &str, path: &mut String, depth: usize) {
        if self.nodes >= self.options.max_json_nodes {
            self.diagnostic("json_node_limit");
            return;
        }
        self.nodes += 1;
        if depth > self.options.max_depth {
            self.diagnostic("json_depth_limit");
            return;
        }
        if value.is_object() {
            if let Some(product) = product(value, source, path, url) {
                let size = serde_json::to_vec(&product).map_or(usize::MAX, |bytes| bytes.len());
                if self.products.len() >= self.options.max_products {
                    self.diagnostic("product_limit");
                } else if size > self.options.max_commerce_bytes.saturating_sub(self.bytes) {
                    self.diagnostic("commerce_byte_limit");
                } else {
                    self.bytes += size;
                    self.products.push(product);
                }
            }
            if let Some(hint) = pagination(value, source, path, url) {
                let size = serde_json::to_vec(&hint).map_or(usize::MAX, |bytes| bytes.len());
                if size <= self.options.max_commerce_bytes.saturating_sub(self.bytes) {
                    self.bytes += size;
                    self.pagination.push(hint);
                } else {
                    self.diagnostic("commerce_byte_limit");
                }
            }
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if self.nodes >= self.options.max_json_nodes {
                        self.diagnostic("json_node_limit");
                        break;
                    }
                    let len = path.len();
                    path.push('/');
                    path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    self.visit(value, source, url, path, depth + 1);
                    path.truncate(len);
                }
            }
            Value::Array(values) => {
                for (i, value) in values.iter().enumerate() {
                    if self.nodes >= self.options.max_json_nodes {
                        self.diagnostic("json_node_limit");
                        break;
                    }
                    let len = path.len();
                    path.push('/');
                    path.push_str(&i.to_string());
                    self.visit(value, source, url, path, depth + 1);
                    path.truncate(len);
                }
            }
            _ => {}
        }
    }
}
fn string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
fn field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(string))
}
fn named(value: Option<&Value>) -> Option<String> {
    let value = value?;
    string(value).or_else(|| field(value, &["name", "title", "@id", "id"]))
}
fn values(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(values)) => values.iter().take(100).collect(),
        Some(value) => vec![value],
        None => Vec::new(),
    }
}
fn members(value: Option<&Value>) -> Vec<&Value> {
    if let Some(Value::Object(object)) = value {
        if let Some(Value::Array(nodes)) = object.get("nodes") {
            return nodes.iter().take(100).collect();
        }
        if let Some(Value::Array(edges)) = object.get("edges") {
            return edges
                .iter()
                .take(100)
                .filter_map(|edge| edge.get("node"))
                .collect();
        }
    }
    values(value)
}
fn amount(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if value.is_string() || value.is_number() {
        Some(value.clone())
    } else {
        value
            .get("amount")
            .or_else(|| value.get("value"))
            .filter(|v| v.is_string() || v.is_number())
            .cloned()
    }
}
fn resolved(value: Option<String>, base: &str) -> Option<String> {
    let value = value?;
    let url = url::Url::parse(base).ok()?.join(&value).ok()?;
    normalize_url(url.as_str(), 8192).ok().map(Into::into)
}
fn offer(value: &Value, base: &str, declared: bool) -> CommerceOffer {
    let price = value
        .get("price")
        .or_else(|| value.get("priceV2"))
        .or_else(|| value.get("priceSpecification").and_then(|v| v.get("price")));
    let range = value
        .get("priceRange")
        .or_else(|| value.get("priceRangeV2"));
    let minimum = range.and_then(|v| v.get("minVariantPrice"));
    let maximum = range.and_then(|v| v.get("maxVariantPrice"));
    let range_currency = minimum
        .and_then(|v| field(v, &["currencyCode", "currency"]))
        .filter(|currency| {
            maximum
                .and_then(|v| field(v, &["currencyCode", "currency"]))
                .is_none_or(|other| other == *currency)
        });
    CommerceOffer {
        id: field(value, &["@id", "id"]),
        price: amount(price),
        low_price: amount(value.get("lowPrice").or(minimum)),
        high_price: amount(value.get("highPrice").or(maximum)),
        currency: field(
            value,
            &["priceCurrency", "currency", "currencyCode", "currency_id"],
        )
        .or_else(|| price.and_then(|v| field(v, &["currencyCode", "currency"])))
        .or_else(|| {
            value
                .get("priceSpecification")
                .and_then(|v| field(v, &["priceCurrency"]))
        })
        .or(range_currency),
        price_unit: if declared { "major" } else { "unspecified" }.into(),
        price_context: field(value, &["price_context"]),
        availability: field(value, &["availability", "availableForSale", "available"]),
        available_quantity: value
            .get("available_quantity")
            .filter(|v| v.is_u64())
            .cloned(),
        seller: named(value.get("seller")).or_else(|| field(value, &["seller_id"])),
        url: resolved(field(value, &["url", "permalink"]), base),
    }
}
fn has_type(value: &Value, name: &str) -> bool {
    values(value.get("@type"))
        .iter()
        .filter_map(|v| v.as_str())
        .any(|kind| {
            kind == name
                || kind.strip_suffix(name).is_some_and(|prefix| {
                    prefix.ends_with('/') || prefix.ends_with(':') || prefix.ends_with('#')
                })
        })
}
fn product(value: &Value, source: usize, pointer: &str, base: &str) -> Option<CommerceProduct> {
    let declared = has_type(value, "Product")
        || has_type(value, "ProductGroup")
        || has_type(value, "IndividualProduct");
    let name = field(value, &["name", "title"]);
    if !declared
        && (has_type(value, "Offer")
            || has_type(value, "AggregateOffer")
            || name.is_none()
            || ![
                "sku",
                "variants",
                "hasVariant",
                "offers",
                "price",
                "priceRange",
            ]
            .iter()
            .any(|key| value.get(*key).is_some()))
    {
        return None;
    }
    let offers = if value.get("offers").is_some() {
        members(value.get("offers"))
            .into_iter()
            .map(|v| offer(v, base, declared))
            .collect()
    } else {
        vec![offer(value, base, declared)]
    };
    let variants = members(value.get("variants").or_else(|| value.get("hasVariant")))
        .into_iter()
        .filter(|v| v.is_object())
        .map(|v| CommerceVariant {
            id: field(v, &["@id", "id"]),
            name: field(v, &["name", "title"]),
            sku: field(v, &["sku"]),
            options: v
                .get("selectedOptions")
                .or_else(|| v.get("options"))
                .cloned(),
            offer: offer(
                members(v.get("offers")).first().copied().unwrap_or(v),
                base,
                declared,
            ),
        })
        .collect();
    let images = members(
        value
            .get("image")
            .or_else(|| value.get("images"))
            .or_else(|| value.get("pictures")),
    )
    .into_iter()
    .take(16)
    .filter_map(|v| string(v).or_else(|| field(v, &["secure_url", "url", "src", "contentUrl"])))
    .collect();
    Some(CommerceProduct {
        basis: if declared {
            "declared_type"
        } else {
            "field_shape"
        }
        .into(),
        source,
        json_pointer: pointer.into(),
        id: field(value, &["@id", "productID", "id"]),
        name,
        description: value
            .get("description")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| field(v, &["plain_text", "text"]))
            })
            .or_else(|| field(value, &["descriptionHtml", "body_html", "plain_text"])),
        condition: field(value, &["itemCondition", "condition"]),
        sku: field(value, &["sku"]),
        gtin: field(value, &["gtin", "gtin14", "gtin13", "gtin12", "gtin8"]),
        brand: named(value.get("brand")),
        url: resolved(field(value, &["url", "canonical_url", "permalink"]), base),
        images,
        offers,
        variants,
    })
}
fn pagination(value: &Value, source: usize, pointer: &str, base: &str) -> Option<PaginationHint> {
    let next = field(value, &["next", "next_page_url", "nextPageUrl"])
        .or_else(|| value.get("next").and_then(|v| field(v, &["href", "url"])));
    let has_next_page = value.get("hasNextPage").and_then(Value::as_bool);
    let end_cursor = field(value, &["endCursor", "next_cursor"]);
    let next_url = resolved(next, base);
    (next_url.is_some() || has_next_page.is_some() || end_cursor.is_some()).then(|| {
        PaginationHint {
            source,
            json_pointer: pointer.into(),
            next_url,
            end_cursor,
            has_next_page,
        }
    })
}

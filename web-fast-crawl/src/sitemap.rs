use crate::{normalize_url, Kind};
use quick_xml::{events::Event, Reader};
use url::Url;
pub(crate) fn parse(
    bytes: &[u8],
    max_urls: usize,
    max_url_bytes: usize,
) -> Result<(Kind, Vec<Url>), &'static str> {
    let mut reader =
        Reader::from_str(std::str::from_utf8(bytes).map_err(|_| "invalid_sitemap_encoding")?);
    let mut root = None;
    let mut depth: usize = 0;
    let mut urls = Vec::new();
    loop {
        match reader.read_event().map_err(|_| "invalid_sitemap_xml")? {
            Event::Start(tag) => {
                if depth == 0 && root.is_some() {
                    return Err("multiple_sitemap_roots");
                }
                depth += 1;
                if depth > 64 {
                    return Err("sitemap_depth_limit");
                }
                if root.is_none() {
                    root = match tag.local_name().as_ref() {
                        "urlset" => Some(Kind::Page),
                        "sitemapindex" => Some(Kind::Sitemap),
                        _ => return Err("invalid_sitemap_root"),
                    };
                }
                if tag.local_name().as_ref() == "loc" {
                    let value = reader
                        .read_text(tag.name())
                        .map_err(|_| "invalid_sitemap_location")?;
                    let value = quick_xml::escape::unescape(&value)
                        .map_err(|_| "invalid_sitemap_escape")?;
                    let url = normalize_url(value.trim(), max_url_bytes)
                        .map_err(|_| "invalid_sitemap_url")?;
                    if urls.len() >= max_urls {
                        return Err("sitemap_url_limit");
                    }
                    urls.push(url);
                    depth -= 1;
                }
            }
            Event::Empty(tag) if root.is_none() => {
                root = match tag.local_name().as_ref() {
                    "urlset" => Some(Kind::Page),
                    "sitemapindex" => Some(Kind::Sitemap),
                    _ => return Err("invalid_sitemap_root"),
                };
            }
            Event::Empty(_) if depth == 0 => return Err("multiple_sitemap_roots"),
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or("invalid_sitemap_xml")?;
            }
            Event::DocType(_) => return Err("sitemap_doctype_not_supported"),
            Event::Eof => {
                if depth != 0 {
                    return Err("truncated_sitemap_xml");
                }
                break;
            }
            _ => {}
        }
    }
    Ok((root.ok_or("missing_sitemap_root")?, urls))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_locations_and_xml_entities_survive_parser_upgrade() {
        let (_, urls) = parse(
            "<urlset><url><loc>https://e.test/café?a=1&amp;b=2</loc></url></urlset>".as_bytes(),
            10,
            1024,
        )
        .unwrap();
        assert_eq!(urls[0].as_str(), "https://e.test/caf%C3%A9?a=1&b=2");
        assert_eq!(
            parse(b"<urlset>\xff</urlset>", 10, 1024),
            Err("invalid_sitemap_encoding")
        );
    }

    #[test]
    fn malformed_sitemaps_are_rejected_without_external_entities() {
        for bytes in [
            b"<!DOCTYPE urlset SYSTEM 'http://localhost/private'><urlset/>".as_slice(),
            b"<urlset><url>",
            b"<urlset/><urlset/>",
            b"<wrong/>",
        ] {
            assert!(parse(bytes, 10, 1024).is_err());
        }
        assert_eq!(parse(b"<urlset/>", 10, 1024).unwrap().1.len(), 0);
        assert!(parse(
            b"<urlset><url><loc>https://e.test/</loc></url></urlset>",
            0,
            1024
        )
        .is_err());
    }
}

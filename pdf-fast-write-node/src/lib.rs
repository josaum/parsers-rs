use napi::bindgen_prelude::*;
use napi_derive::napi;
use pdf_fast_write::PdfWriter;

/// Convert JavaScript page numbers to `usize`, refusing what a cast would bend.
///
/// A fraction, a negative number, NaN and infinity are errors, not rounded
/// or wrapped into some other page.
fn page_numbers(pages: &[f64]) -> Result<Vec<usize>> {
    pages
        .iter()
        .map(|&page| {
            if page.is_finite() && page >= 1.0 && page.fract() == 0.0 && page <= f64::from(u32::MAX)
            {
                Ok(page as usize)
            } else {
                Err(Error::new(
                    Status::InvalidArg,
                    format!("page numbers are whole numbers starting at 1, got {page}"),
                ))
            }
        })
        .collect()
}

/// Extract specific 1-based pages from `source` into a new PDF at `output`.
#[napi(catch_unwind)]
pub fn extract_pdf_pages(source: String, pages: Vec<f64>, output: String) -> Result<()> {
    let pages = page_numbers(&pages)?;
    PdfWriter::extract_pages(&source, &pages, &output)
        .map_err(|e| Error::from_reason(e.to_string()))
}

/// Extract specific 1-based pages from a PDF path and return subset PDF bytes.
#[napi(catch_unwind)]
pub fn extract_pdf_pages_to_bytes(source: String, pages: Vec<f64>) -> Result<Buffer> {
    let pages = page_numbers(&pages)?;
    PdfWriter::extract_pages_to_bytes(&source, &pages)
        .map(Buffer::from)
        .map_err(|e| Error::from_reason(e.to_string()))
}

/// Extract specific 1-based pages from in-memory PDF bytes.
#[napi(catch_unwind)]
pub fn extract_pdf_pages_from_bytes(data: Buffer, pages: Vec<f64>) -> Result<Buffer> {
    let pages = page_numbers(&pages)?;
    PdfWriter::extract_pages_from_bytes(&data, &pages)
        .map(Buffer::from)
        .map_err(|e| Error::from_reason(e.to_string()))
}

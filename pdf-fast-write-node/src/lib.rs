use napi::bindgen_prelude::*;
use napi_derive::napi;
use pdf_fast_write::PdfWriter;

/// Extract specific 1-based pages from `source` into a new PDF at `output`.
#[napi(catch_unwind)]
pub fn extract_pdf_pages(source: String, pages: Vec<u32>, output: String) -> Result<()> {
    let pages: Vec<usize> = pages.into_iter().map(|p| p as usize).collect();
    PdfWriter::extract_pages(&source, &pages, &output)
        .map_err(|e| Error::from_reason(e.to_string()))
}

/// Extract specific 1-based pages from a PDF path and return subset PDF bytes.
#[napi(catch_unwind)]
pub fn extract_pdf_pages_to_bytes(source: String, pages: Vec<u32>) -> Result<Buffer> {
    let pages: Vec<usize> = pages.into_iter().map(|p| p as usize).collect();
    PdfWriter::extract_pages_to_bytes(&source, &pages)
        .map(Buffer::from)
        .map_err(|e| Error::from_reason(e.to_string()))
}

/// Extract specific 1-based pages from in-memory PDF bytes.
#[napi(catch_unwind)]
pub fn extract_pdf_pages_from_bytes(data: Buffer, pages: Vec<u32>) -> Result<Buffer> {
    let pages: Vec<usize> = pages.into_iter().map(|p| p as usize).collect();
    PdfWriter::extract_pages_from_bytes(&data, &pages)
        .map(Buffer::from)
        .map_err(|e| Error::from_reason(e.to_string()))
}

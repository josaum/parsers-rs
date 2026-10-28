use napi::bindgen_prelude::*;
use napi_derive::napi;
use xlsx_fast_write::Workbook;

/// Write an XLSX file from a JSON workbook spec to `out_path`.
#[napi]
pub fn write_xlsx_json(json: String, out_path: String) -> Result<()> {
    Workbook::write_json_spec(&json, &out_path).map_err(|e| Error::from_reason(e.to_string()))
}

/// Write an XLSX from a JSON workbook spec and return the bytes.
#[napi]
pub fn write_xlsx_json_bytes(json: String) -> Result<Buffer> {
    let bytes =
        Workbook::write_json_spec_to_bytes(&json).map_err(|e| Error::from_reason(e.to_string()))?;
    Ok(bytes.into())
}

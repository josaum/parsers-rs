const native = require('./xlsx_fast_write_node.node');

function _payload(spec) {
  return typeof spec === 'string' ? spec : JSON.stringify(spec);
}

module.exports = {
  /** Write an XLSX file from a workbook spec (object or JSON string). */
  writeXlsx: (spec, outPath) => native.writeXlsxJson(_payload(spec), outPath),
  /** Write an XLSX from a workbook spec and return a Buffer. */
  writeXlsxBytes: (spec) => native.writeXlsxJsonBytes(_payload(spec)),
};

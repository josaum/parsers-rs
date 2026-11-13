const native = require('./pdf_fast_write_node.node');

module.exports = {
  /** Extract 1-based pages from `source` PDF into `output` PDF. */
  extractPages: (source, pages, output) =>
    native.extractPdfPages(source, pages.map((p) => Number(p)), output),
  /** Extract 1-based pages from the PDF at `source`; return the subset as a Buffer. */
  extractPagesToBytes: (source, pages) =>
    native.extractPdfPagesToBytes(source, pages.map((p) => Number(p))),
  /** Extract 1-based pages from the PDF bytes in `data`; return the subset as a Buffer. */
  extractPagesFromBytes: (data, pages) =>
    native.extractPdfPagesFromBytes(data, pages.map((p) => Number(p))),
};

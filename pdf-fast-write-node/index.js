const native = require('./pdf_fast_write_node.node');

module.exports = {
  /** Extract 1-based pages from `source` PDF into `output` PDF. */
  extractPages: (source, pages, output) =>
    native.extractPdfPages(source, pages.map((p) => Number(p)), output),
};

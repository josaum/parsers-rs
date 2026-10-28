const native = require('./pdf_fast_node.node');

const { PdfScanner } = native;

module.exports = {
  PdfScanner,
  scanPdfJson: native.scanPdfJson,
  scanPdfSemanticRunsJson: native.scanPdfSemanticRunsJson,
  scanPdfParagraphsJson: native.scanPdfParagraphsJson,
  scanPdfSemanticRunsArrowIpc: native.scanPdfSemanticRunsArrowIpc,
  scanPdfPageMultimodalArrowIpc: native.scanPdfPageMultimodalArrowIpc,
  scanPdfPageMultimodalIpc: native.scanPdfPageMultimodalIpc,
  scanPdfPageBitmapComponentsJson: native.scanPdfPageBitmapComponentsJson,
  scanPdfPageTableLayoutArrowIpc: native.scanPdfPageTableLayoutArrowIpc,
  scanPdfPageTableLayoutFromOcrArrowIpc: native.scanPdfPageTableLayoutFromOcrArrowIpc,
  scanPdfPageTableArrowIpc: native.scanPdfPageTableArrowIpc,
  scanPdfPageTableFromOcrArrowIpc: native.scanPdfPageTableFromOcrArrowIpc,
  scanPdfPageCount: native.scanPdfPageCount,
  scanPdfPageDimensionsJson: native.scanPdfPageDimensionsJson,
  scanPdfPageRasterizedPng: native.scanPdfPageRasterizedPng,
  scanPdfFullText: native.scanPdfFullText,
};

const native = require('./pptx_fast_node.node');

function parsePptx(path, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.parsePptxJs(path, opts);
}

function parsePptxNdjson(path, outPath, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.parsePptxNdjsonJs(path, outPath, opts);
}

function parsePptxArrowIpc(path, outDir, options = undefined) {
  let opts = undefined;
  let arrowOpts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
    arrowOpts = {
      batchSize: options.batchSize ?? options.batch_size,
    };
  }
  return native.parsePptxArrowIpcJs(path, outDir, opts, arrowOpts);
}

function parsePptxArrowDataset(path, outDir, options = undefined) {
  let opts = undefined;
  let arrowOpts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
    arrowOpts = {
      batchSize: options.batchSize ?? options.batch_size,
    };
  }
  return native.parsePptxArrowDatasetJs(path, outDir, opts, arrowOpts);
}

function extractPptxImages(path, outDir, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.extractPptxImagesJs(path, outDir, opts);
}

module.exports = {
  parsePptx,
  parsePptxNdjson,
  parsePptxArrowIpc,
  parsePptxArrowDataset,
  extractPptxImages,
};

const native = require('./docx_fast_node.node');

function parseDocx(path, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.parseDocxJs(path, opts);
}

function parseDocxNdjson(path, outPath, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.parseDocxNdjsonJs(path, outPath, opts);
}

function parseDocxArrowIpc(path, outDir, options = undefined) {
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
  return native.parseDocxArrowIpcJs(path, outDir, opts, arrowOpts);
}

function parseDocxArrowDataset(path, outDir, options = undefined) {
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
  return native.parseDocxArrowDatasetJs(path, outDir, opts, arrowOpts);
}

function extractDocxImages(path, outDir) {
  return native.extractDocxImagesJs(path, outDir);
}

module.exports = {
  parseDocx,
  parseDocxNdjson,
  parseDocxArrowIpc,
  parseDocxArrowDataset,
  extractDocxImages,
};

const native = require('./xlsx_fast_node.node');

function parseXlsx(path, options = undefined) {
  let opts = undefined;
  if (options) {
    opts = {
      decodeDates: options.decodeDates ?? options.decode_dates,
      includeFormulas: options.includeFormulas ?? options.include_formulas,
      rawValues: options.rawValues ?? options.raw_values,
      maxInflate: options.maxInflate ?? options.max_inflate,
    };
  }
  return native.parseXlsxJs(path, opts);
}

function parseOpts(options) {
  if (!options) {
    return undefined;
  }
  return {
    decodeDates: options.decodeDates ?? options.decode_dates,
    includeFormulas: options.includeFormulas ?? options.include_formulas,
    rawValues: options.rawValues ?? options.raw_values,
    maxInflate: options.maxInflate ?? options.max_inflate,
  };
}

module.exports = {
  parseXlsx,
  parseXlsxNdjson: (path, outPath, options = undefined) => {
    return native.parseXlsxNdjsonJs(path, outPath, parseOpts(options));
  },
  parseXlsxArrowIpc: (path, outPath, options = undefined) => {
    let arrowOpts = undefined;
    if (options) {
      arrowOpts = {
        batchSize: options.batchSize ?? options.batch_size,
      };
    }
    return native.parseXlsxArrowIpcJs(path, outPath, parseOpts(options), arrowOpts);
  },
};

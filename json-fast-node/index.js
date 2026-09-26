const native = require('./json_fast_node.node');

module.exports = {
  parseJson: native.parseJson,
  parseJsonl: native.parseJsonl,
  countLines: native.countLines,
  jsonType: native.jsonType,
  validate: native.validate,
  jsonRpcRequest: native.jsonRpcRequest,
  jsonRpcResponse: native.jsonRpcResponse,
  jsonApiDocument: native.jsonApiDocument,
  jwtParts: native.jwtParts,
  jsonLdMeta: native.jsonLdMeta,
  version: native.version,
};

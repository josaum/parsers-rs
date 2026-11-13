const native = require('./json_fast_node.node');

// `resources` must be a plain object from URI to schema JSON text. The native
// conversion would read a number or a Map as nothing and a string as its
// characters, so anything else is refused here, before it gets there.
function validate(s, schema, resources) {
  if (resources === undefined || resources === null) return native.validate(s, schema);
  const prototype = typeof resources === 'object' ? Object.getPrototypeOf(resources) : undefined;
  if (prototype !== Object.prototype && prototype !== null) {
    throw new TypeError('validate: resources must be a plain object mapping URI strings to schema JSON text');
  }
  for (const [uri, text] of Object.entries(resources)) {
    if (typeof text !== 'string') {
      throw new TypeError(`validate: resources[${JSON.stringify(uri)}] must be a string of schema JSON text`);
    }
  }
  return native.validate(s, schema, resources);
}

module.exports = {
  parseJson: native.parseJson,
  parseJsonl: native.parseJsonl,
  countLines: native.countLines,
  jsonType: native.jsonType,
  validate,
  jsonRpcRequest: native.jsonRpcRequest,
  jsonRpcResponse: native.jsonRpcResponse,
  jsonApiDocument: native.jsonApiDocument,
  jwtParts: native.jwtParts,
  jsonLdMeta: native.jsonLdMeta,
  version: native.version,
};

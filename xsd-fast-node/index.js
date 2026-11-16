const native = require('./xsd_fast_node.node');

module.exports = {
  listElements: native.listElements,
  describeElementJson: native.describeElementJson,
  validateJson: native.validateJson,
  validateBytesJson: native.validateBytesJson,
  writeSchemaIpc: native.writeSchemaIpc,
  schemaIpcBytes: native.schemaIpcBytes,
  allArrowSchemasJson: native.allArrowSchemasJson,
};

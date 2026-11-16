const native = require('./geo_fast_node.node');

module.exports = {
  version: native.version,
  parseGeojsonToRecordsJson: native.parseGeojsonToRecordsJson,
  parseShapefileToRecordsJson: native.parseShapefileToRecordsJson,
  bboxIouGeodesic: native.bboxIouGeodesicJs,
  bboxAreaKm2: native.bboxAreaKm2Js,
  splitBboxAntimeridianJson: native.splitBboxAntimeridianJson,
  strtreeQueryJson: native.strtreeQueryJson,
  geotiffMetadataJson: native.geotiffMetadataJson,
  geotiffBands: native.geotiffBands,
  computeNdvi: native.computeNdvi,
  spectralAngle: native.spectralAngle,
};

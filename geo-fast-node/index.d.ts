export interface GeoRecord {
  id: string | null;
  wkt: string;
  lon: number;
  lat: number;
  properties: unknown;
}

export declare function version(): string;
export declare function parseGeojsonToRecordsJson(geojson: string): string;
export declare function parseShapefileToRecordsJson(path: string): string;
export declare function bboxIouGeodesic(a: string, b: string): number;
export declare function bboxAreaKm2(bbox: string): number;
export declare function splitBboxAntimeridianJson(bbox: string): string;
export declare function strtreeQueryJson(boxesJson: string, queryBBox: string): string;

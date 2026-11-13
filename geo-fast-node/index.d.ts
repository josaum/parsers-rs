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

/** Georeferencing of a GeoTIFF, as `JSON.parse(geotiffMetadataJson(path))` returns it. */
export interface GeoTiffMetadata {
  width: number;
  height: number;
  band_count: number;
  /** `[min_lon, min_lat, max_lon, max_lat]` in degrees; `null` for a projected raster. */
  bbox: [number, number, number, number] | null;
  pixel_scale: [number, number] | null;
  tiepoints: number[][];
  transformation_matrix: number[] | null;
  epsg_code: number | null;
  is_geographic: boolean;
  /** True when the raster type is PixelIsPoint: a tiepoint ties a pixel centre. */
  pixel_is_point: boolean;
  upper_left: [number, number] | null;
  lower_right: [number, number] | null;
  /** GeoKey directory, by GeoKey ID. */
  geokeys: Record<string, number>;
  /** GeoKey values stored in the double and ASCII parameter tags, by GeoKey ID. */
  geokey_params: { doubles: Record<string, number[]>; strings: Record<string, string> };
}

/** Read a GeoTIFF's georeferencing as a JSON string (see `GeoTiffMetadata`). Throws on an unreadable file. */
export declare function geotiffMetadataJson(path: string): string;
/** Read every band, one `Float32Array` per band. Throws when the bands exceed `maxBandBytes`. */
export declare function geotiffBands(path: string, maxBandBytes?: number | null): Float32Array[];
/** NDVI per sample. Throws when the bands differ in length. */
export declare function computeNdvi(nirBand: number[], redBand: number[]): Float32Array;
/** Spectral angle in radians. Throws when the spectra differ in length. */
export declare function spectralAngle(pixelSpectrum: number[], reference: number[]): number;

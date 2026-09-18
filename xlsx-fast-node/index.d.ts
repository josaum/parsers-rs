/**
 * xlsx-fast-node - High-performance Excel (.xlsx) parser
 * 
 * @example
 * ```typescript
 * import { parseXlsx, parseXlsxNdjson } from 'xlsx-fast-node';
 * 
 * // Parse entire workbook
 * const workbook = parseXlsx('/path/to/file.xlsx');
 * console.log(workbook.sheets[0].name);
 * 
 * // Stream to NDJSON
 * parseXlsxNdjson('/path/to/large.xlsx', '/tmp/out.ndjson');
 * ```
 */

/**
 * Options for parsing Excel files
 */
export interface ParseOptions {
  /** Convert Excel dates to ISO8601 strings (default: true) */
  decodeDates?: boolean;
  /** Include formula text when present (default: false) */
  includeFormulas?: boolean;
  /** Emit raw values for non-string cells (default: false) */
  rawValues?: boolean;
  /** Max uncompressed part size in bytes (default: 134217728 = 128 MiB) */
  maxInflate?: number;
  /** Cells per Arrow record batch when using parseXlsxArrowIpc (default: 16384) */
  batchSize?: number;
}

/**
 * A cell value can be one of several types
 */
export type CellValue =
  | { type: 'empty' }
  | { type: 'bool'; v: boolean }
  | { type: 'number'; v: number }
  | { type: 'string'; v: string }
  | { type: 'raw'; v: string }
  | { type: 'error'; v: string }
  | { type: 'date'; iso: string; serial: number };

/**
 * A single cell in a worksheet
 */
export interface Cell {
  /** Row index (1-based) */
  row: number;
  /** Column index (1-based, A=1, B=2, ...) */
  col: number;
  /** Formula text if present and includeFormulas is enabled */
  formula?: string;
  /** The cell's value */
  value: CellValue;
}

/**
 * A single worksheet within an Excel workbook
 */
export interface Sheet {
  /** Sheet index (1-based) */
  id: number;
  /** Sheet name as displayed in Excel */
  name: string;
  /** All cells with values in this sheet */
  cells: Cell[];
}

/**
 * A complete Excel workbook containing multiple sheets
 */
export interface Workbook {
  /** All worksheets in the workbook, in order */
  sheets: Sheet[];
}

/**
 * Parse an Excel file into a Workbook object
 * 
 * @param path - Path to the .xlsx file
 * @param options - Parsing options
 * @returns The parsed workbook
 * @throws Error if the file cannot be parsed
 * 
 * @example
 * ```typescript
 * const workbook = parseXlsx('data.xlsx');
 * for (const sheet of workbook.sheets) {
 *   console.log(`Sheet: ${sheet.name}`);
 *   for (const cell of sheet.cells) {
 *     console.log(`  [${cell.row},${cell.col}] = ${JSON.stringify(cell.value)}`);
 *   }
 * }
 * ```
 */
export function parseXlsx(path: string, options?: ParseOptions): Workbook;

/**
 * Parse an Excel file and stream output to NDJSON
 * 
 * This is memory-efficient for large files as it doesn't load everything
 * into memory at once.
 * 
 * @param path - Path to the .xlsx file
 * @param outPath - Path for the output .ndjson file
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 * 
 * @example
 * ```typescript
 * parseXlsxNdjson('large.xlsx', 'output.ndjson');
 * 
 * // Output format (one JSON object per line):
 * // {"kind":"sheet","id":1,"name":"Sheet1"}
 * // {"kind":"cell","sheet_id":1,"row":1,"col":1,"type":"string","v":"Hello"}
 * // {"kind":"cell","sheet_id":1,"row":1,"col":2,"type":"number","v":42}
 * ```
 */
export function parseXlsxNdjson(path: string, outPath: string, options?: ParseOptions): void;

/**
 * Parse an Excel file and write Arrow IPC (Feather v2) to `outPath`.
 */
export function parseXlsxArrowIpc(
  path: string,
  outPath: string,
  options?: ParseOptions
): void;

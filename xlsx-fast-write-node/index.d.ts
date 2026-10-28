/** A single cell in a workbook spec. */
export interface CellSpec {
  /** Cell type: 's' string, 'n' number, 'b' bool. Defaults to 's'. */
  t?: 's' | 'n' | 'b';
  /** The value (string, number, or boolean). */
  v: string | number | boolean;
  /** Optional format name: 'bold' or 'header'. */
  fmt?: string;
}

/** A worksheet spec. */
export interface SheetSpec {
  /** Sheet name. */
  name: string;
  /** Column widths keyed by 0-based column index (string keys). */
  column_widths?: Record<string, number>;
  /** Rows of cells, row 0 first. */
  rows?: CellSpec[][];
}

/** A workbook spec. */
export interface WorkbookSpec {
  sheets: SheetSpec[];
}

/** Write an XLSX file from a workbook spec (object or JSON string). */
export function writeXlsx(spec: WorkbookSpec | string, outPath: string): void;

/** Write an XLSX from a workbook spec and return a Buffer. */
export function writeXlsxBytes(spec: WorkbookSpec | string): Buffer;

/**
 * docx-fast-node - High-performance Word (.docx) parser
 * 
 * @example
 * ```typescript
 * import { parseDocx, extractDocxImages } from 'docx-fast-node';
 * 
 * const doc = parseDocx('/path/to/document.docx');
 * console.log(doc.metadata.core.title);
 * 
 * // Extract images
 * const imagePaths = extractDocxImages('/path/to/doc.docx', './images');
 * ```
 */

/**
 * Options for parsing Word documents
 */
export interface ParseOptions {
  /** Max uncompressed part size in bytes (default: 134217728 = 128 MiB) */
  maxInflate?: number;
  /** Batch size for Arrow output (default: 16384) */
  batchSize?: number;
}

/**
 * Document metadata from core, app, and custom properties
 */
export interface Metadata {
  /** Dublin Core properties (title, creator, subject, etc.) */
  core: Record<string, string>;
  /** Application properties (Application, AppVersion, etc.) */
  app: Record<string, string>;
  /** Custom document properties defined by the user */
  custom: Record<string, string>;
}

/**
 * Table of contents entry
 */
export interface TocEntry {
  level: number;
  text: string;
  source: string;
  paragraph_id?: string;
  style_id?: string;
}

/**
 * Document style
 */
export interface Style {
  id: string;
  name?: string;
  type?: string;
  based_on?: string;
  next?: string;
  outline_level?: number;
  ui_priority?: number;
  is_default: boolean;
  is_qformat: boolean;
}

/**
 * Numbering reference for lists
 */
export interface NumberingRef {
  num_id: number;
  level: number;
}

/**
 * Text run with formatting
 */
export interface Run {
  text: string;
  style_id?: string;
  bold: boolean;
  italic: boolean;
  underline: boolean;
  field_id?: number;
  revision?: RevisionRef;
  comment_ids: number[];
}

/**
 * Revision reference
 */
export interface RevisionRef {
  id: string;
  kind: string;
}

/**
 * Paragraph structure
 */
export interface Paragraph {
  id?: string;
  style_id?: string;
  style_name?: string;
  outline_level?: number;
  numbering?: NumberingRef;
  toc_level?: number;
  runs: Run[];
  text: string;
}

/**
 * Table cell containing nested blocks
 */
export interface TableCell {
  blocks: Block[];
}

/**
 * Table row
 */
export interface TableRow {
  cells: TableCell[];
}

/**
 * Table structure
 */
export interface Table {
  rows: TableRow[];
}

/**
 * Block can be a paragraph or table
 */
export type Block =
  | { type: 'paragraph'; id?: string; style_id?: string; style_name?: string; outline_level?: number; numbering?: NumberingRef; toc_level?: number; runs: Run[]; text: string }
  | { type: 'table'; rows: TableRow[] };

/**
 * Header or footer
 */
export interface HeaderFooter {
  kind: string;
  ref_type?: string;
  part: string;
  blocks: Block[];
}

/**
 * Footnote or endnote
 */
export interface Note {
  id: number;
  blocks: Block[];
}

/**
 * Comment with author and date
 */
export interface Comment {
  id: number;
  author?: string;
  initials?: string;
  date?: string;
  parent_id?: number;
  blocks: Block[];
}

/**
 * Revision (track changes)
 */
export interface Revision {
  id: string;
  kind: string;
  author?: string;
  date?: string;
}

/**
 * Field (TOC, page numbers, etc.)
 */
export interface Field {
  id: number;
  instr?: string;
  result: string;
  kind?: string;
}

/**
 * Image reference
 */
export interface ImageRef {
  id: string;
  source_part: string;
  target: string;
  content_type?: string;
}

/**
 * Complete Word document
 */
export interface DocxDocument {
  metadata: Metadata;
  settings: Record<string, string>;
  page_count?: number;
  toc: TocEntry[];
  styles: Style[];
  numbering: {
    abstract_nums: Array<{ id: number; levels: Array<{ level: number; format?: string; text?: string; start?: number }> }>;
    nums: Array<{ id: number; abstract_id?: number }>;
  };
  footnotes: Note[];
  endnotes: Note[];
  comments: Comment[];
  revisions: Revision[];
  fields: Field[];
  headers: HeaderFooter[];
  footers: HeaderFooter[];
  blocks: Block[];
  images: ImageRef[];
}

/**
 * Parse a Word document
 * 
 * @param path - Path to the .docx file
 * @param options - Parsing options
 * @returns The parsed document
 * @throws Error if the file cannot be parsed
 */
export function parseDocx(path: string, options?: ParseOptions): DocxDocument;

/**
 * Parse a Word document to NDJSON
 * 
 * @param path - Path to the .docx file
 * @param outPath - Path for the output .ndjson file
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 */
export function parseDocxNdjson(path: string, outPath: string, options?: ParseOptions): void;

/**
 * Parse a Word document to Arrow IPC file
 * 
 * @param path - Path to the .docx file
 * @param outDir - Output directory for Arrow IPC files
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 */
export function parseDocxArrowIpc(path: string, outDir: string, options?: ParseOptions): void;

/**
 * Parse a Word document to Arrow IPC dataset (multiple files)
 * 
 * @param path - Path to the .docx file
 * @param outDir - Output directory for Arrow IPC files
 * @param options - Parsing options
 * @returns Array of output file paths
 * @throws Error if the file cannot be parsed or written
 */
export function parseDocxArrowDataset(path: string, outDir: string, options?: ParseOptions): string[];

/**
 * Extract images from a Word document
 * 
 * @param path - Path to the .docx file
 * @param outDir - Output directory for extracted images
 * @returns Array of paths to extracted images
 * @throws Error if the file cannot be parsed or images cannot be extracted
 */
export function extractDocxImages(path: string, outDir: string): string[];

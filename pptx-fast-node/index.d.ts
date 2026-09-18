/**
 * pptx-fast-node - High-performance PowerPoint (.pptx) parser
 * 
 * @example
 * ```typescript
 * import { parsePptx, extractPptxImages } from 'pptx-fast-node';
 * 
 * const presentation = parsePptx('/path/to/slides.pptx');
 * console.log(`Slides: ${presentation.slides.length}`);
 * 
 * for (const slide of presentation.slides) {
 *   console.log(`Slide ${slide.id}: ${slide.shapes.length} shapes`);
 * }
 * ```
 */

/**
 * Options for parsing PowerPoint presentations
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
 * Text run with formatting
 */
export interface Run {
  text: string;
  bold: boolean;
  italic: boolean;
  underline: boolean;
}

/**
 * Paragraph within a shape
 */
export interface Paragraph {
  level?: number;
  runs: Run[];
  text: string;
}

/**
 * Text-containing shape on a slide
 */
export interface ShapeText {
  shape_id?: number;
  name?: string;
  placeholder?: string;
  paragraphs: Paragraph[];
}

/**
 * Table cell
 */
export interface TableCell {
  text: string;
}

/**
 * Table row
 */
export interface TableRow {
  cells: TableCell[];
}

/**
 * Table on a slide
 */
export interface Table {
  rows: TableRow[];
}

/**
 * Slide comment
 */
export interface Comment {
  id: number;
  author?: string;
  date?: string;
  text?: string;
  slide_id: number;
}

/**
 * Slide master (template)
 */
export interface SlideMaster {
  id: string;
  target: string;
  shapes: ShapeText[];
  tables: Table[];
}

/**
 * Slide layout
 */
export interface SlideLayout {
  id: string;
  target: string;
  shapes: ShapeText[];
  tables: Table[];
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
 * A single slide in the presentation
 */
export interface Slide {
  /** Slide number (1-based) */
  id: number;
  /** Optional slide name/title */
  name?: string;
  /** Text shapes on the slide */
  shapes: ShapeText[];
  /** Tables on the slide */
  tables: Table[];
  /** Speaker notes for this slide */
  notes: ShapeText[];
}

/**
 * Complete PowerPoint presentation
 */
export interface PptxDocument {
  metadata: Metadata;
  slides: Slide[];
  masters: SlideMaster[];
  layouts: SlideLayout[];
  comments: Comment[];
  images: ImageRef[];
}

/**
 * Parse a PowerPoint presentation
 * 
 * @param path - Path to the .pptx file
 * @param options - Parsing options
 * @returns The parsed presentation
 * @throws Error if the file cannot be parsed
 * 
 * @example
 * ```typescript
 * const presentation = parsePptx('slides.pptx');
 * 
 * for (const slide of presentation.slides) {
 *   console.log(`Slide ${slide.id}:`);
 *   
 *   // Text shapes
 *   for (const shape of slide.shapes) {
 *     console.log(`  Shape: ${shape.name}`);
 *     for (const para of shape.paragraphs) {
 *       console.log(`    ${para.text}`);
 *     }
 *   }
 *   
 *   // Tables
 *   for (const table of slide.tables) {
 *     for (const row of table.rows) {
 *       console.log(row.cells.map(c => c.text).join('\t'));
 *     }
 *   }
 *   
 *   // Notes
 *   for (const note of slide.notes) {
 *     console.log(`Note: ${note.paragraphs.map(p => p.text).join(' ')}`);
 *   }
 * }
 * ```
 */
export function parsePptx(path: string, options?: ParseOptions): PptxDocument;

/**
 * Parse a PowerPoint presentation to NDJSON
 * 
 * @param path - Path to the .pptx file
 * @param outPath - Path for the output .ndjson file
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 */
export function parsePptxNdjson(path: string, outPath: string, options?: ParseOptions): void;

/**
 * Parse a PowerPoint presentation to Arrow IPC file
 * 
 * @param path - Path to the .pptx file
 * @param outDir - Output directory for Arrow IPC files
 * @param options - Parsing options
 * @throws Error if the file cannot be parsed or written
 */
export function parsePptxArrowIpc(path: string, outDir: string, options?: ParseOptions): void;

/**
 * Parse a PowerPoint presentation to Arrow IPC dataset (multiple files)
 * 
 * @param path - Path to the .pptx file
 * @param outDir - Output directory for Arrow IPC files
 * @param options - Parsing options
 * @returns Array of output file paths
 * @throws Error if the file cannot be parsed or written
 */
export function parsePptxArrowDataset(path: string, outDir: string, options?: ParseOptions): string[];

/**
 * Extract images from a PowerPoint presentation
 * 
 * @param path - Path to the .pptx file
 * @param outDir - Output directory for extracted images
 * @returns Array of paths to extracted images
 * @throws Error if the file cannot be parsed or images cannot be extracted
 * 
 * @example
 * ```typescript
 * const imagePaths = extractPptxImages('slides.pptx', './images');
 * console.log('Extracted:', imagePaths);
 * ```
 */
export function extractPptxImages(path: string, outDir: string, options?: ParseOptions): string[];

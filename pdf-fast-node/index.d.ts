export interface FullTextDiagnostics {
  pages: number;
  runs: number;
  chars: number;
  fallbackChars: number;
  viaToUnicode: number;
  viaType0: number;
  viaDifferences: number;
  viaNamed: number;
  viaStandard: number;
  viaRawByte: number;
}

export interface FullTextResult {
  text: string;
  diagnostics: FullTextDiagnostics;
}

export declare class PdfScanner {
  constructor(path: string);
  scanJson(): string;
  semanticRunsJson(): string;
  paragraphsJson(): string;
  semanticRunsArrowIpc(): Buffer;
  pageMultimodalArrowIpc(): Buffer;
  /** Single-page multimodal batch as Arrow IPC Buffer (Node-safe replacement for Python C-Data pointers). */
  pageMultimodalIpc(pageIndex: number): Buffer;
  pageBitmapComponentsJson(pageIndex: number): string;
  pageTableLayoutArrowIpc(pageIndex: number, epsilon?: number): Buffer;
  pageTableLayoutFromOcrArrowIpc(pageIndex: number, ocrRunsJson: string, epsilon?: number, dpi?: number): Buffer;
  pageTableArrowIpc(pageIndex: number, epsilon?: number): Buffer;
  pageTableFromOcrArrowIpc(pageIndex: number, ocrRunsJson: string, epsilon?: number, dpi?: number): Buffer;
  pageCount(): number;
  pageDimensionsJson(): string;
  pageRasterizedPng(pageIndex: number, dpi?: number): Buffer | null;
  debugReadingOrderBlocks(pageIndex: number): string;
  readingOrderBlocksJson(pageIndex: number): string;
  fullText(readingOrder?: boolean): FullTextResult;
}

export declare function scanPdfJson(path: string): string;
export declare function scanPdfSemanticRunsJson(path: string): string;
export declare function scanPdfParagraphsJson(path: string): string;
export declare function scanPdfSemanticRunsArrowIpc(path: string): Buffer;
export declare function scanPdfPageMultimodalArrowIpc(path: string): Buffer;
export declare function scanPdfPageMultimodalIpc(path: string, pageIndex: number): Buffer;
export declare function scanPdfPageBitmapComponentsJson(path: string, pageIndex: number): string;
export declare function scanPdfPageTableLayoutArrowIpc(path: string, pageIndex: number, epsilon?: number): Buffer;
export declare function scanPdfPageTableLayoutFromOcrArrowIpc(path: string, pageIndex: number, ocrRunsJson: string, epsilon?: number, dpi?: number): Buffer;
export declare function scanPdfPageTableArrowIpc(path: string, pageIndex: number, epsilon?: number): Buffer;
export declare function scanPdfPageTableFromOcrArrowIpc(path: string, pageIndex: number, ocrRunsJson: string, epsilon?: number, dpi?: number): Buffer;
export declare function scanPdfPageCount(path: string): number;
export declare function scanPdfPageDimensionsJson(path: string): string;
export declare function scanPdfPageRasterizedPng(path: string, pageIndex: number, dpi?: number): Buffer | null;
export declare function scanPdfFullText(path: string, readingOrder?: boolean): FullTextResult;

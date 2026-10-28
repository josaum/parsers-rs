/** Extract specific 1-based pages from a source PDF into a new PDF. */
export function extractPages(source: string, pages: number[], output: string): void;

/** Extract specific 1-based pages from a PDF path; return subset PDF bytes. */
export function extractPagesToBytes(source: string, pages: number[]): Buffer;

/** Extract specific 1-based pages from in-memory PDF bytes. */
export function extractPagesFromBytes(data: Buffer, pages: number[]): Buffer;

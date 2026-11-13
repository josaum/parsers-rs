#!/usr/bin/env node
// Load-and-parse smoke for the Node bindings. Each fixture is generated here,
// as a STORE-only OOXML zip or a plain EML, so the check needs no committed
// binary and survives a fresh checkout. Exits non-zero on the first failure.
//
// Usage: node scripts/smoke-node.mjs [docx|xlsx|pptx|email|geo|xsd|pdf|pdf-write|xlsx-write|json ...]
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, writeFileSync, readFileSync, rmSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { crc32 } from 'node:zlib';
import path from 'node:path';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Minimal ZIP writer (STORE, no compression): local headers, central directory, end record.
function zipStore(parts) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, text] of parts) {
    const nameBuf = Buffer.from(name);
    const data = Buffer.from(text);
    const crc = crc32(data);
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0); lh.writeUInt16LE(20, 4); lh.writeUInt16LE(0, 6);
    lh.writeUInt16LE(0, 8); lh.writeUInt16LE(0, 10); lh.writeUInt16LE(0, 12);
    lh.writeUInt32LE(crc, 14); lh.writeUInt32LE(data.length, 18); lh.writeUInt32LE(data.length, 22);
    lh.writeUInt16LE(nameBuf.length, 26); lh.writeUInt16LE(0, 28);
    const cd = Buffer.alloc(46);
    cd.writeUInt32LE(0x02014b50, 0); cd.writeUInt16LE(20, 4); cd.writeUInt16LE(20, 6);
    cd.writeUInt16LE(0, 8); cd.writeUInt16LE(0, 10); cd.writeUInt16LE(0, 12); cd.writeUInt16LE(0, 14);
    cd.writeUInt32LE(crc, 16); cd.writeUInt32LE(data.length, 20); cd.writeUInt32LE(data.length, 24);
    cd.writeUInt16LE(nameBuf.length, 28); cd.writeUInt16LE(0, 30); cd.writeUInt16LE(0, 32);
    cd.writeUInt16LE(0, 34); cd.writeUInt16LE(0, 36); cd.writeUInt32LE(0, 38); cd.writeUInt32LE(offset, 42);
    local.push(lh, nameBuf, data);
    central.push(cd, nameBuf);
    offset += lh.length + nameBuf.length + data.length;
  }
  const cdBuf = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0); end.writeUInt16LE(0, 4); end.writeUInt16LE(0, 6);
  end.writeUInt16LE(parts.length, 8); end.writeUInt16LE(parts.length, 10);
  end.writeUInt32LE(cdBuf.length, 12); end.writeUInt32LE(offset, 16); end.writeUInt16LE(0, 20);
  return Buffer.concat([...local, cdBuf, end]);
}

const relsNs = 'http://schemas.openxmlformats.org/package/2006/relationships';
const officeRelNs = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';
const rels = (entries) => `<?xml version="1.0" encoding="UTF-8"?>\n<Relationships xmlns="${relsNs}">${
  entries.map(([id, type, target]) => `<Relationship Id="${id}" Type="${officeRelNs}/${type}" Target="${target}"/>`).join('')
}</Relationships>`;

function pdfFixture() {
  const content = (text) => {
    const body = `BT /F1 24 Tf 20 100 Td (${text}) Tj ET`;
    return `<< /Length ${body.length} >>\nstream\n${body}\nendstream`;
  };
  const page = (contents) => `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents ${contents} 0 R /Resources << /Font << /F1 7 0 R >> >> >>`;
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>',
    page(4), content('Hello PDF'), page(6), content('Second page'),
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let out = '%PDF-1.4\n';
  const offsets = [];
  objects.forEach((body, i) => { offsets.push(out.length); out += `${i + 1} 0 obj\n${body}\nendobj\n`; });
  const xref = out.length;
  out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (const offset of offsets) out += `${String(offset).padStart(10, '0')} 00000 n \n`;
  out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, 'latin1');
}

const fixtures = {
  docx: () => zipStore([
    ['_rels/.rels', rels([['rId1', 'officeDocument', 'word/document.xml']])],
    ['word/_rels/document.xml.rels', rels([])],
    ['word/document.xml', `<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>Hello DOCX</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell 1</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>`],
  ]),
  xlsx: () => zipStore([
    ['_rels/.rels', rels([['rId1', 'officeDocument', 'xl/workbook.xml']])],
    ['xl/workbook.xml', `<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="${officeRelNs}"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>`],
    ['xl/_rels/workbook.xml.rels', rels([['rId1', 'worksheet', 'worksheets/sheet1.xml'], ['rId2', 'sharedStrings', 'sharedStrings.xml']])],
    ['xl/sharedStrings.xml', `<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>Hello XLSX</t></si></sst>`],
    ['xl/worksheets/sheet1.xml', `<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c></row></sheetData></worksheet>`],
  ]),
  pptx: () => zipStore([
    ['_rels/.rels', rels([['rId1', 'officeDocument', 'ppt/presentation.xml']])],
    ['ppt/presentation.xml', `<?xml version="1.0" encoding="UTF-8"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="${officeRelNs}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>`],
    ['ppt/_rels/presentation.xml.rels', rels([['rId1', 'slide', 'slides/slide1.xml']])],
    ['ppt/slides/slide1.xml', `<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld name="Intro"><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/></p:nvSpPr><p:txBody><a:p><a:r><a:t>Hello PPTX</a:t></a:r></a:p></p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>`],
  ]),
  email: () => Buffer.from([
    'From: Alice <alice@example.com>',
    'To: Bob <bob@example.com>',
    'Subject: Hello EML',
    'Message-ID: <smoke-1@example.com>',
    'Date: Thu, 18 Sep 2026 12:00:00 +0000',
    'Content-Type: text/plain; charset=utf-8',
    '',
    'Hello EML body',
    '',
  ].join('\r\n')),
  // 2x2 8-bit little-endian GeoTIFF in EPSG:4326, PixelIsPoint, samples 1..4 (read back as n/255).
  geo: () => {
    const geokeys = [1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 2, 2048, 0, 1, 4326];
    const dataAt = 8 + 2 + 12 * 12 + 4;
    const tags = [
      [256, 3, 1, 2], [257, 3, 1, 2], [258, 3, 1, 8], [259, 3, 1, 1], [262, 3, 1, 1],
      [273, 4, 1, dataAt], [277, 3, 1, 1], [278, 3, 1, 2], [279, 4, 1, 4],
      [33550, 12, 3, dataAt + 4], [33922, 12, 6, dataAt + 28], [34735, 3, geokeys.length, dataAt + 76],
    ];
    const buf = Buffer.alloc(dataAt + 76 + geokeys.length * 2);
    buf.write('II', 0, 'latin1'); buf.writeUInt16LE(42, 2); buf.writeUInt32LE(8, 4); buf.writeUInt16LE(tags.length, 8);
    tags.forEach(([tag, type, count, value], i) => {
      const at = 10 + i * 12;
      buf.writeUInt16LE(tag, at); buf.writeUInt16LE(type, at + 2); buf.writeUInt32LE(count, at + 4);
      if (type === 3 && count === 1) buf.writeUInt16LE(value, at + 8); else buf.writeUInt32LE(value, at + 8);
    });
    Buffer.from([1, 2, 3, 4]).copy(buf, dataAt);
    [1, 1, 0].forEach((v, i) => buf.writeDoubleLE(v, dataAt + 4 + i * 8));
    [0, 0, 0, 10, 50, 0].forEach((v, i) => buf.writeDoubleLE(v, dataAt + 28 + i * 8));
    geokeys.forEach((v, i) => buf.writeUInt16LE(v, dataAt + 76 + i * 2));
    return buf;
  },
  xsd: () => Buffer.from(`<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:element name="record"><xs:complexType><xs:sequence><xs:element name="name" type="xs:string"/></xs:sequence></xs:complexType></xs:element></xs:schema>`),
  // Two pages of Helvetica text with a classic cross-reference table.
  pdf: () => pdfFixture(),
  'pdf-write': () => pdfFixture(),
  'xlsx-write': () => Buffer.from(JSON.stringify({
    sheets: [{ name: 'Summary', rows: [[{ t: 's', v: 'Hello XLSX write' }, { t: 'n', v: 42 }]] }],
  })),
  json: () => Buffer.from('{"hello":"Hello JSON","nested":{"a":1}}\n{"hello":"JSONL","nested":{"a":2}}\n'),
};

// A bad input must surface as a thrown JavaScript error, never a process abort.
function mustThrow(call, what) {
  let threw = false;
  try { call(); } catch (err) { threw = err instanceof Error; }
  if (!threw) throw new Error(`${what}: expected a thrown Error`);
}

// Each check must be non-vacuous: a fixture string round-trips through the parse.
const checks = {
  docx(m, file, dir) {
    const doc = m.parseDocx(file);
    if (!JSON.stringify(doc.blocks).includes('Hello DOCX')) throw new Error('paragraph text missing');
    m.parseDocxNdjson(file, path.join(dir, 'out.ndjson'));
    const ipcDir = path.join(dir, 'ipc'); m.parseDocxArrowIpc(file, ipcDir);
    if (readdirSync(ipcDir).length === 0) throw new Error('no arrow tables written');
    if (m.extractDocxImages(file, path.join(dir, 'img')).length !== 0) throw new Error('unexpected images');
    return `${doc.blocks.length} blocks, ${readdirSync(ipcDir).length} arrow tables`;
  },
  xlsx(m, file, dir) {
    const wb = m.parseXlsx(file);
    const cells = wb.sheets[0].cells;
    if (!JSON.stringify(cells).includes('Hello XLSX')) throw new Error('shared string missing');
    if (!JSON.stringify(cells).includes('42')) throw new Error('numeric cell missing');
    m.parseXlsxNdjson(file, path.join(dir, 'out.ndjson'));
    const ipc = path.join(dir, 'out.arrow'); m.parseXlsxArrowIpc(file, ipc);
    if (statSync(ipc).size === 0) throw new Error('empty arrow ipc');
    return `${cells.length} cells`;
  },
  pptx(m, file, dir) {
    const p = m.parsePptx(file);
    if (!JSON.stringify(p.slides).includes('Hello PPTX')) throw new Error('slide text missing');
    m.parsePptxNdjson(file, path.join(dir, 'out.ndjson'));
    const ipcDir = path.join(dir, 'ipc'); m.parsePptxArrowIpc(file, ipcDir);
    if (readdirSync(ipcDir).length === 0) throw new Error('no arrow tables written');
    if (m.extractPptxImages(file, path.join(dir, 'img')).length !== 0) throw new Error('unexpected images');
    return `${p.slides.length} slide(s), ${readdirSync(ipcDir).length} arrow tables`;
  },
  email(m, file, dir) {
    const e = m.parseEmail(file);
    if (e.subject !== 'Hello EML') throw new Error(`subject: ${e.subject}`);
    if (!(e.body.text || '').includes('Hello EML body')) throw new Error('body missing');
    if (m.parseEml(file).from.address !== 'alice@example.com') throw new Error('from missing');
    m.parseEmailNdjson(file, path.join(dir, 'out.ndjson'), { includeRawHeaders: true, maxAttachmentSize: 1024 });
    if (!m.parseEmail(file, { include_raw_headers: true }).headers) throw new Error('raw headers option not forwarded');
    if (m.extractAttachments(file, path.join(dir, 'att'), 1024).length !== 0) throw new Error('unexpected attachments');
    return `subject=${e.subject}`;
  },
  geo(m, file) {
    const meta = JSON.parse(m.geotiffMetadataJson(file));
    if (meta.width !== 2 || meta.epsg_code !== 4326 || meta.pixel_is_point !== true) throw new Error(`metadata: ${JSON.stringify(meta)}`);
    const bands = m.geotiffBands(file);
    if (!(bands[0] instanceof Float32Array) || bands[0].length !== 4 || Array.from(bands[0]).some((v, i) => Math.abs(v - (i + 1) / 255) > 1e-6)) throw new Error('band samples missing');
    mustThrow(() => m.geotiffBands(file, 15), 'band limit');
    mustThrow(() => m.geotiffMetadataJson(file + '.missing'), 'missing file');
    if (Math.abs(m.computeNdvi([0.5], [0.1])[0] - 2 / 3) > 1e-6) throw new Error('ndvi value');
    mustThrow(() => m.computeNdvi([0.5], [0.1, 0.2]), 'ndvi length');
    if (Math.abs(m.spectralAngle([1, 0], [0, 1]) - Math.PI / 2) > 1e-3) throw new Error('spectral angle value');
    mustThrow(() => m.spectralAngle([1], [1, 0]), 'spectral angle length');
    const recs = JSON.parse(m.parseGeojsonToRecordsJson('{"type":"Feature","geometry":{"type":"Point","coordinates":[10,50]},"properties":{}}'));
    if (recs[0].lon !== 10) throw new Error('geojson point missing');
    mustThrow(() => m.bboxAreaKm2('nope'), 'bad bbox');
    return `version=${m.version()}, ${bands.length} band(s)`;
  },
  xsd(m, file) {
    if (m.listElements(file).join() !== 'record') throw new Error('elements missing');
    const all = JSON.parse(m.allArrowSchemasJson(file));
    if (JSON.stringify(all.record) !== m.describeElementJson(file, 'record')) throw new Error('all schemas disagree with one');
    if (m.validateBytesJson(file, Buffer.from('<record><name>a</name></record>')) !== '[]') throw new Error('valid document refused');
    if (m.schemaIpcBytes(file, 'record').length === 0) throw new Error('empty schema ipc');
    mustThrow(() => m.allArrowSchemasJson(file + '.missing'), 'missing schema');
    return `${Object.keys(all).length} element(s)`;
  },
  pdf(m, file, dir) {
    if (m.scanPdfPageCount(file) !== 2) throw new Error('page count');
    const full = m.scanPdfFullText(file);
    if (!full.text.includes('Hello PDF') || !full.text.includes('Second page')) throw new Error(`text: ${full.text}`);
    if (full.diagnostics.pages !== 2) throw new Error('diagnostics pages');
    if (!m.scanPdfSemanticRunsJson(file).includes('Hello PDF')) throw new Error('semantic runs missing');
    if (JSON.parse(m.scanPdfPageDimensionsJson(file)).length !== 2) throw new Error('page dimensions');
    if (m.scanPdfSemanticRunsArrowIpc(file).length === 0) throw new Error('empty arrow ipc');
    const scanner = new m.PdfScanner(file);
    if (scanner.pageCount() !== 2) throw new Error('scanner page count');
    mustThrow(() => m.scanPdfJson(file + '.missing'), 'missing pdf');
    const bad = path.join(dir, 'bad.pdf'); writeFileSync(bad, 'not a pdf');
    // The parser is lenient: bytes that are not a PDF read as an empty document.
    if (m.scanPdfPageCount(bad) !== 0 || m.scanPdfFullText(bad).text !== '') throw new Error('malformed pdf is not empty');
    mustThrow(() => m.scanPdfPageTableArrowIpc(file, 99), 'page out of range');
    mustThrow(() => new m.PdfScanner(file).pageMultimodalIpc(99), 'page out of range through the class');
    return `${full.diagnostics.pages} pages, ${full.text.length} chars`;
  },
  'pdf-write'(m, file, dir) {
    const one = m.extractPagesToBytes(file, [2]);
    if (one.subarray(0, 5).toString('latin1') !== '%PDF-') throw new Error('not a pdf');
    if (!one.includes('Second page') || one.includes('Hello PDF')) throw new Error('wrong page kept');
    const out = path.join(dir, 'subset.pdf'); m.extractPages(file, [1], out);
    if (!readFileSync(out).includes('Hello PDF')) throw new Error('subset file missing its page');
    if (m.extractPagesFromBytes(readFileSync(file), [1, 2]).length === 0) throw new Error('empty subset');
    mustThrow(() => m.extractPagesToBytes(file, [99]), 'page out of range');
    mustThrow(() => m.extractPagesFromBytes(Buffer.from('not a pdf'), [1]), 'malformed pdf');
    return `${one.length} bytes for one page`;
  },
  'xlsx-write'(m, file, dir) {
    const spec = JSON.parse(readFileSync(file, 'utf8'));
    const bytes = m.writeXlsxBytes(spec);
    if (bytes.subarray(0, 2).toString('latin1') !== 'PK') throw new Error('not a zip archive');
    const out = path.join(dir, 'out.xlsx'); m.writeXlsx(JSON.stringify(spec), out);
    if (statSync(out).size === 0) throw new Error('empty workbook');
    const back = require(path.join(root, 'xlsx-fast-node')).parseXlsx(out);
    if (!JSON.stringify(back.sheets[0].cells).includes('Hello XLSX write')) throw new Error('written cell not read back');
    mustThrow(() => m.writeXlsxBytes('{not json'), 'malformed spec');
    mustThrow(() => m.writeXlsx(spec, path.join(dir, 'no-such-dir', 'out.xlsx')), 'unwritable path');
    return `${bytes.length} bytes`;
  },
  json(m, file, dir) {
    const text = readFileSync(file, 'utf8');
    const firstLine = text.split('\n')[0];
    const val = m.parseJson(firstLine);
    if (val.hello !== 'Hello JSON') throw new Error('json parse failed');
    const rows = m.parseJsonl(text);
    if (rows.length !== 2) throw new Error('jsonl parse count mismatch');
    const count = m.countLines(text);
    if (count !== 2) throw new Error(`count lines mismatch: ${count}`);
    return `version=${m.version()}, lines=${count}`;
  },
};

const ext = { docx: 'docx', xlsx: 'xlsx', pptx: 'pptx', email: 'eml', geo: 'tif', xsd: 'xsd', pdf: 'pdf', 'pdf-write': 'pdf', 'xlsx-write': 'json', json: 'json' };
// The writer crates are named `<format>-fast-write-node`.
const crateOf = (name) => (name.endsWith('-write') ? `${name.slice(0, -6)}-fast-write-node` : `${name}-fast-node`);
const wanted = process.argv.slice(2).length ? process.argv.slice(2) : Object.keys(checks);
let failures = 0;
for (const name of wanted) {
  const crate = crateOf(name);
  const dir = mkdtempSync(path.join(tmpdir(), `smoke-${name}-`));
  try {
    const mod = require(path.join(root, crate));
    const file = path.join(dir, `fixture.${ext[name]}`);
    writeFileSync(file, fixtures[name]());
    console.log(`ok ${crate}: exports ${Object.keys(mod).join(',')}; ${checks[name](mod, file, dir)}`);
  } catch (err) {
    failures += 1;
    console.error(`FAIL ${crate}: ${err && err.stack ? err.stack : err}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
if (failures) { console.error(`${failures} smoke failure(s)`); process.exit(1); }
console.log('all node-binding smoke checks passed');

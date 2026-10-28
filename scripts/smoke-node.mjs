#!/usr/bin/env node
// Load-and-parse smoke for the Node bindings. Each fixture is generated here,
// as a STORE-only OOXML zip or a plain EML, so the check needs no committed
// binary and survives a fresh checkout. Exits non-zero on the first failure.
//
// Usage: node scripts/smoke-node.mjs [docx|xlsx|pptx|email ...]
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
  json: () => Buffer.from('{"hello":"Hello JSON","nested":{"a":1}}\n{"hello":"JSONL","nested":{"a":2}}\n'),
};

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

const ext = { docx: 'docx', xlsx: 'xlsx', pptx: 'pptx', email: 'eml', json: 'json' };
const wanted = process.argv.slice(2).length ? process.argv.slice(2) : Object.keys(checks);
let failures = 0;
for (const name of wanted) {
  const crate = `${name}-fast-node`;
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

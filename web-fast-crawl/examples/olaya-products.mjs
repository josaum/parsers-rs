/** Optional O-Laya postprocessor. Reads crawl evidence; never fetches pages or rewrites prices. */
import { readFile, writeFile, stat } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

function sourceEvidence(source, pointer) {
  if (!['html_product_detail', 'html_product_cards'].includes(source.kind)) return undefined;
  let value = source.value;
  if (pointer !== '') {
    if (typeof pointer !== 'string' || !pointer.startsWith('/')) throw new Error('Invalid source pointer');
    for (const segment of pointer.slice(1).split('/')) {
      if (/~[^01]|~$/.test(segment)) throw new Error('Invalid source pointer');
      const key = segment.replace(/~1/g, '/').replace(/~0/g, '~');
      if (value === null || typeof value !== 'object' || !Object.hasOwn(value, key)) throw new Error('Source pointer does not resolve');
      value = value[key];
    }
  }
  return value?.evidence;
}

export async function enrichProducts(report, runtime, ontology, minimumConfidence = 0.2) {
  if (!Number.isFinite(minimumConfidence) || minimumConfidence < 0 || minimumConfidence > 1) throw new Error('Invalid confidence threshold');
  if (!Array.isArray(report.pages) || !Array.isArray(ontology.concepts) || ontology.concepts.length === 0) throw new Error('Invalid report or ontology');
  const dag = runtime.OntologyDAG.fromJSON(ontology);
  const engine = new runtime.OLaya(dag);
  const products = [];
  for (const page of report.pages) {
    if (page.robots?.noindex) continue;
    const data = page.structured;
    for (const product of data?.products ?? []) {
      if (products.length >= 10000) throw new Error('Product limit exceeded');
      const source = data.sources?.[product.source];
      if (!source) throw new Error('Product source is missing');
      // Inference is limited to product text. Prices and DOM metadata are evidence,
      // not ontology instructions or a request for an external action.
      const state = [product.name, product.description, product.brand].filter(x => typeof x === 'string').join('\n');
      const decision = await engine.systemOne(state, Math.min(ontology.concepts.length, 64));
      const selected = decision.selectedConcept;
      const lineage = [...dag.getAncestors(selected.id)].map(id => dag.getById(id))
        .sort((a, b) => dag.getAncestors(a.id).size - dag.getAncestors(b.id).size || a.curie.localeCompare(b.curie))
        .map(concept => concept.curie).concat(selected.curie);
      const accepted = decision.axiomsVerified && decision.confidence >= minimumConfidence && selected.parents.length > 0;
      const missing = ['name', 'description'].filter(key => !product[key]);
      for (const key of ['price', 'currency', 'availability', 'seller']) {
        if (!(product.offers ?? []).some(offer => offer[key] !== null && offer[key] !== undefined && offer[key] !== '')) missing.push(key);
      }
      products.push({
        ...product,
        provenance: { page_url: page.final_url ?? page.url, source_kind: source.kind, source_locator: source.locator, source_value_sha256: source.value_sha256, json_pointer: product.json_pointer, extraction_evidence: sourceEvidence(source, product.json_pointer) },
        missing_fields: missing,
        classification: { status: accepted ? 'classified' : 'needs_review', curie: accepted ? selected.curie : null, candidate_curie: selected.curie, label: selected.label, lineage, confidence: decision.confidence, axioms_verified: decision.axiomsVerified, probabilities: decision.probabilities }
      });
    }
  }
  return { engine: 'O-Laya Node lexical', minimum_confidence: minimumConfidence,
    limitations: ['Confidence measures lexical concentration, not calibrated accuracy.', 'Axiom verification checks ontology consistency, not the truth of source claims.', 'Prices, stock, and descriptions remain source observations; missing fields are not generated.'], products };
}

async function main() {
  const { values } = parseArgs({ options: { input: { type: 'string' }, output: { type: 'string' }, runtime: { type: 'string' }, ontology: { type: 'string', default: fileURLToPath(new URL('./commerce-ontology.json', import.meta.url)) }, 'minimum-confidence': { type: 'string', default: '0.2' } } });
  if (!values.input || !values.output || !values.runtime) throw new Error('Usage: --input crawl.json --output products.json --runtime /path/to/olaya/packages/olaya-node/dist/index.mjs [--ontology taxonomy.json]');
  if ((await stat(values.input)).size > 128 * 1024 * 1024) throw new Error('Report exceeds 128 MiB');
  const [inputBytes, ontologyBytes, runtimeBytes] = await Promise.all([readFile(values.input), readFile(values.ontology), readFile(values.runtime)]);
  const runtime = await import(pathToFileURL(resolve(values.runtime)).href);
  const result = await enrichProducts(JSON.parse(inputBytes), runtime, JSON.parse(ontologyBytes), Number(values['minimum-confidence']));
  const sha = bytes => createHash('sha256').update(bytes).digest('hex');
  result.provenance = { input_sha256: sha(inputBytes), ontology_sha256: sha(ontologyBytes), runtime_sha256: sha(runtimeBytes) };
  await writeFile(values.output, JSON.stringify(result, null, 2) + '\n');
  console.log(JSON.stringify({ products: result.products.length, classified: result.products.filter(p => p.classification.status === 'classified').length, output: values.output }));
}
if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) main().catch(error => { console.error(error instanceof SyntaxError ? 'Invalid JSON input' : error.message); process.exitCode = 1; });

import { parse as goParse } from '@astrojs/compiler';
import { parse as rsParse } from '@astrojs/compiler-rs';
const src = process.argv[2].replace(/\\n/g,'\n');
const strip = (n) => JSON.parse(JSON.stringify(n, (k, v) => (k === 'position' || k==='decorators'||k==='optional'||k==='typeAnnotation') ? undefined : v));
const g = await goParse(src, { position: false });
console.log('GO', JSON.stringify(strip(g)));
const r = rsParse(src);
const body = { ...r.ast }; delete body.frontmatter;
console.log('RS', JSON.stringify(strip(body)), JSON.stringify(r.diagnostics));

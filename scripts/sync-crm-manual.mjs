// Rebuilds api/src/chat/crm_manual.json from the CRM repo: the manual
// (docs/manual/*.md) and the topic table of app/utils/crmManual.server.ts.
// Usage: node scripts/sync-crm-manual.mjs ../general_datebase
import fs from 'node:fs'
import path from 'node:path'

const crm = process.argv[2] ?? '../general_datebase'
const src = fs.readFileSync(path.join(crm, 'app/utils/crmManual.server.ts'), 'utf8')
const grab = name => {
  const start = src.indexOf(`const ${name}`)
  const eq = src.indexOf('=', start)
  const end = src.indexOf('\n]\n', eq)
  return eval(`(${src.slice(eq + 1, end + 2)})`)
}
const dir = path.join(crm, 'docs/manual')
const sections = fs
  .readdirSync(dir)
  .filter(f => f.endsWith('.md') && f !== '_intro.md')
  .sort()
  .map(f => ({ id: f.replace(/\.md$/, ''), content: fs.readFileSync(path.join(dir, f), 'utf8').trim() }))
const manual = {
  intro: fs.readFileSync(path.join(dir, '_intro.md'), 'utf8').trim(),
  sections,
  core: grab('CORE_SECTIONS'),
  topics: grab('TOPIC_SECTIONS'),
  fallback: grab('FALLBACK_SECTIONS'),
}
fs.writeFileSync(
  new URL('../api/src/chat/crm_manual.json', import.meta.url),
  `${JSON.stringify(manual, null, 1)}\n`,
)

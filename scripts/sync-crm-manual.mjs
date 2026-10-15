// Refreshes the manual text in api/src/chat/crm_manual.json from the CRM
// repo's docs/manual/*.md. The topic table (which keywords pick which
// sections) lives in the JSON itself; edit it there.
// Usage: node scripts/sync-crm-manual.mjs ../general_datebase
import fs from 'node:fs'
import path from 'node:path'

const crm = process.argv[2] ?? '../general_datebase'
const target = new URL('../api/src/chat/crm_manual.json', import.meta.url)
const current = JSON.parse(fs.readFileSync(target, 'utf8'))
const dir = path.join(crm, 'docs/manual')
const sections = fs
  .readdirSync(dir)
  .filter(f => f.endsWith('.md') && f !== '_intro.md')
  .sort()
  .map(f => ({
    id: f.replace(/\.md$/, ''),
    content: fs.readFileSync(path.join(dir, f), 'utf8').trim(),
  }))
const ids = new Set(sections.map(s => s.id))
for (const id of [
  ...current.core,
  ...current.fallback,
  ...current.topics.flatMap(t => [t.id, ...(t.related ?? [])]),
]) {
  if (!ids.has(id)) console.warn(`topic table names a missing section: ${id}`)
}
const manual = {
  ...current,
  intro: fs.readFileSync(path.join(dir, '_intro.md'), 'utf8').trim(),
  sections,
}
fs.writeFileSync(target, `${JSON.stringify(manual, null, 1)}\n`)

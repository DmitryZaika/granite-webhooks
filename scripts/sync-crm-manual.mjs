// Refreshes the manual text in api/src/chat/crm_manual.json from the CRM
// repo's docs/manual/*.md. The topic table (which keywords pick which
// sections) and the clarify table (words of several features, asked back)
// live in the JSON itself; edit them there. Also copies the CRM repo's test
// cases of the how-to detector (both repos run the same ones).
// Usage: node scripts/sync-crm-manual.mjs ../general_datebase
import fs from 'node:fs'
import path from 'node:path'

const crm = process.argv[2] ?? '../general_datebase'
const target = new URL('../api/src/chat/crm_manual.json', import.meta.url)
const current = JSON.parse(fs.readFileSync(target, 'utf8'))
const dir = path.join(crm, 'docs/manual')
// A Windows checkout has CRLF line ends; the JSON keeps LF.
const read = file => fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n').trim()
const sections = fs
  .readdirSync(dir)
  .filter(f => f.endsWith('.md') && f !== '_intro.md')
  .sort()
  .map(f => ({
    id: f.replace(/\.md$/, ''),
    content: read(path.join(dir, f)),
  }))
const ids = new Set(sections.map(s => s.id))
for (const id of [
  ...current.core,
  ...current.fallback,
  ...current.topics.flatMap(t => [t.id, ...(t.related ?? [])]),
  ...(current.clarify ?? []).flatMap(c => c.choices.flatMap(choice => choice.sections)),
]) {
  if (!ids.has(id)) console.warn(`topic table names a missing section: ${id}`)
}
const manual = {
  ...current,
  intro: read(path.join(dir, '_intro.md')),
  sections,
}
fs.writeFileSync(target, `${JSON.stringify(manual, null, 1)}\n`)
fs.writeFileSync(
  new URL('../api/src/chat/crm_manual_how_to_cases.json', import.meta.url),
  `${read(path.join(crm, 'app/utils/crmManualHowToCases.json'))}\n`,
)

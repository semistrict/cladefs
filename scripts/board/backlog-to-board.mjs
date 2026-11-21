#!/usr/bin/env node
// Moves Backlog.md's tasks onto the board (the user's call, 2026-11-21): every task, Done ones
// included, as a board task in one channel, so the board is where tasks live from then on.
//
//   BOARD_URL=https://<board host>/api/board node scripts/board/backlog-to-board.mjs [--channel tasks]
//
// Each task keeps its Backlog ID as the label `task` (TASK-633.11), which the repo's commits and
// comments name, and its root is the whole task as Markdown: description, criteria, plan, notes,
// final summary. Dependencies become the board's `needs`; Done tasks are completed with their
// final summary as the result. Run again, it makes the tasks filed since and brings the others up
// to what Backlog.md has now, so a last run at the switch catches up.

import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { board, createTasksInChunks } from '@vercel/board'

const run = promisify(execFile)

// The board's limits (the local board's, which are the tighter): a message's text, a task's
// needs, a result.
export const TEXT_MAX = 10_000
export const NEEDS_MAX = 20
export const RESULT_MAX = 1_000

// Every task, as `backlog task view --json` has it, read from the repo at ROOT.
export async function loadBacklog(root, { concurrency = 8 } = {}) {
  const backlog = async (...args) =>
    JSON.parse((await run('backlog', args, { cwd: root, maxBuffer: 256 << 20 })).stdout)
  const { tasks: listed, total } = await backlog('task', 'list', '--json', '--max-count', '100000')
  if (total !== undefined && listed.length !== total) throw new Error(`backlog listed ${listed.length} of ${total} tasks`)
  const tasks = new Array(listed.length)
  let next = 0
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (next < listed.length) {
        const at = next++
        tasks[at] = (await backlog('task', 'view', listed[at].id, '--json')).task
      }
    }),
  )
  return tasks
}

// A label key the board takes: lowercase letters, digits, '.', '_' and '-', from a letter or
// digit, up to 64.
export function labelKey(name) {
  const key = name.toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^[^a-z0-9]+/, '')
  if (!key) throw new Error(`no label key can be made of ${JSON.stringify(name)}`)
  return key.slice(0, 64)
}

const checklist = (items) => items.map((c) => `- [${c.checked ? 'x' : ' '}] #${c.index} ${c.text}`).join('\n')

// The task as Markdown, whole: its heading, a line of what it is, then each section it has.
export function markdown(task, { dropped = [] } = {}) {
  const facts = [
    task.status === 'In Progress' && 'was In Progress in Backlog.md',
    task.type && `type ${task.type}`,
    task.priority && `priority ${task.priority}`,
    task.parentTaskId && `subtask of ${task.parentTaskId}`,
    task.subtasks?.length && `subtasks ${task.subtasks.map((s) => s.id).join(', ')}`,
    task.dependencies?.length && `needs ${task.dependencies.join(', ')}`,
    dropped.length && `(${dropped.join(', ')} not on the board: archived in Backlog.md)`,
    task.milestone && `milestone ${task.milestone}`,
    task.assignees?.length && `assigned ${task.assignees.join(', ')}`,
    task.labels?.length && `labels ${task.labels.join(', ')}`,
    `created ${task.createdAt.slice(0, 10)}`,
    task.updatedAt && `updated ${task.updatedAt.slice(0, 10)}`,
  ].filter(Boolean)
  const sections = [
    ['Description', task.description],
    ['Acceptance Criteria', task.acceptanceCriteria?.length && checklist(task.acceptanceCriteria)],
    ['Definition of Done', task.definitionOfDone?.length && checklist(task.definitionOfDone)],
    ['Implementation Plan', task.implementationPlan],
    ['Implementation Notes', task.implementationNotes],
    ['Comments', task.comments?.length && task.comments.map((c) => (typeof c === 'string' ? c : JSON.stringify(c))).join('\n\n')],
    ['Final Summary', task.finalSummary],
    ['References', task.references?.length && task.references.map((r) => `- ${r}`).join('\n')],
    ['Documentation', task.documentation?.length && task.documentation.map((d) => `- ${d}`).join('\n')],
    ['Modified Files', task.modifiedFiles?.length && task.modifiedFiles.map((f) => `- ${f}`).join('\n')],
  ].filter(([, body]) => body && String(body).trim())
  return [
    `# ${task.id}: ${task.title}`,
    facts.join(' · '),
    ...sections.map(([title, body]) => `## ${title}\n\n${String(body).trim()}`),
  ].join('\n\n')
}

// The root's text: the whole task when it fits, else as much as fits and the whole task attached.
export function rootText(full, attachmentName) {
  if (full.length <= TEXT_MAX) return { text: full, attach: false }
  const tail = `\n\n…\n\n_The whole task is attached as ${attachmentName}._`
  return { text: full.slice(0, TEXT_MAX - tail.length) + tail, attach: true }
}

// TASK-26.6.1 as [26, 6, 1], to order IDs as numbers.
const idParts = (id) => id.replace(/^TASK-/, '').split('.').map(Number)
const byId = (a, b) => {
  const [x, y] = [idParts(a.id), idParts(b.id)]
  for (let at = 0; at < Math.max(x.length, y.length); at++) if ((x[at] ?? -1) !== (y[at] ?? -1)) return (x[at] ?? -1) - (y[at] ?? -1)
  return 0
}

// What the board gets for each task, oldest first (the order the board's queue lists them in):
// its text, labels, needs (by Backlog ID, of tasks in TASKS), and for a Done task its result.
export function plan(tasks) {
  const status = new Map(tasks.map((t) => [t.id, t.status]))
  return [...tasks].sort((a, b) => a.createdAt.localeCompare(b.createdAt) || byId(a, b)).map((task) => {
    // A Done task waits only for Done ones: one waiting for an open task couldn't be completed.
    // Its root still names every dependency.
    const needs = (task.dependencies ?? []).filter(
      (d) => status.has(d) && (task.status !== 'Done' || status.get(d) === 'Done'),
    )
    const dropped = (task.dependencies ?? []).filter((d) => !status.has(d))
    if (needs.length > NEEDS_MAX) throw new Error(`${task.id} needs ${needs.length} tasks, over the board's ${NEEDS_MAX}`)
    const labels = { task: task.id }
    if (task.priority) labels.priority = task.priority
    if (task.type) labels.type = labelKey(task.type)
    if (task.parentTaskId) labels.parent = task.parentTaskId
    if (task.milestone) labels.milestone = task.milestone
    if (task.status === 'In Progress') labels.status = 'in-progress'
    if (task.assignees?.length) labels.assignee = task.assignees.map((a) => a.replace(/^@/, '')).join(',')
    for (const name of task.labels ?? []) labels[`tag.${labelKey(name)}`.slice(0, 64)] = 'yes'
    const name = `${task.id}.md`
    const full = markdown(task, { dropped })
    const { text, attach } = rootText(full, name)
    let result = null
    if (task.status === 'Done') {
      const summary = (task.finalSummary ?? '').trim() || 'Done in Backlog.md.'
      const more = '… (the whole summary is in the root)'
      result = summary.length <= RESULT_MAX ? summary : summary.slice(0, RESULT_MAX - more.length) + more
    }
    return { id: task.id, text, labels, needs, attachment: attach ? { name, data: full } : null, result }
  })
}

// CHANNEL's tasks' roots, by their `task` label.
export async function boardTasks(team, channel) {
  const roots = new Map()
  for (let after; ; ) {
    const page = await team.tasks.list({ channel, state: 'all', limit: 500, after })
    for (const { message } of page.tasks) if (message.labels.task) roots.set(message.labels.task, message)
    if (page.tasks.length < 500) return roots
    after = page.cursor
  }
}

const upload = (team, { name, data }) =>
  team.upload({ data: new TextEncoder().encode(data), name, type: 'text/markdown' })

// Brings the task ROOT is the root of up to EACH, what Backlog.md has now: its text, its labels,
// and, while it's open and unclaimed, what it waits for. Whether anything changed.
async function update(team, root, each, byTask) {
  let changed = false
  if (root.text !== each.text) {
    await team.edit(root.id, each.text)
    if (each.attachment) {
      const text = `The whole task, as Backlog.md had it last: ${each.attachment.name}.`
      await team.reply(root.id, { text, attachments: [await upload(team, each.attachment)], noSync: true })
    }
    changed = true
  }
  const labels = Object.fromEntries(Object.entries(root.labels).filter(([key]) => !key.startsWith('board.')))
  const set = Object.fromEntries(Object.entries(each.labels).filter(([key, value]) => labels[key] !== value))
  const unset = Object.keys(labels).filter((key) => !(key in each.labels))
  if (Object.keys(set).length || unset.length) {
    await team.label(root.id, { set, unset })
    changed = true
  }
  const needs = each.needs.map((need) => byTask.get(need))
  const same = needs.length === root.task.needs.length && needs.every((need) => root.task.needs.includes(need))
  if (!same && ['waiting', 'ready', 'blocked'].includes(root.task.state)) {
    await team.tasks.needs(root.id, needs)
    changed = true
  }
  return changed
}

// Puts PLANNED on TEAM's CHANNEL: makes the tasks it lacks, brings those it has up to what
// Backlog.md has now, and completes the Done ones not completed yet. Answers every task's board ID
// by its Backlog ID. Run again, it changes only what Backlog.md changed since.
export async function migrate(team, channel, planned, { log = () => {} } = {}) {
  const roots = await boardTasks(team, channel)
  const byTask = new Map([...roots].map(([task, root]) => [task, root.id]))
  const missing = planned.filter((each) => !roots.has(each.id))
  log(`${roots.size} tasks on the board, ${missing.length} to make`)

  if (missing.length) {
    const making = new Set(missing.map((each) => each.id))
    const batch = []
    for (const each of missing) {
      batch.push({
        key: each.id,
        text: each.text,
        labels: each.labels,
        needs: each.needs.map((need) => (making.has(need) ? { key: need } : byTask.get(need))),
        noSync: true,
        ...(each.attachment ? { attachments: [await upload(team, each.attachment)] } : {}),
      })
    }
    const made = await createTasksInChunks(team, channel, batch)
    missing.forEach((each, at) => byTask.set(each.id, made[at].id))
    log(`made ${made.length} tasks`)
  }

  let updated = 0
  for (const each of planned) {
    const root = roots.get(each.id)
    if (root && (await update(team, root, each, byTask))) updated++
    if (root?.task.state === 'completed' && each.result === null) log(`${each.id} is completed on the board, and no longer Done in Backlog.md: left completed`)
  }
  log(`updated ${updated} tasks Backlog.md changed`)

  // A Done task is completed only once what it needs is (a waiting task can't be claimed), so
  // Done tasks go in dependency order.
  const done = new Map(
    planned.filter((each) => each.result !== null && roots.get(each.id)?.task.state !== 'completed').map((each) => [each.id, each]),
  )
  const settled = new Set()
  const settle = async (each) => {
    if (settled.has(each.id)) return
    settled.add(each.id)
    for (const need of each.needs) if (done.has(need)) await settle(done.get(need))
    const claim = await team.tasks.claim(byTask.get(each.id), { lease: 60 })
    await claim.complete(each.result)
  }
  let count = 0
  for (const each of done.values()) {
    await settle(each)
    if (++count % 100 === 0) log(`completed ${count} of ${done.size}`)
  }
  log(`completed ${done.size} Done tasks`)
  return byTask
}

async function main() {
  const at = process.argv.indexOf('--channel')
  const channel = at > 0 ? process.argv[at + 1] : 'tasks'
  if (!process.env.BOARD_URL) throw new Error('set BOARD_URL to the board to migrate onto')
  const root = (await run('git', ['rev-parse', '--show-toplevel'])).stdout.trim()
  const tasks = await loadBacklog(root)
  console.log(`read ${tasks.length} tasks from Backlog.md`)
  const planned = plan(tasks)
  await migrate(board(), channel, planned, { log: console.log })
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((error) => {
    console.error(error)
    process.exit(1)
  })
}

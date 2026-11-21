import assert from 'node:assert/strict'
import { test } from 'node:test'
import { board } from '@vercel/board'
import { local } from '@vercel/board/backend-local'
import { TEXT_MAX, labelKey, migrate, plan } from './backlog-to-board.mjs'

// Backlog tasks as `backlog task view --json` has them, only the fields the migration reads.
const task = (fields) => ({
  status: 'To Do',
  type: null,
  priority: null,
  assignees: [],
  labels: [],
  milestone: null,
  parentTaskId: null,
  dependencies: [],
  subtasks: [],
  acceptanceCriteria: [],
  definitionOfDone: [],
  createdAt: '2026-11-01T10:00:00Z',
  updatedAt: '2026-11-02T10:00:00Z',
  ...fields,
})

const backlog = [
  task({ id: 'TASK-1', title: 'First', status: 'Done', priority: 'high', description: 'Do the first thing.',
    acceptanceCriteria: [{ index: 1, text: 'It is done', checked: true }], finalSummary: 'Did it.' }),
  task({ id: 'TASK-2', title: 'Second', description: 'After the first.', dependencies: ['TASK-1'] }),
  task({ id: 'TASK-2.1', title: 'A subtask', status: 'In Progress', type: 'bug', parentTaskId: 'TASK-2',
    labels: ['Review 2026-10-21', 'perf'], assignees: ['@claude'], milestone: 'm-3' }),
  task({ id: 'TASK-3', title: 'Long and done', status: 'Done', description: 'x'.repeat(TEXT_MAX),
    dependencies: ['TASK-2', 'TASK-1'], finalSummary: 'y'.repeat(2000) }),
  task({ id: 'TASK-4', title: 'Needs an archived one', dependencies: ['TASK-999'] }),
]

test('labelKey makes keys the board takes', () => {
  assert.equal(labelKey('Review 2026-10-21'), 'review-2026-10-21')
  assert.equal(labelKey('extent_store'), 'extent_store')
  assert.equal(labelKey('-x'), 'x')
})

test('the plan keeps each task whole, its ID as a label', () => {
  const [first, second, sub, long, archived] = plan(backlog)
  assert.equal(
    first.text,
    '# TASK-1: First\n\npriority high · created 2026-11-01 · updated 2026-11-02\n\n' +
      '## Description\n\nDo the first thing.\n\n## Acceptance Criteria\n\n- [x] #1 It is done\n\n## Final Summary\n\nDid it.',
  )
  assert.deepEqual(first.labels, { task: 'TASK-1', priority: 'high' })
  assert.equal(first.result, 'Did it.')
  assert.deepEqual(second.needs, ['TASK-1'])
  assert.equal(second.result, null)
  assert.deepEqual(sub.labels, {
    task: 'TASK-2.1', type: 'bug', parent: 'TASK-2', milestone: 'm-3', status: 'in-progress', assignee: 'claude',
    'tag.review-2026-10-21': 'yes', 'tag.perf': 'yes',
  })
  // A Done task waits only for Done ones; a long one is cut, and attached whole.
  assert.deepEqual(long.needs, ['TASK-1'])
  assert.equal(long.text.length, TEXT_MAX)
  assert.match(long.text, /_The whole task is attached as TASK-3\.md\._$/)
  assert.equal(long.attachment.name, 'TASK-3.md')
  assert.match(long.attachment.data, /## Final Summary\n\ny{2000}$/)
  assert.equal(long.result.length, 1000)
  assert.deepEqual(archived.needs, [])
  assert.match(archived.text, /needs TASK-999 · \(TASK-999 not on the board: archived in Backlog\.md\)/)
})

test('migrate makes every task and completes the Done ones; again, what Backlog.md changed since', async () => {
  const team = board({ backend: local({ memory: true }) })
  const byTask = await migrate(team, 'tasks', plan(backlog))
  assert.deepEqual([...byTask.keys()], ['TASK-1', 'TASK-2', 'TASK-2.1', 'TASK-3', 'TASK-4'])

  const state = async (id) => (await team.thread(byTask.get(id), { limit: 1 })).message
  const first = await state('TASK-1')
  assert.equal(first.labels['board.task'], 'completed')
  assert.equal(first.task.result, 'Did it.')
  assert.equal((await state('TASK-2')).labels['board.task'], 'ready')
  const sub = await state('TASK-2.1')
  assert.equal(sub.labels['board.task'], 'ready')
  assert.equal(sub.labels.status, 'in-progress')
  const long = await state('TASK-3')
  assert.equal(long.labels['board.task'], 'completed')
  assert.deepEqual(long.task.needs, [byTask.get('TASK-1')])
  assert.equal(long.attachments.length, 1)
  assert.equal(long.attachments[0].name, 'TASK-3.md')
  assert.equal((await state('TASK-4')).labels['board.task'], 'ready')

  // Since: a task filed, one done, one no longer In Progress and one waiting for another.
  const later = task({ id: 'TASK-5', title: 'Filed since', status: 'Done', dependencies: ['TASK-1'], finalSummary: 'Later.' })
  const since = backlog.map((each) => ({ ...each }))
  Object.assign(since[1], { status: 'Done', description: 'After the first, done.', finalSummary: 'Second done.' })
  Object.assign(since[2], { status: 'To Do', labels: ['perf'] })
  Object.assign(since[4], { dependencies: ['TASK-2.1'] })
  const again = await migrate(team, 'tasks', plan([...since, later]))
  assert.deepEqual([...again.keys()], ['TASK-1', 'TASK-2', 'TASK-2.1', 'TASK-3', 'TASK-4', 'TASK-5'])
  assert.deepEqual(again.get('TASK-1'), byTask.get('TASK-1'))
  const { tasks } = await team.tasks.list({ channel: 'tasks', state: 'all' })
  assert.equal(tasks.length, 6)
  const fifth = (await team.thread(again.get('TASK-5'), { limit: 1 })).message
  assert.equal(fifth.labels['board.task'], 'completed')
  assert.deepEqual(fifth.task.needs, [byTask.get('TASK-1')])

  const second = await state('TASK-2')
  assert.equal(second.labels['board.task'], 'completed')
  assert.equal(second.task.result, 'Second done.')
  assert.match(second.text, /## Description\n\nAfter the first, done\./)
  const subNow = await state('TASK-2.1')
  assert.deepEqual(subNow.labels, {
    task: 'TASK-2.1', type: 'bug', parent: 'TASK-2', milestone: 'm-3', assignee: 'claude', 'tag.perf': 'yes',
    'board.task': 'ready',
  })
  const fourth = await state('TASK-4')
  assert.equal(fourth.labels['board.task'], 'waiting')
  assert.deepEqual(fourth.task.needs, [byTask.get('TASK-2.1')])

  // Once more, nothing changed: nothing to do.
  const logged = []
  await migrate(team, 'tasks', plan([...since, later]), { log: (line) => logged.push(line) })
  assert.deepEqual(logged, ['6 tasks on the board, 0 to make', 'updated 0 tasks Backlog.md changed', 'completed 0 Done tasks'])
})

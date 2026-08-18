import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react'
import ReactDOM from 'react-dom/client'
import {
  Check,
  ChevronDown,
  ChevronRight,
  Download,
  FilePenLine,
  Gauge,
  ListTodo,
  Plus,
  RotateCcw,
  Search,
  Settings,
  Square,
  Trash2,
  X,
} from 'lucide-react'
import './styles.css'

type CaseStatus =
  | 'QUEUED'
  | 'COMP'
  | 'RUN'
  | 'PASS'
  | 'WA'
  | 'CE'
  | 'RE'
  | 'TLE'
  | 'ERROR'
  | 'CANCELLED'
type TaskStatus = 'QUEUED' | 'RUNNING' | 'COMPLETE' | 'CANCELLED' | 'ERROR'
type PerformanceMode = 'relative' | 'absolute'
type Panel = 'board' | 'queue' | 'new' | 'cases' | 'settings' | null
type DetailTab = 'details' | 'output'
type ContentKind =
  | 'input'
  | 'compile_stdout'
  | 'compile_stderr'
  | 'stdout'
  | 'stderr'
  | 'expected_output'
  | 'error'

type TestCase = {
  id: string
  suite: string
  name: string
  has_input: boolean
  has_expected: boolean
}

type EditableTestCase = {
  id: string
  suite: 'functional' | 'h_functional' | 'perf'
  name: string
  source: string
  input: string | null
  expected: string | null
}

type Baseline = {
  id: string
  task_id: string
  status: CaseStatus
  median_ms: number | null
  task_number: number
  git_hash: string
  dirty: boolean
  compiler: 'SOYO' | 'CLANG'
}

type TaskCaseSummary = {
  id: string
  task_id: string
  position: number
  case_id: string
  suite: string
  name: string
  status: CaseStatus
  started_at: number | null
  run_started_at: number | null
  finished_at: number | null
  median_ms: number | null
  min_ms: number | null
  max_ms: number | null
  returncode: number | null
  baseline: Baseline | null
}

type TaskCase = TaskCaseSummary & {
  source_path: string
  samples: number[]
  warmup_samples: number[]
  compile_command: string | null
  run_command: string | null
  remote_path: string | null
  content_sizes: Record<ContentKind, number | null>
  artifacts: Array<'assembly' | 'elf' | 'raana'>
}

type Task = {
  id: string
  number: number
  git_hash: string
  dirty: boolean
  compiler: 'SOYO' | 'CLANG'
  opt_level: number
  warmups: number
  repeats: number
  timeout_seconds: number
  board_host: string
  board_port: number
  cpu: number
  status: TaskStatus
  created_at: number
  started_at: number | null
  finished_at: number | null
  cases: TaskCaseSummary[]
  completed_cases: number
  total_cases: number
  status_counts: Record<string, number>
}

type Board = {
  host: string
  port: number
  cpu: number
  service: string
  online?: boolean
  architecture?: string
  cpus?: string
  frequency_khz?: number
  temperature_millidegrees?: number
  latency_ms?: number
}

type StreamEvent = {
  seq: number
  type: string
  taskId: string | null
  caseId: string | null
  payload: Record<string, unknown>
  at?: number
}

type Selection = { type: 'task'; taskId: string } | { type: 'case'; taskId: string; caseId: string }

type TaskDraft = {
  compiler: 'SOYO' | 'CLANG'
  optLevel: number
  warmups: number
  repeats: number
  timeout: number
  suite: string
  query: string
  selected: string[]
}

const taskDraftKey = 'arm-bench-task-draft'

const loadTaskDraft = (): TaskDraft => {
  const defaults: TaskDraft = {
    compiler: 'SOYO',
    optLevel: 2,
    warmups: 0,
    repeats: 1,
    timeout: 180,
    suite: 'all',
    query: '',
    selected: [],
  }
  try {
    const saved = JSON.parse(localStorage.getItem(taskDraftKey) || '{}') as Partial<TaskDraft>
    return {
      ...defaults,
      ...saved,
      compiler: saved.compiler === 'CLANG' ? 'CLANG' : 'SOYO',
      selected: Array.isArray(saved.selected) ? saved.selected : [],
    }
  } catch {
    return defaults
  }
}

const progressStatusOrder = ['PASS', 'WA', 'CE', 'RE', 'TLE', 'ERROR', 'CANCELLED']
const retryableCaseStatuses = new Set<CaseStatus>(['WA', 'CE', 'RE', 'TLE', 'ERROR', 'CANCELLED'])
const statusColors: Record<string, string> = {
  PASS: '#4b9a63',
  WA: '#d15d5d',
  CE: '#8b68b3',
  RE: '#9d3e3e',
  TLE: '#d58b3f',
  ERROR: '#d2ad3f',
  CANCELLED: '#303431',
  RUNNING: '#5e8ea5',
  COMP: '#7c719c',
  RUN: '#5e8ea5',
  QUEUED: '#d2d7d1',
}
const statusLabels: Record<TaskStatus | CaseStatus, string> = {
  QUEUED: 'QUEUE',
  RUNNING: 'RUN',
  COMP: 'COMP',
  RUN: 'RUN',
  COMPLETE: 'DONE',
  PASS: 'PASS',
  WA: 'WA',
  CE: 'CE',
  RE: 'RE',
  TLE: 'TLE',
  ERROR: 'ERROR',
  CANCELLED: 'ABORT',
}

const api = async <T,>(path: string, init?: RequestInit): Promise<T> => {
  const response = await fetch(path, {
    headers: { 'Content-Type': 'application/json', ...init?.headers },
    ...init,
  })
  if (!response.ok) {
    const text = await response.text()
    let message = text || response.statusText
    try {
      const body = JSON.parse(text)
      message = body.detail || message
    } catch {}
    throw new Error(message)
  }
  if (response.status === 204) return undefined as T
  return response.json()
}

const caseApiPath = (id: string) =>
  `/api/cases/${id
    .split('/')
    .map((part) => encodeURIComponent(part))
    .join('/')}`

const timeLabel = (seconds: number) =>
  new Date(seconds * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })

const revisionLabel = (task: Task) => `${task.git_hash}${task.dirty ? '*' : ''}`

const taskLabel = (task: Task) =>
  `#${task.number} ${timeLabel(task.created_at)} ${revisionLabel(task)} ${task.compiler}`

const formatTime = (time: number | null) => (time ? new Date(time * 1000).toLocaleString() : '—')

const formatDuration = (milliseconds: number | null) => {
  if (milliseconds == null) return '—'
  if (milliseconds >= 1000) return `${(milliseconds / 1000).toFixed(3)} s`
  return `${milliseconds.toFixed(2)} ms`
}

const formatSignedDuration = (milliseconds: number) => {
  const sign = milliseconds > 0 ? '+' : milliseconds < 0 ? '-' : ''
  const absolute = Math.abs(milliseconds)
  return absolute >= 1000
    ? `${sign}${(absolute / 1000).toPrecision(3)} s`
    : `${sign}${absolute.toPrecision(3)} ms`
}

const formatTaskDuration = (task: Task, now: number) => {
  if (!task.started_at) return '—'
  const end = task.finished_at || now
  return `${(end - task.started_at).toFixed(2)} s`
}

const formatActiveCaseDuration = (item: TaskCaseSummary, now: number) => {
  const startedAt = item.status === 'RUN' ? item.run_started_at : item.started_at
  return startedAt == null ? '—' : `${Math.max(0, now - startedAt).toFixed(2)} s`
}

const caseDuration = (item: TaskCaseSummary, now: number) =>
  item.status === 'COMP' || item.status === 'RUN'
    ? formatActiveCaseDuration(item, now)
    : formatDuration(item.median_ms)

function PerformanceDelta({
  current,
  baseline,
  mode,
}: {
  current: number | null
  baseline: number | null | undefined
  mode: PerformanceMode
}) {
  const delta = current != null && baseline != null ? current - baseline : null
  const percent = delta != null && baseline ? (delta / baseline) * 100 : null
  return (
    <small
      className={
        delta == null || percent == null ? 'placeholder' : percent > 0 ? 'slower' : 'faster'
      }
    >
      {delta != null && percent != null
        ? mode === 'relative'
          ? `${percent > 0 ? '+' : ''}${percent.toFixed(1)}%`
          : formatSignedDuration(delta)
        : '—'}
    </small>
  )
}

function Status({ value }: { value: TaskStatus | CaseStatus }) {
  return <span className={`status status-${value.toLowerCase()}`}>{statusLabels[value]}</span>
}

function Progress({ task }: { task: Task }) {
  let completed = 0
  const segments = progressStatusOrder.flatMap((status) => {
    const count = task.status_counts[status] || 0
    if (!count) return []
    const start = (completed / task.total_cases) * 100
    completed += count
    const end = (completed / task.total_cases) * 100
    return [`${statusColors[status]} ${start}%`, `${statusColors[status]} ${end}%`]
  })
  const background = segments.length
    ? `linear-gradient(to right, ${segments.join(', ')}, transparent ${(completed / task.total_cases) * 100}%, transparent 100%)`
    : undefined
  return <span className='task-progress' style={{ background }} />
}

function IconAction({
  label,
  disabled,
  onClick,
  children,
}: {
  label: string
  disabled?: boolean
  onClick?: () => void
  children: ReactNode
}) {
  return (
    <button
      className='row-action'
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={(event) => {
        event.stopPropagation()
        onClick?.()
      }}
    >
      {children}
    </button>
  )
}

function TaskActions({
  task,
  onCancel,
  onRetry,
  onBaseline,
  onDelete,
}: {
  task: Task
  onCancel: () => void
  onRetry: () => void
  onBaseline: () => void
  onDelete: () => void
}) {
  const retryCount = task.cases.filter((item) => retryableCaseStatuses.has(item.status)).length
  return (
    <span className='task-actions'>
      <IconAction
        label='停止任务'
        disabled={!['RUNNING', 'QUEUED'].includes(task.status)}
        onClick={onCancel}
      >
        <Square size={11} />
      </IconAction>
      <IconAction
        label={`原地重试 ${retryCount} 个失败用例`}
        disabled={['RUNNING', 'QUEUED'].includes(task.status) || retryCount === 0}
        onClick={onRetry}
      >
        <RotateCcw size={12} />
      </IconAction>
      <IconAction
        label='将 PASS 结果设为基准'
        disabled={!task.cases.some((item) => item.status === 'PASS')}
        onClick={onBaseline}
      >
        <Gauge size={12} />
      </IconAction>
      <IconAction
        label='删除任务'
        disabled={!['COMPLETE', 'CANCELLED', 'ERROR'].includes(task.status)}
        onClick={onDelete}
      >
        <Trash2 size={12} />
      </IconAction>
    </span>
  )
}

function CaseActions({
  task,
  item,
  onCancel,
  onRetry,
  onBaseline,
}: {
  task: Task
  item: TaskCaseSummary
  onCancel: () => void
  onRetry: () => void
  onBaseline: () => void
}) {
  return (
    <span className='case-actions'>
      <IconAction
        label='停止用例'
        disabled={!['COMP', 'RUN', 'QUEUED'].includes(item.status) || task.status === 'CANCELLED'}
        onClick={onCancel}
      >
        <Square size={10} />
      </IconAction>
      <IconAction
        label='原地重试用例'
        disabled={['RUNNING', 'QUEUED'].includes(task.status)}
        onClick={onRetry}
      >
        <RotateCcw size={11} />
      </IconAction>
      <IconAction label='设为基准' disabled={item.status !== 'PASS'} onClick={onBaseline}>
        <Gauge size={11} />
      </IconAction>
    </span>
  )
}

function BoardPanel({
  board,
  onClose,
  onSaved,
}: {
  board: Board
  onClose: () => void
  onSaved: (board: Board) => void
}) {
  const [form, setForm] = useState<Board>(board)
  const [busy, setBusy] = useState<'save' | 'test' | null>(null)
  const [feedback, setFeedback] = useState<{ kind: 'success' | 'error'; text: string } | null>(null)

  const save = async () => {
    setBusy('save')
    setFeedback(null)
    try {
      const saved = await api<Board>('/api/board', {
        method: 'PUT',
        body: JSON.stringify(form),
      })
      onSaved(saved)
      setFeedback({ kind: 'success', text: '配置已保存' })
    } catch (reason) {
      setFeedback({ kind: 'error', text: (reason as Error).message })
    } finally {
      setBusy(null)
    }
  }

  const test = async () => {
    setBusy('test')
    setFeedback(null)
    try {
      await api('/api/board', { method: 'PUT', body: JSON.stringify(form) })
      const status = await api<Board>('/api/board/test', { method: 'POST' })
      onSaved(status)
      setFeedback({ kind: 'success', text: `连接正常 · ${status.latency_ms ?? '—'} ms` })
    } catch (reason) {
      setFeedback({ kind: 'error', text: (reason as Error).message })
    } finally {
      setBusy(null)
    }
  }

  return (
    <div className='popover board-panel'>
      <div className='popover-heading'>
        <strong>目标板配置</strong>
        <button className='close-button' onClick={onClose} aria-label='关闭'>
          <X size={14} />
        </button>
      </div>
      <div className='form-grid'>
        <label>
          <span>IP / 主机</span>
          <input
            value={form.host}
            onChange={(event) => setForm({ ...form, host: event.target.value })}
          />
        </label>
        <label>
          <span>服务端口</span>
          <input
            type='number'
            value={form.port}
            onChange={(event) => setForm({ ...form, port: +event.target.value })}
          />
        </label>
        <label>
          <span>目标 CPU</span>
          <input
            type='number'
            min='0'
            value={form.cpu}
            onChange={(event) => setForm({ ...form, cpu: +event.target.value })}
          />
        </label>
        <label>
          <span>远程入口</span>
          <input value={form.service} readOnly />
        </label>
      </div>
      <dl className='board-readout'>
        <div>
          <dt>连接</dt>
          <dd>{board.online ? 'ONLINE' : 'NOT TESTED'}</dd>
        </div>
        <div>
          <dt>架构</dt>
          <dd>{board.architecture || '—'}</dd>
        </div>
        <div>
          <dt>在线 CPU</dt>
          <dd>{board.cpus || '—'}</dd>
        </div>
        <div>
          <dt>频率</dt>
          <dd>
            {board.frequency_khz ? `${(board.frequency_khz / 1_000_000).toFixed(2)} GHz` : '—'}
          </dd>
        </div>
        <div>
          <dt>温度</dt>
          <dd>
            {board.temperature_millidegrees
              ? `${Math.round(board.temperature_millidegrees / 1000)}°C`
              : '—'}
          </dd>
        </div>
        <div>
          <dt>延迟</dt>
          <dd>{board.latency_ms != null ? `${board.latency_ms} ms` : '—'}</dd>
        </div>
      </dl>
      {feedback && <div className={`panel-feedback ${feedback.kind}`}>{feedback.text}</div>}
      <div className='popover-footer'>
        <button onClick={test} disabled={busy !== null}>
          {busy === 'test' ? '测试中…' : '测试连接'}
        </button>
        <button className='confirm-button' onClick={save} disabled={busy !== null}>
          <Check size={13} /> {busy === 'save' ? '保存中…' : '保存'}
        </button>
      </div>
    </div>
  )
}

function QueuePanel({
  tasks,
  now,
  onCancel,
}: {
  tasks: Task[]
  now: number
  onCancel: (id: string) => void
}) {
  return (
    <div className='popover queue-panel'>
      <div className='popover-heading'>
        <strong>评测队列</strong>
        <span>{tasks.length} 个任务</span>
      </div>
      {tasks.length === 0 ? (
        <div className='panel-empty'>队列为空</div>
      ) : (
        tasks.map((task, index) => {
          const active = task.cases.find((item) => ['COMP', 'RUN'].includes(item.status))
          return (
            <div
              className={`queue-row ${task.status === 'RUNNING' ? 'running-wave' : ''}`}
              key={task.id}
            >
              <div>
                <b>#{task.number}</b>
                <span>
                  {task.status === 'RUNNING'
                    ? active
                      ? `${statusLabels[active.status]} · ${active.case_id} · ${formatActiveCaseDuration(active, now)}`
                      : '准备工具链'
                    : `等待中 · 前方 ${index} 个任务`}
                </span>
              </div>
              <Status value={task.status} />
              <span className='queue-progress'>
                {task.completed_cases}/{task.total_cases}
              </span>
              <IconAction label='终止任务' onClick={() => onCancel(task.id)}>
                <Square size={11} />
              </IconAction>
            </div>
          )
        })
      )}
    </div>
  )
}

function CaseEditorPanel({
  cases,
  onClose,
  onChanged,
}: {
  cases: TestCase[]
  onClose: () => void
  onChanged: () => Promise<void>
}) {
  const blank: EditableTestCase = {
    id: '',
    suite: 'functional',
    name: '',
    source: '',
    input: null,
    expected: null,
  }
  const [selectedId, setSelectedId] = useState<string | null>(cases[0]?.id || null)
  const [draft, setDraft] = useState<EditableTestCase>(blank)
  const [query, setQuery] = useState('')
  const [loading, setLoading] = useState(false)
  const [busy, setBusy] = useState(false)
  const [feedback, setFeedback] = useState<{ kind: 'success' | 'error'; text: string } | null>(null)
  const visible = cases.filter((item) => item.id.toLowerCase().includes(query.toLowerCase()))

  useEffect(() => {
    if (!selectedId) return
    let cancelled = false
    setLoading(true)
    setFeedback(null)
    api<EditableTestCase>(caseApiPath(selectedId))
      .then((record) => {
        if (!cancelled) setDraft(record)
      })
      .catch((reason: Error) => {
        if (!cancelled) setFeedback({ kind: 'error', text: reason.message })
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [selectedId])

  const startNew = () => {
    setSelectedId(null)
    setDraft(blank)
    setFeedback(null)
  }

  const save = async () => {
    setBusy(true)
    setFeedback(null)
    try {
      const record = await api<EditableTestCase>(
        selectedId ? caseApiPath(selectedId) : '/api/cases',
        {
          method: selectedId ? 'PUT' : 'POST',
          body: JSON.stringify(draft),
        },
      )
      await onChanged()
      setSelectedId(record.id)
      setDraft(record)
      setFeedback({ kind: 'success', text: '测试用例已保存' })
    } catch (reason) {
      setFeedback({ kind: 'error', text: (reason as Error).message })
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    if (!selectedId || !window.confirm(`删除 ${selectedId} 及对应的输入和输出文件？`)) return
    setBusy(true)
    setFeedback(null)
    try {
      await api(caseApiPath(selectedId), { method: 'DELETE' })
      const next = cases.find((item) => item.id !== selectedId)?.id || null
      await onChanged()
      setSelectedId(next)
      if (!next) setDraft(blank)
      setFeedback({ kind: 'success', text: '测试用例已删除' })
    } catch (reason) {
      setFeedback({ kind: 'error', text: (reason as Error).message })
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className='popover case-editor-panel'>
      <div className='popover-heading'>
        <strong>测试用例</strong>
        <button className='close-button' onClick={onClose} aria-label='关闭'>
          <X size={14} />
        </button>
      </div>
      <div className='case-editor-layout'>
        <aside className='case-editor-sidebar'>
          <div className='case-editor-tools'>
            <span className='search-box'>
              <Search size={13} />
              <input
                placeholder='搜索用例'
                value={query}
                onChange={(event) => setQuery(event.target.value)}
              />
            </span>
            <button onClick={startNew} title='新建测试用例'>
              <Plus size={13} /> 新建
            </button>
          </div>
          <div className='case-editor-list'>
            {visible.map((item) => (
              <button
                className={selectedId === item.id ? 'selected' : ''}
                onClick={() => setSelectedId(item.id)}
                key={item.id}
              >
                <code>{item.id}</code>
                <span>{item.has_input ? 'IN' : ''}</span>
                <span>{item.has_expected ? 'OUT' : ''}</span>
              </button>
            ))}
          </div>
        </aside>
        <section className='case-editor-form'>
          <div className='case-identity'>
            <label>
              <span>分组</span>
              <select
                value={draft.suite}
                onChange={(event) =>
                  setDraft({
                    ...draft,
                    suite: event.target.value as EditableTestCase['suite'],
                  })
                }
              >
                <option value='functional'>functional</option>
                <option value='h_functional'>h_functional</option>
                <option value='perf'>perf</option>
              </select>
            </label>
            <label>
              <span>文件名</span>
              <input
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                placeholder='example'
              />
            </label>
          </div>
          <label className='case-source-editor'>
            <span>SysY 源码</span>
            <textarea
              value={draft.source}
              onChange={(event) => setDraft({ ...draft, source: event.target.value })}
              spellCheck={false}
            />
          </label>
          <div className='case-sidecars'>
            {(
              [
                ['input', '输入 .in'],
                ['expected', '预期输出 .out'],
              ] as const
            ).map(([field, label]) => (
              <label key={field}>
                <span>
                  <input
                    type='checkbox'
                    checked={draft[field] !== null}
                    onChange={(event) =>
                      setDraft({ ...draft, [field]: event.target.checked ? '' : null })
                    }
                  />
                  {label}
                </span>
                <textarea
                  value={draft[field] || ''}
                  disabled={draft[field] === null}
                  onChange={(event) => setDraft({ ...draft, [field]: event.target.value })}
                  spellCheck={false}
                />
              </label>
            ))}
          </div>
          {loading && <div className='case-editor-loading'>读取中…</div>}
        </section>
      </div>
      <div className='popover-footer'>
        {feedback && <span className={`inline-feedback ${feedback.kind}`}>{feedback.text}</span>}
        <button className='danger-button' onClick={remove} disabled={!selectedId || busy}>
          <Trash2 size={13} /> 删除
        </button>
        <button
          className='confirm-button'
          onClick={save}
          disabled={busy || loading || !draft.name.trim()}
        >
          <Check size={13} /> {busy ? '保存中…' : '保存'}
        </button>
      </div>
    </div>
  )
}

function NewTaskPanel({
  cases,
  onClose,
  onCreated,
  onError,
}: {
  cases: TestCase[]
  onClose: () => void
  onCreated: (task: Task) => void
  onError: (message: string) => void
}) {
  const savedDraft = useMemo(loadTaskDraft, [])
  const [compiler, setCompiler] = useState<'SOYO' | 'CLANG'>(savedDraft.compiler)
  const [optLevel, setOptLevel] = useState(savedDraft.optLevel)
  const [warmups, setWarmups] = useState(savedDraft.warmups)
  const [repeats, setRepeats] = useState(savedDraft.repeats)
  const [timeout, setTimeoutSeconds] = useState(savedDraft.timeout)
  const [suite, setSuite] = useState(savedDraft.suite)
  const [query, setQuery] = useState(savedDraft.query)
  const [selected, setSelected] = useState<Set<string>>(new Set(savedDraft.selected))
  const [busy, setBusy] = useState(false)
  const suites = ['all', ...Array.from(new Set(cases.map((item) => item.suite)))]
  const visible = cases.filter(
    (item) =>
      (suite === 'all' || item.suite === suite) &&
      item.id.toLowerCase().includes(query.toLowerCase()),
  )

  useEffect(() => {
    const valid = new Set(cases.map((item) => item.id))
    setSelected((old) => {
      const next = new Set([...old].filter((id) => valid.has(id)))
      return next.size === old.size ? old : next
    })
  }, [cases])

  useEffect(() => {
    localStorage.setItem(
      taskDraftKey,
      JSON.stringify({
        compiler,
        optLevel,
        warmups,
        repeats,
        timeout,
        suite,
        query,
        selected: [...selected],
      } satisfies TaskDraft),
    )
  }, [compiler, optLevel, query, repeats, selected, suite, timeout, warmups])

  const transformVisible = (mode: 'all' | 'clear' | 'invert') => {
    setSelected((old) => {
      const next = new Set(old)
      for (const item of visible) {
        if (mode === 'all') next.add(item.id)
        if (mode === 'clear') next.delete(item.id)
        if (mode === 'invert') {
          if (next.has(item.id)) next.delete(item.id)
          else next.add(item.id)
        }
      }
      return next
    })
  }

  const submit = async () => {
    setBusy(true)
    try {
      const task = await api<Task>('/api/tasks', {
        method: 'POST',
        body: JSON.stringify({
          compiler,
          opt_level: optLevel,
          cases: [...selected],
          warmups,
          repeats,
          timeout_seconds: timeout,
        }),
      })
      onCreated(task)
    } catch (reason) {
      onError((reason as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className='popover new-task-panel'>
      <div className='popover-heading'>
        <strong>新建评测任务</strong>
        <button className='close-button' onClick={onClose} aria-label='关闭'>
          <X size={14} />
        </button>
      </div>
      <div className='task-config'>
        <div className='segmented' aria-label='编译器'>
          {(['SOYO', 'CLANG'] as const).map((value) => (
            <button
              className={compiler === value ? 'active' : ''}
              onClick={() => setCompiler(value)}
              key={value}
            >
              {value}
            </button>
          ))}
        </div>
        <label>
          <span>优化</span>
          <select value={optLevel} onChange={(event) => setOptLevel(+event.target.value)}>
            <option value='0'>-O0</option>
            <option value='1'>-O1</option>
            <option value='2'>-O2</option>
            <option value='3'>-O3</option>
          </select>
        </label>
        <label>
          <span>预热</span>
          <input
            type='number'
            min='0'
            value={warmups}
            onChange={(event) => setWarmups(+event.target.value)}
          />
        </label>
        <label>
          <span>重复</span>
          <input
            type='number'
            min='1'
            value={repeats}
            onChange={(event) => setRepeats(+event.target.value)}
          />
        </label>
        <label>
          <span>超时</span>
          <input
            type='number'
            min='1'
            value={timeout}
            onChange={(event) => setTimeoutSeconds(+event.target.value)}
          />
          <em>s</em>
        </label>
      </div>
      <div className='case-filter'>
        <span className='search-box'>
          <Search size={13} />
          <input
            placeholder='搜索用例'
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
        </span>
        <div className='suite-tabs'>
          {suites.map((name) => (
            <button
              className={suite === name ? 'active' : ''}
              onClick={() => setSuite(name)}
              key={name}
            >
              {name === 'all' ? '全部' : name}
              <span>{cases.filter((item) => name === 'all' || item.suite === name).length}</span>
            </button>
          ))}
        </div>
        <div className='bulk-actions'>
          <button onClick={() => transformVisible('all')}>全选当前</button>
          <button onClick={() => transformVisible('clear')}>清空当前</button>
          <button onClick={() => transformVisible('invert')}>反选当前</button>
          <span>
            {visible.filter((item) => selected.has(item.id)).length}/{visible.length} 当前已选
          </span>
        </div>
      </div>
      <div className='case-picker'>
        {visible.map((item) => (
          <label key={item.id}>
            <input
              type='checkbox'
              checked={selected.has(item.id)}
              onChange={() =>
                setSelected((old) => {
                  const next = new Set(old)
                  if (next.has(item.id)) next.delete(item.id)
                  else next.add(item.id)
                  return next
                })
              }
            />
            <code>{item.id}</code>
            <span>{item.has_input ? 'IN' : ''}</span>
            <span>{item.has_expected ? 'OUT' : ''}</span>
          </label>
        ))}
      </div>
      <div className='popover-footer'>
        <span>
          <b>{selected.size}</b> 个用例
        </span>
        <button className='confirm-button' disabled={selected.size === 0 || busy} onClick={submit}>
          <Plus size={13} /> {busy ? '提交中…' : '提交任务'}
        </button>
      </div>
    </div>
  )
}

function TaskPerformance({ task, mode }: { task: Task; mode: PerformanceMode }) {
  const results = useMemo(
    () =>
      task.cases
        .filter((item) => item.median_ms != null)
        .sort((a, b) => (b.median_ms || 0) - (a.median_ms || 0)),
    [task.cases],
  )
  const maximum = Math.max(
    ...results.flatMap((item) => [item.median_ms || 0, item.baseline?.median_ms || 0]),
    1,
  )
  if (!results.length) return <div className='section-empty'>任务尚无有效性能样本</div>
  return (
    <div className='performance-list'>
      {results.map((item) => {
        const baseline = item.baseline?.median_ms
        const current = item.median_ms || 0
        return (
          <div className='performance-group' key={item.id}>
            <code>{item.name}</code>
            <i className='performance-track'>
              <b style={{ width: `${(current / maximum) * 100}%` }} />
              {baseline != null && <span style={{ width: `${(baseline / maximum) * 100}%` }} />}
            </i>
            <em className='performance-value'>
              <span>{formatDuration(item.median_ms)}</span>
              <PerformanceDelta current={item.median_ms} baseline={baseline} mode={mode} />
            </em>
          </div>
        )
      })}
    </div>
  )
}

function TaskDetails({ task, mode, now }: { task: Task; mode: PerformanceMode; now: number }) {
  return (
    <div className='detail-content'>
      <section className='detail-overview-card'>
        <div>
          <span>进度</span>
          <strong>
            {task.completed_cases} / {task.total_cases}
          </strong>
        </div>
        <div>
          <span>总耗时</span>
          <strong>{formatTaskDuration(task, now)}</strong>
        </div>
        <div>
          <span>编译配置</span>
          <strong>
            {task.compiler} -O{task.opt_level}
          </strong>
        </div>
      </section>
      <dl className='detail-metadata'>
        <div>
          <dt>任务</dt>
          <dd>
            #{task.number} · Git {revisionLabel(task)}
          </dd>
        </div>
        <div>
          <dt>目标板</dt>
          <dd>
            {task.board_host}:{task.board_port} · CPU {task.cpu}
          </dd>
        </div>
        <div>
          <dt>采样</dt>
          <dd>
            预热 {task.warmups} · 正式 {task.repeats} · 超时 {task.timeout_seconds} s
          </dd>
        </div>
        <div>
          <dt>时间</dt>
          <dd>
            创建 {formatTime(task.created_at)} · 开始 {formatTime(task.started_at)} · 结束{' '}
            {formatTime(task.finished_at)}
          </dd>
        </div>
      </dl>
      <section className='detail-section'>
        <h2>状态分布</h2>
        <div className='status-summary'>
          {Object.entries(task.status_counts).map(([status, count]) => (
            <span key={status}>
              <i style={{ background: statusColors[status] || '#b8bdb6' }} />
              {statusLabels[status as TaskStatus | CaseStatus] || status} <b>{count}</b>
            </span>
          ))}
        </div>
        <div className='case-status-grid' aria-label='按用例顺序排列的状态'>
          {task.cases.map((item, index) => (
            <span
              key={item.id}
              style={{ background: statusColors[item.status] }}
              title={`${index + 1}. ${item.case_id} · ${statusLabels[item.status]}`}
            />
          ))}
        </div>
      </section>
      <section className='detail-section'>
        <h2>性能</h2>
        <TaskPerformance task={task} mode={mode} />
      </section>
    </div>
  )
}

function CaseDetails({
  item,
  task,
  tasks,
  now,
}: {
  item: TaskCase
  task: Task
  tasks: Task[]
  now: number
}) {
  const history = useMemo(
    () =>
      tasks
        .flatMap((candidate) =>
          candidate.cases
            .filter((result) => result.case_id === item.case_id)
            .map((result) => ({ result, task: candidate })),
        )
        .sort((a, b) => b.task.created_at - a.task.created_at),
    [item.case_id, tasks],
  )
  const maximum = Math.max(...history.map(({ result }) => result.median_ms || 0), 1)
  return (
    <div className='detail-content'>
      <section className='detail-overview-card detail-overview-card-case'>
        <div>
          <span>
            {item.status === 'COMP' ? '编译计时' : item.status === 'RUN' ? '运行计时' : '本次时间'}
          </span>
          <strong>{caseDuration(item, now)}</strong>
        </div>
        <div>
          <span>运行范围</span>
          <strong>
            {formatDuration(item.min_ms)} – {formatDuration(item.max_ms)}
          </strong>
        </div>
      </section>
      <dl className='detail-metadata'>
        <div>
          <dt>来源</dt>
          <dd>
            任务 #{task.number} · {task.compiler} · {item.suite}
          </dd>
        </div>
        <div>
          <dt>时间</dt>
          <dd>
            开始 {formatTime(item.started_at)} · 结束 {formatTime(item.finished_at)}
          </dd>
        </div>
        <div>
          <dt>编译</dt>
          <dd>
            <code>{item.compile_command || '—'}</code>
          </dd>
        </div>
        <div>
          <dt>执行</dt>
          <dd>
            <code>{item.run_command || '—'}</code>
          </dd>
        </div>
        <div>
          <dt>远程路径</dt>
          <dd>
            <code>{item.remote_path || '—'}</code>
          </dd>
        </div>
        <div>
          <dt>采样</dt>
          <dd>
            预热 {item.warmup_samples.map((sample) => `${sample.toFixed(2)} ms`).join(' · ') || '—'}
            {' · '}正式 {item.samples.map((sample) => `${sample.toFixed(2)} ms`).join(' · ') || '—'}
          </dd>
        </div>
      </dl>
      <section className='detail-section'>
        <h2>历史性能</h2>
        <div className='history-list'>
          {history.map(({ result, task: historyTask }) => (
            <div
              className={`history-row ${result.id === item.id ? 'current' : ''} ${result.baseline?.id === result.id ? 'baseline' : ''}`}
              key={result.id}
            >
              <code>
                #{historyTask.number} {revisionLabel(historyTask)} · {historyTask.compiler}
              </code>
              <i>
                <b style={{ width: `${((result.median_ms || 0) / maximum) * 100}%` }} />
              </i>
              <em>{formatDuration(result.median_ms)}</em>
              <Status value={result.status} />
            </div>
          ))}
        </div>
      </section>
    </div>
  )
}

const byteSize = (bytes: number) => {
  if (bytes < 1024) return `${bytes} B`
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${(bytes / 1024).toFixed(1)} KB`
}

function FileAction({ label, size, url }: { label?: string; size: number; url: string }) {
  return (
    <span className='file-action'>
      <span>
        {label && `${label} `}
        {byteSize(size)}
      </span>
      <a className='download-button' href={`${url}?download=true`}>
        <Download size={11} /> 下载
      </a>
    </span>
  )
}

function TextArtifact({
  item,
  task,
  kind,
  available,
}: {
  item: TaskCase
  task: Task
  kind: 'assembly' | 'raana'
  available: boolean
}) {
  const [content, setContent] = useState<string | null>(null)
  const url = `/api/tasks/${task.id}/cases/${item.id}/artifacts/${kind}`
  useEffect(() => {
    if (!available) {
      setContent('')
      return
    }
    setContent(null)
    fetch(url).then(async (response) => {
      const value = await response.text()
      setContent(response.headers.get('X-Content-Truncated') ? `${value}\n…` : value)
    })
  }, [url, available])
  return (
    <div className={`artifact-view artifact-view-${kind}`}>
      <div className='artifact-heading'>
        <h3>{kind === 'assembly' ? '汇编' : 'Raana IR'}</h3>
        {available && (
          <a className='download-button artifact-download' href={`${url}?download=true`}>
            <Download size={11} /> 下载
          </a>
        )}
      </div>
      <pre className={content === '' ? 'empty-output' : ''}>
        {content === '' ? '（无输出）' : (content ?? '加载中…')}
      </pre>
    </div>
  )
}

function useCaseContents(item: TaskCase, task: Task) {
  const [contents, setContents] = useState<Partial<Record<ContentKind, string>>>({})
  useEffect(() => {
    let cancelled = false
    const kinds = (
      [
        'input',
        'compile_stdout',
        'compile_stderr',
        'stdout',
        'stderr',
        'expected_output',
        'error',
      ] as ContentKind[]
    ).filter((kind) => item.content_sizes[kind] != null && item.content_sizes[kind] !== 0)
    Promise.all(
      kinds.map(async (kind) => {
        const response = await fetch(`/api/tasks/${task.id}/cases/${item.id}/content/${kind}`)
        const value = await response.text()
        return [kind, response.headers.get('X-Content-Truncated') ? `${value}\n…` : value] as const
      }),
    ).then((entries) => {
      if (!cancelled) setContents(Object.fromEntries(entries))
    })
    return () => {
      cancelled = true
    }
  }, [item, task.id])
  return contents
}

function CaseOutput({ item, task }: { item: TaskCase; task: Task }) {
  const contents = useCaseContents(item, task)
  const hasContent = (kind: ContentKind) =>
    item.content_sizes[kind] != null && item.content_sizes[kind] !== 0
  const content = (kind: ContentKind) => {
    if (item.content_sizes[kind] == null) return null
    if (item.content_sizes[kind] === 0) return ''
    return contents[kind] ?? '加载中…'
  }
  const contentUrl = (kind: ContentKind) => `/api/tasks/${task.id}/cases/${item.id}/content/${kind}`
  const stdout = content('stdout')
  const stdoutWithReturncode =
    stdout === '加载中…' || item.returncode == null
      ? stdout
      : `${stdout || ''}${stdout && !stdout.endsWith('\n') ? '\n' : ''}${item.returncode}\n`
  const hasProgramOutput = hasContent('stdout') || item.returncode != null
  return (
    <div className='output-content'>
      <section className='output-section'>
        <div className='program-output-headings'>
          <div className='output-column-heading'>
            <h2>标准输出</h2>
            {hasContent('expected_output') && (
              <FileAction
                size={item.content_sizes.expected_output!}
                url={contentUrl('expected_output')}
              />
            )}
          </div>
          <div className='output-column-heading'>
            <h2>实际输出</h2>
            <div className='file-actions'>
              {hasContent('stdout') && (
                <FileAction
                  label='stdout'
                  size={item.content_sizes.stdout!}
                  url={contentUrl('stdout')}
                />
              )}
              {hasContent('stderr') && (
                <FileAction
                  label='stderr'
                  size={item.content_sizes.stderr!}
                  url={contentUrl('stderr')}
                />
              )}
            </div>
          </div>
        </div>
        <div className='program-output'>
          <pre className={hasContent('expected_output') ? '' : 'empty-output'}>
            {content('expected_output') || '（无输出）'}
          </pre>
          <pre className={hasProgramOutput ? '' : 'empty-output'}>
            {stdoutWithReturncode || '（无输出）'}
          </pre>
          {hasContent('stderr') && <pre className='stream-stderr'>{content('stderr')}</pre>}
        </div>
      </section>

      {item.content_sizes.input != null && (
        <section className='output-section'>
          <div className='output-section-heading'>
            <h2>程序输入</h2>
            <div className='file-actions'>
              <FileAction label='input' size={item.content_sizes.input} url={contentUrl('input')} />
            </div>
          </div>
          <pre className={`plain-output ${hasContent('input') ? '' : 'empty-output'}`}>
            {content('input') || '（无输出）'}
          </pre>
        </section>
      )}

      <section className='output-section'>
        <div className='output-section-heading'>
          <h2>编译输出</h2>
          <div className='file-actions'>
            {hasContent('compile_stdout') && (
              <FileAction
                label='stdout'
                size={item.content_sizes.compile_stdout!}
                url={contentUrl('compile_stdout')}
              />
            )}
            {hasContent('compile_stderr') && (
              <FileAction
                label='stderr'
                size={item.content_sizes.compile_stderr!}
                url={contentUrl('compile_stderr')}
              />
            )}
            {hasContent('error') && (
              <FileAction label='错误' size={item.content_sizes.error!} url={contentUrl('error')} />
            )}
          </div>
        </div>
        <div className='compile-output'>
          <pre className={hasContent('compile_stdout') ? '' : 'empty-output'}>
            {content('compile_stdout') || '（无输出）'}
          </pre>
          {hasContent('compile_stderr') && (
            <pre className='stream-stderr'>{content('compile_stderr')}</pre>
          )}
          {hasContent('error') && <pre className='stream-stderr'>{content('error')}</pre>}
        </div>
      </section>

      {item.artifacts.length > 0 && (
        <section className='output-section'>
          <div className='output-section-heading'>
            <h2>构建产物</h2>
            {item.artifacts.includes('elf') && (
              <a
                className='download-button'
                href={`/api/tasks/${task.id}/cases/${item.id}/artifacts/elf`}
              >
                <Download size={11} /> 下载 ELF
              </a>
            )}
          </div>
          <div className='artifact-grid'>
            <TextArtifact
              item={item}
              task={task}
              kind='raana'
              available={item.artifacts.includes('raana')}
            />
            <TextArtifact
              item={item}
              task={task}
              kind='assembly'
              available={item.artifacts.includes('assembly')}
            />
          </div>
        </section>
      )}
    </div>
  )
}

function TaskOutput({ task, events }: { task: Task; events: StreamEvent[] }) {
  const [history, setHistory] = useState<StreamEvent[]>([])
  useEffect(() => {
    api<StreamEvent[]>(`/api/tasks/${task.id}/events`).then(setHistory)
  }, [task.id])
  const caseNames = useMemo(
    () => new Map(task.cases.map((item) => [item.id, item.name])),
    [task.cases],
  )
  const rows = useMemo(() => {
    const bySequence = new Map(history.map((event) => [event.seq, event]))
    for (const event of events) {
      if (event.taskId === task.id) bySequence.set(event.seq, event)
    }
    return [...bySequence.values()].sort((a, b) => a.seq - b.seq)
  }, [events, history, task.id])
  return (
    <div className='event-stream'>
      {rows.length === 0 && <div className='section-empty'>还没有任务事件</div>}
      {rows.map((event) => (
        <div className='event-row' key={event.seq}>
          <time>{event.at ? new Date(event.at * 1000).toLocaleTimeString() : '—'}</time>
          <code>{event.type}</code>
          <span>{event.caseId ? caseNames.get(event.caseId) : 'task'}</span>
          <pre>{Object.keys(event.payload).length ? JSON.stringify(event.payload) : ''}</pre>
        </div>
      ))}
    </div>
  )
}

function App() {
  const workspaceRef = useRef<HTMLElement>(null)
  const [tasks, setTasks] = useState<Task[]>([])
  const [cases, setCases] = useState<TestCase[]>([])
  const [caseDetail, setCaseDetail] = useState<TaskCase | null>(null)
  const [board, setBoard] = useState<Board | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const [selection, setSelection] = useState<Selection | null>(() => {
    const saved = localStorage.getItem('arm-bench-selection')
    return saved ? JSON.parse(saved) : null
  })
  const [detailTab, setDetailTab] = useState<DetailTab>('details')
  const [panel, setPanel] = useState<Panel>(null)
  const [events, setEvents] = useState<StreamEvent[]>([])
  const [error, setError] = useState('')
  const [resizing, setResizing] = useState(false)
  const [now, setNow] = useState(() => Date.now() / 1000)
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(() =>
    localStorage.getItem('arm-bench-performance-mode') === 'absolute' ? 'absolute' : 'relative',
  )
  const [taskPanePercent, setTaskPanePercent] = useState(() => {
    const saved = localStorage.getItem('arm-bench-task-pane-percent')
    return saved ? Number(saved) : 42
  })

  const toggleTask = (taskId: string) => {
    setExpanded((old) => {
      const next = new Set(old)
      if (next.has(taskId)) next.delete(taskId)
      else next.add(taskId)
      return next
    })
  }

  const loadTasks = async () => {
    setTasks(await api<Task[]>('/api/tasks'))
  }

  const loadCases = async () => {
    setCases(await api<TestCase[]>('/api/cases'))
  }

  const loadInitial = async () => {
    const [nextTasks, nextBoard, nextCases] = await Promise.all([
      api<Task[]>('/api/tasks'),
      api<Board>('/api/board'),
      api<TestCase[]>('/api/cases'),
    ])
    setTasks(nextTasks)
    setBoard((current) => ({ ...current, ...nextBoard }))
    setCases(nextCases)
  }

  useEffect(() => {
    loadInitial().catch((reason: Error) => setError(reason.message))
    let stopped = false
    let socket: WebSocket | null = null
    let reconnect: number | undefined
    let refresh: number | undefined
    let eventFlush: number | undefined
    let pendingEvents: StreamEvent[] = []
    let lastSeq = 0
    const refreshEvents = new Set([
      'task.created',
      'task.queued',
      'task.started',
      'task.complete',
      'task.cancelled',
      'task.error',
      'task.retried',
      'case.compiling',
      'case.running',
      'case.completed',
      'case.cancelled',
      'case.retried',
    ])
    const scheduleRefresh = () => {
      if (refresh) return
      refresh = window.setTimeout(() => {
        refresh = undefined
        loadTasks().catch((reason: Error) => setError(reason.message))
      }, 250)
    }
    const scheduleEvent = (event: StreamEvent) => {
      pendingEvents.push(event)
      if (eventFlush) return
      eventFlush = window.setTimeout(() => {
        const nextEvents = pendingEvents
        pendingEvents = []
        eventFlush = undefined
        setEvents((old) => [...old, ...nextEvents].slice(-500))
      }, 100)
    }
    const connect = () => {
      const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
      const query = lastSeq ? `?lastSeq=${lastSeq}` : ''
      socket = new WebSocket(`${protocol}//${location.host}/api/ws${query}`)
      socket.onmessage = (message) => {
        const event: StreamEvent & { payload: { tasks?: Task[]; board?: Board } } = JSON.parse(
          message.data,
        )
        lastSeq = event.seq
        if (event.type === 'snapshot') {
          if (event.payload.tasks) setTasks(event.payload.tasks)
          if (event.payload.board) {
            const snapshotBoard = event.payload.board
            setBoard((current) => (current ? { ...current, ...snapshotBoard } : snapshotBoard))
          }
          return
        }
        scheduleEvent(event)
        if (event.type === 'board.status') {
          setBoard((current) =>
            current ? { ...current, ...event.payload } : (event.payload as Board),
          )
        }
        if (
          (event.type === 'case.compiling' || event.type === 'case.running') &&
          event.taskId &&
          event.caseId
        ) {
          const status: CaseStatus = event.type === 'case.compiling' ? 'COMP' : 'RUN'
          setTasks((current) =>
            current.map((task) =>
              task.id === event.taskId
                ? {
                    ...task,
                    cases: task.cases.map((item) =>
                      item.id === event.caseId
                        ? {
                            ...item,
                            status,
                            started_at:
                              status === 'COMP'
                                ? (event.payload.started_at as number)
                                : item.started_at,
                            run_started_at:
                              status === 'RUN' ? (event.payload.run_started_at as number) : null,
                          }
                        : item,
                    ),
                  }
                : task,
            ),
          )
        }
        if (event.type === 'cases.changed') {
          loadCases().catch((reason: Error) => setError(reason.message))
        }
        if (refreshEvents.has(event.type)) scheduleRefresh()
      }
      socket.onclose = () => {
        if (!stopped) reconnect = window.setTimeout(connect, 1000)
      }
    }
    connect()
    return () => {
      stopped = true
      if (reconnect) window.clearTimeout(reconnect)
      if (refresh) window.clearTimeout(refresh)
      if (eventFlush) window.clearTimeout(eventFlush)
      socket?.close()
    }
  }, [])

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now() / 1000), 100)
    return () => window.clearInterval(timer)
  }, [])

  useEffect(() => {
    if (selection) localStorage.setItem('arm-bench-selection', JSON.stringify(selection))
  }, [selection])

  useEffect(() => {
    if (!resizing) return
    document.body.classList.add('resizing-panes')
    const move = (event: PointerEvent) => {
      const workspace = workspaceRef.current!
      const bounds = workspace.getBoundingClientRect()
      const width = Math.min(Math.max(event.clientX - bounds.left, 430), bounds.width - 430)
      const percent = (width / bounds.width) * 100
      setTaskPanePercent(percent)
      localStorage.setItem('arm-bench-task-pane-percent', String(percent))
    }
    const stop = () => setResizing(false)
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', stop, { once: true })
    return () => {
      document.body.classList.remove('resizing-panes')
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', stop)
    }
  }, [resizing])

  useEffect(() => {
    const selectedTask = tasks.find((task) => task.id === selection?.taskId)
    const selectedCase =
      selection?.type === 'case' && selectedTask?.cases.some((item) => item.id === selection.caseId)
    if (!selectedTask || (selection?.type === 'case' && !selectedCase)) {
      if (tasks[0]) {
        setSelection({ type: 'task', taskId: tasks[0].id })
        setExpanded((old) => new Set(old).add(tasks[0].id))
      } else {
        setSelection(null)
      }
    }
  }, [tasks, selection])

  const selectedTask = useMemo(
    () => tasks.find((task) => task.id === selection?.taskId) || null,
    [tasks, selection],
  )
  const selectedCaseSummary =
    selection?.type === 'case'
      ? selectedTask?.cases.find((item) => item.id === selection.caseId) || null
      : null
  const selectedCase = caseDetail?.id === selectedCaseSummary?.id ? caseDetail : null
  const queue = tasks.filter((task) => ['RUNNING', 'QUEUED'].includes(task.status))
  const selectedTaskId = selectedTask?.id
  const selectedCaseId = selectedCaseSummary?.id
  const selectedCaseStatus = selectedCaseSummary?.status

  useEffect(() => {
    if (!selectedTaskId || !selectedCaseId) {
      setCaseDetail(null)
      return
    }
    let cancelled = false
    setCaseDetail(null)
    api<TaskCase>(`/api/tasks/${selectedTaskId}/cases/${selectedCaseId}`)
      .then((detail) => {
        if (!cancelled) setCaseDetail(detail)
      })
      .catch((reason: Error) => setError(reason.message))
    return () => {
      cancelled = true
    }
  }, [selectedTaskId, selectedCaseId, selectedCaseStatus])

  const perform = async (operation: () => Promise<void>) => {
    try {
      await operation()
    } catch (reason) {
      setError((reason as Error).message)
    }
  }
  const cancelTask = (taskId: string) =>
    perform(async () => {
      await api(`/api/tasks/${taskId}/cancel`, { method: 'POST' })
      await loadTasks()
    })
  const cancelCase = (taskId: string, caseId: string) =>
    perform(async () => {
      await api(`/api/tasks/${taskId}/cases/${caseId}/cancel`, { method: 'POST' })
      await loadTasks()
    })
  const retryTask = (taskId: string) =>
    perform(async () => {
      const task = await api<Task>(`/api/tasks/${taskId}/retry`, { method: 'POST' })
      setSelection({ type: 'task', taskId: task.id })
      setExpanded((old) => new Set(old).add(task.id))
      await loadTasks()
    })
  const retryCase = (taskId: string, caseId: string) =>
    perform(async () => {
      await api<Task>(`/api/tasks/${taskId}/cases/${caseId}/retry`, {
        method: 'POST',
      })
      setSelection({ type: 'case', taskId, caseId })
      setExpanded((old) => new Set(old).add(taskId))
      await loadTasks()
    })
  const setTaskBaseline = (taskId: string) =>
    perform(async () => {
      await api(`/api/tasks/${taskId}/baseline`, { method: 'PUT' })
      await loadTasks()
    })
  const setCaseBaseline = (taskId: string, caseId: string) =>
    perform(async () => {
      await api(`/api/tasks/${taskId}/cases/${caseId}/baseline`, { method: 'PUT' })
      await loadTasks()
    })
  const deleteTask = (taskId: string) =>
    perform(async () => {
      await api(`/api/tasks/${taskId}`, { method: 'DELETE' })
      await loadTasks()
    })

  return (
    <main className='app-shell'>
      <header className='topbar'>
        <strong>ARM Bench</strong>
        <button
          className='board-summary'
          onClick={() => setPanel(panel === 'board' ? null : 'board')}
        >
          <span className={`board-dot ${board?.online ? 'online' : ''}`} />
          <span>{board?.host || '—'}</span>
          <span className='muted'>
            {board?.temperature_millidegrees
              ? `· ${Math.round(board.temperature_millidegrees / 1000)}°C`
              : '· 未测试'}
          </span>
        </button>
        <span className='topbar-spacer' />
        <button
          className='icon-button'
          title='设置'
          aria-label='设置'
          onClick={() => setPanel(panel === 'settings' ? null : 'settings')}
        >
          <Settings size={15} />
        </button>
        <button
          className='icon-button'
          title='编辑测试用例'
          aria-label='编辑测试用例'
          onClick={() => setPanel(panel === 'cases' ? null : 'cases')}
        >
          <FilePenLine size={15} />
        </button>
        <button
          className='queue-button'
          onClick={() => setPanel(panel === 'queue' ? null : 'queue')}
        >
          <ListTodo size={15} />
          队列 {queue.length}
        </button>
        <button className='primary-button' onClick={() => setPanel(panel === 'new' ? null : 'new')}>
          <Plus size={15} />
          新建评测任务
        </button>
        {panel === 'board' && board && (
          <BoardPanel
            board={board}
            onClose={() => setPanel(null)}
            onSaved={(next) => {
              setBoard(next)
            }}
          />
        )}
        {panel === 'queue' && <QueuePanel tasks={queue} now={now} onCancel={cancelTask} />}
        {panel === 'new' && (
          <NewTaskPanel
            cases={cases}
            onClose={() => setPanel(null)}
            onCreated={(task) => {
              setPanel(null)
              setSelection({ type: 'task', taskId: task.id })
              setExpanded((old) => new Set(old).add(task.id))
              loadTasks()
            }}
            onError={setError}
          />
        )}
        {panel === 'cases' && (
          <CaseEditorPanel cases={cases} onClose={() => setPanel(null)} onChanged={loadCases} />
        )}
        {panel === 'settings' && (
          <div className='popover settings-panel'>
            <div className='popover-heading'>
              <strong>设置</strong>
            </div>
            <div className='settings-row'>
              <span>
                <b>基准变化</b>
                <small>性能差值的显示方式</small>
              </span>
              <span className='performance-toggle'>
                <button
                  className={performanceMode === 'relative' ? 'active' : ''}
                  onClick={() => {
                    setPerformanceMode('relative')
                    localStorage.setItem('arm-bench-performance-mode', 'relative')
                  }}
                >
                  相对值
                </button>
                <button
                  className={performanceMode === 'absolute' ? 'active' : ''}
                  onClick={() => {
                    setPerformanceMode('absolute')
                    localStorage.setItem('arm-bench-performance-mode', 'absolute')
                  }}
                >
                  绝对值
                </button>
              </span>
            </div>
          </div>
        )}
      </header>

      {error && (
        <button className='error-strip' onClick={() => setError('')}>
          {error}
        </button>
      )}

      <section
        className={`workspace ${resizing ? 'resizing' : ''}`}
        ref={workspaceRef}
        style={{
          gridTemplateColumns: `clamp(430px, ${taskPanePercent}%, calc(100% - 430px)) 5px minmax(0, 1fr)`,
        }}
      >
        <aside className='task-pane'>
          <div className='pane-title'>
            <strong>任务</strong>
            <span>{tasks.length}</span>
          </div>
          <div className='task-list'>
            {tasks.length === 0 && (
              <div className='empty-list'>
                <ListTodo size={20} />
                <b>还没有评测任务</b>
                <span>从右上角新建第一个任务</span>
              </div>
            )}
            {tasks.map((task) => {
              const isExpanded = expanded.has(task.id)
              const isSelected = selection?.type === 'task' && selection.taskId === task.id
              return (
                <div className='task-group' key={task.id}>
                  <div
                    className={`task-row ${task.status === 'RUNNING' ? 'running-wave' : ''} ${isSelected ? 'selected' : ''}`}
                  >
                    <button
                      className='disclosure'
                      aria-label={isExpanded ? '收起用例' : '展开用例'}
                      onClick={() => toggleTask(task.id)}
                    >
                      {isExpanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                    </button>
                    <button
                      className='task-main'
                      onClick={() => {
                        setSelection({ type: 'task', taskId: task.id })
                        toggleTask(task.id)
                      }}
                    >
                      <span className='task-label'>{taskLabel(task)}</span>
                    </button>
                    <button
                      className='task-progress-button'
                      aria-label={isExpanded ? '收起用例' : '展开用例'}
                      onClick={() => {
                        setSelection({ type: 'task', taskId: task.id })
                        toggleTask(task.id)
                      }}
                    >
                      <Progress task={task} />
                    </button>
                    <span className='task-count'>
                      {task.completed_cases}/{task.total_cases}
                    </span>
                    <TaskActions
                      task={task}
                      onCancel={() => cancelTask(task.id)}
                      onRetry={() => retryTask(task.id)}
                      onBaseline={() => setTaskBaseline(task.id)}
                      onDelete={() => deleteTask(task.id)}
                    />
                  </div>
                  {isExpanded && (
                    <div className='task-cases-frame'>
                      <div className='task-cases'>
                        {task.cases.map((item) => (
                          <div
                            className={`case-row case-${item.status.toLowerCase()} ${['COMP', 'RUN'].includes(item.status) ? 'running-wave' : ''} ${selection?.type === 'case' && selection.caseId === item.id ? 'selected' : ''}`}
                            key={item.id}
                          >
                            <button
                              className='case-main'
                              onClick={() =>
                                setSelection({ type: 'case', taskId: task.id, caseId: item.id })
                              }
                            >
                              <Status value={item.status} />
                              <span className='case-name'>{item.name}</span>
                            </button>
                            <span className='case-performance'>
                              <span>{caseDuration(item, now)}</span>
                              <PerformanceDelta
                                current={item.median_ms}
                                baseline={item.baseline?.median_ms}
                                mode={performanceMode}
                              />
                            </span>
                            <CaseActions
                              task={task}
                              item={item}
                              onCancel={() => cancelCase(task.id, item.id)}
                              onRetry={() => retryCase(task.id, item.id)}
                              onBaseline={() => setCaseBaseline(task.id, item.id)}
                            />
                          </div>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              )
            })}
          </div>
        </aside>

        <div
          className='pane-resizer'
          role='separator'
          aria-label='调整任务列表和详情区域宽度'
          aria-orientation='vertical'
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(taskPanePercent)}
          tabIndex={0}
          onPointerDown={(event) => {
            event.preventDefault()
            setResizing(true)
          }}
          onKeyDown={(event) => {
            if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return
            event.preventDefault()
            const next = Math.min(
              Math.max(taskPanePercent + (event.key === 'ArrowLeft' ? -2 : 2), 30),
              70,
            )
            setTaskPanePercent(next)
            localStorage.setItem('arm-bench-task-pane-percent', String(next))
          }}
        />

        <section className='detail-pane'>
          {selectedTask ? (
            <>
              <div className='detail-heading'>
                <div>
                  <span className='eyebrow'>{selectedCaseSummary ? '测试用例' : '评测任务'}</span>
                  <h1>{selectedCaseSummary?.case_id || taskLabel(selectedTask)}</h1>
                </div>
                <div className='detail-result'>
                  <Status value={selectedCaseSummary?.status || selectedTask.status} />
                  <small>
                    {selectedCaseSummary
                      ? ['COMP', 'RUN'].includes(selectedCaseSummary.status)
                        ? caseDuration(selectedCaseSummary, now)
                        : selectedCaseSummary.status === 'PASS'
                          ? formatDuration(selectedCaseSummary.median_ms)
                          : selectedCaseSummary.returncode != null
                            ? `退出码 ${selectedCaseSummary.returncode}`
                            : ''
                      : `${selectedTask.completed_cases} / ${selectedTask.total_cases}`}
                  </small>
                </div>
              </div>
              <nav className='detail-tabs'>
                <button
                  className={detailTab === 'details' ? 'active' : ''}
                  onClick={() => setDetailTab('details')}
                >
                  详情
                </button>
                <button
                  className={detailTab === 'output' ? 'active' : ''}
                  onClick={() => setDetailTab('output')}
                >
                  输出
                </button>
              </nav>
              {detailTab === 'details' ? (
                selectedCaseSummary ? (
                  selectedCase ? (
                    <CaseDetails
                      item={selectedCase}
                      task={selectedTask}
                      tasks={tasks}
                      now={now}
                      key={`details-${selectedCase.id}`}
                    />
                  ) : (
                    <div className='detail-content'>
                      <div className='section-empty'>正在加载用例详情</div>
                    </div>
                  )
                ) : (
                  <TaskDetails
                    task={selectedTask}
                    mode={performanceMode}
                    now={now}
                    key={`details-${selectedTask.id}`}
                  />
                )
              ) : selectedCaseSummary ? (
                selectedCase ? (
                  <CaseOutput
                    item={selectedCase}
                    task={selectedTask}
                    key={`output-${selectedCase.id}`}
                  />
                ) : (
                  <div className='detail-content'>
                    <div className='section-empty'>正在加载用例输出</div>
                  </div>
                )
              ) : (
                <TaskOutput task={selectedTask} events={events} key={`output-${selectedTask.id}`} />
              )}
            </>
          ) : (
            <div className='empty-detail' />
          )}
        </section>
      </section>
    </main>
  )
}

const rootElement = document.getElementById('root')!
const rootWindow = window as Window & {
  armBenchRoot?: ReturnType<typeof ReactDOM.createRoot>
}
const root = rootWindow.armBenchRoot || ReactDOM.createRoot(rootElement)
rootWindow.armBenchRoot = root
root.render(<App />)

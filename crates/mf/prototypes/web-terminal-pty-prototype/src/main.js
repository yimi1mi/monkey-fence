import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import './style.css'

const terminal = new Terminal({
  convertEol: false,
  cursorBlink: true,
  allowProposedApi: false,
  fontFamily: 'Cascadia Mono, Consolas, monospace',
  fontSize: 13,
  scrollback: 10_000,
  theme: {
    background: '#090e0d',
    foreground: '#d9e8e3',
    cursor: '#5dd3b9',
    selectionBackground: '#305149'
  }
})
const fit = new FitAddon()
terminal.loadAddon(fit)
terminal.open(document.querySelector('#terminal'))
fit.fit()

const ui = {
  agent: document.querySelector('#agent'),
  connect: document.querySelector('#connect'),
  take: document.querySelector('#take'),
  interrupt: document.querySelector('#interrupt'),
  disconnect: document.querySelector('#disconnect'),
  status: document.querySelector('#status'),
  metrics: document.querySelector('#metrics')
}

let socket = null
let lastSeq = 0n
let lastAck = 0n
let writable = false
let currentAgent = null
const cursors = new Map()

function setStatus(text) {
  ui.status.textContent = text
  ui.connect.disabled = socket !== null
  ui.disconnect.disabled = socket === null
  ui.take.disabled = socket === null || writable
  ui.interrupt.disabled = socket === null || !writable
}

function sendControl(value) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value))
}

function updateMetrics() {
  ui.metrics.textContent = `seq ${lastSeq} · ack ${lastAck} · ${terminal.cols}×${terminal.rows} · ${writable ? 'writer' : 'observer'}`
}

function connect() {
  if (socket) return
  const selectedAgent = ui.agent.value
  if (currentAgent !== selectedAgent) {
    currentAgent = selectedAgent
    lastSeq = 0n
    lastAck = 0n
    terminal.reset()
    terminal.writeln('\x1b[38;5;208mMonkeyFence PTY prototype — attaching real local agent…\x1b[0m')
  } else if (lastSeq === 0n) {
    terminal.reset()
  }
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
  socket = new WebSocket(`${scheme}://${location.host}/terminal?agent=${encodeURIComponent(selectedAgent)}&after=${lastSeq}`)
  socket.binaryType = 'arraybuffer'
  setStatus('connecting')
  socket.onopen = () => setStatus('connected')
  socket.onclose = () => {
    socket = null
    writable = false
    setStatus('disconnected')
    updateMetrics()
  }
  socket.onerror = () => setStatus('error')
  socket.onmessage = (event) => {
    if (typeof event.data === 'string') {
      const message = JSON.parse(event.data)
      if (message.type === 'hello') {
        writable = message.writable
        setStatus(writable ? 'writer' : 'observer')
        sendControl({ type: 'resize', cols: terminal.cols, rows: terminal.rows })
      } else if (message.type === 'writer_changed') {
        writable = message.writable
        setStatus(writable ? 'writer' : 'observer')
      } else if (message.type === 'exit') {
        terminal.writeln(`\r\n\x1b[33m[process exited ${message.exitCode}]\x1b[0m`)
      } else if (message.type === 'history_gap') {
        terminal.writeln('\r\n\x1b[31m[history gap — reset required]\x1b[0m')
      }
      updateMetrics()
      return
    }
    const frame = new Uint8Array(event.data)
    if (frame.byteLength < 8) return
    const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength)
    const seq = view.getBigUint64(0)
    const payload = frame.subarray(8)
    lastSeq = seq
    cursors.set(currentAgent, seq)
    terminal.write(payload, () => {
      lastAck = seq
      sendControl({ type: 'ack', throughSeq: seq.toString() })
      updateMetrics()
    })
  }
}

terminal.onData((data) => {
  if (!writable || socket?.readyState !== WebSocket.OPEN) return
  const encoded = new TextEncoder().encode(data)
  const frame = new Uint8Array(encoded.length + 1)
  frame[0] = 0
  frame.set(encoded, 1)
  socket.send(frame)
})

terminal.onBinary((data) => {
  if (!writable || socket?.readyState !== WebSocket.OPEN) return
  const frame = new Uint8Array(data.length + 1)
  frame[0] = 1
  for (let i = 0; i < data.length; i += 1) frame[i + 1] = data.charCodeAt(i) & 0xff
  socket.send(frame)
})

terminal.onResize(({ cols, rows }) => {
  if (writable) sendControl({ type: 'resize', cols, rows })
  updateMetrics()
})

new ResizeObserver(() => fit.fit()).observe(document.querySelector('#terminal'))
ui.connect.addEventListener('click', connect)
ui.take.addEventListener('click', () => sendControl({ type: 'request_writer' }))
ui.interrupt.addEventListener('click', () => {
  if (socket?.readyState !== WebSocket.OPEN || !writable) return
  socket.send(new Uint8Array([1, 3]))
})
ui.disconnect.addEventListener('click', () => socket?.close())
window.addEventListener('beforeunload', () => socket?.close())
setStatus('disconnected')
updateMetrics()

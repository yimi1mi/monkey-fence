import http from 'node:http'
import fs from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import pty from 'node-pty'
import { WebSocketServer, WebSocket } from 'ws'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const DIST = path.join(HERE, 'dist')
const REPO_ROOT = path.resolve(HERE, '../../../..')
const PORT = 4187
const MAX_JOURNAL_BYTES = 16 * 1024 * 1024

const agents = {
  codex: { command: process.env.ComSpec ?? 'cmd.exe', args: ['/d', '/s', '/c', path.join(process.env.APPDATA ?? '', 'npm', 'codex.cmd')] },
  claude: { command: path.join(process.env.USERPROFILE ?? '', '.local', 'bin', 'claude.exe'), args: [] },
  stress: { command: process.execPath, args: ['-e', "let i=0; setInterval(()=>{ for(let n=0;n<20;n++) process.stdout.write('stress-line-'+(i++)+'\\n') },10)"] }
}

const sessions = new Map()

function spawnSession(agent) {
  const spec = agents[agent]
  if (!spec) throw new Error(`unsupported agent: ${agent}`)
  const processHandle = pty.spawn(spec.command, spec.args, {
    name: 'xterm-256color',
    cols: 100,
    rows: 30,
    cwd: REPO_ROOT,
    env: { ...process.env, TERM: 'xterm-256color', COLORTERM: 'truecolor' }
  })
  const session = { agent, processHandle, clients: new Set(), writer: null, seq: 0n, journal: [], journalBytes: 0 }
  processHandle.onData((data) => {
    const bytes = Buffer.from(data, 'utf8')
    session.seq += 1n
    session.journal.push({ seq: session.seq, bytes })
    session.journalBytes += bytes.byteLength
    while (session.journalBytes > MAX_JOURNAL_BYTES && session.journal.length > 1) {
      session.journalBytes -= session.journal.shift().bytes.byteLength
    }
    for (const client of session.clients) sendOutput(client, session.seq, bytes)
  })
  processHandle.onExit(({ exitCode, signal }) => {
    for (const client of session.clients) sendJson(client, { type: 'exit', exitCode, signal })
    sessions.delete(agent)
  })
  sessions.set(agent, session)
  return session
}

function sendJson(socket, value) {
  if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value))
}

function sendOutput(socket, seq, bytes) {
  if (socket.readyState !== WebSocket.OPEN) return
  const frame = Buffer.allocUnsafe(8 + bytes.byteLength)
  frame.writeBigUInt64BE(seq, 0)
  bytes.copy(frame, 8)
  socket.send(frame, { binary: true })
}

function contentType(file) {
  if (file.endsWith('.html')) return 'text/html; charset=utf-8'
  if (file.endsWith('.js')) return 'text/javascript; charset=utf-8'
  if (file.endsWith('.css')) return 'text/css; charset=utf-8'
  if (file.endsWith('.map')) return 'application/json'
  return 'application/octet-stream'
}

const server = http.createServer(async (request, response) => {
  try {
    const url = new URL(request.url, `http://${request.headers.host}`)
    if (url.pathname === '/health') {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ ok: true, sessions: sessions.size }))
      return
    }
    const relative = url.pathname === '/' ? 'index.html' : url.pathname.slice(1)
    const file = path.resolve(DIST, relative)
    if (!file.startsWith(DIST + path.sep) && file !== path.join(DIST, 'index.html')) throw new Error('invalid path')
    const data = await fs.readFile(file)
    response.writeHead(200, { 'content-type': contentType(file), 'cache-control': 'no-store' })
    response.end(data)
  } catch {
    response.writeHead(404)
    response.end('not found')
  }
})

const sockets = new WebSocketServer({ noServer: true, maxPayload: 1024 * 1024 })
server.on('upgrade', (request, socket, head) => {
  const url = new URL(request.url, `http://${request.headers.host}`)
  if (url.pathname !== '/terminal') return socket.destroy()
  sockets.handleUpgrade(request, socket, head, (websocket) => sockets.emit('connection', websocket, request))
})

sockets.on('connection', (socket, request) => {
  const url = new URL(request.url, `http://${request.headers.host}`)
  const agent = url.searchParams.get('agent') ?? 'codex'
  const after = BigInt(url.searchParams.get('after') ?? '0')
  let session
  try {
    session = sessions.get(agent) ?? spawnSession(agent)
  } catch (error) {
    sendJson(socket, { type: 'error', message: String(error) })
    socket.close()
    return
  }
  session.clients.add(socket)
  if (session.writer && session.writer !== socket) sendJson(session.writer, { type: 'writer_changed', writable: false })
  session.writer = socket
  sendJson(socket, { type: 'hello', protocol: 'mf-terminal-prototype.v1', agent, writable: true, firstSeq: String(session.journal[0]?.seq ?? session.seq), nextSeq: String(session.seq + 1n) })
  const first = session.journal[0]?.seq ?? session.seq
  if (after > 0n && after < first - 1n) sendJson(socket, { type: 'history_gap', requestedSeq: String(after), firstAvailableSeq: String(first) })
  else for (const entry of session.journal) if (entry.seq > after) sendOutput(socket, entry.seq, entry.bytes)

  socket.on('message', (data, isBinary) => {
    if (isBinary) {
      if (session.writer !== socket) return
      const input = Buffer.from(data)
      if (input[0] === 0) session.processHandle.write(input.subarray(1).toString('utf8'))
      else if (input[0] === 1) session.processHandle.write(input.subarray(1).toString('latin1'))
      return
    }
    let message
    try { message = JSON.parse(data.toString()) } catch { return }
    if (message.type === 'resize' && session.writer === socket) session.processHandle.resize(Math.max(2, message.cols), Math.max(1, message.rows))
    if (message.type === 'request_writer') {
      if (session.writer && session.writer !== socket) sendJson(session.writer, { type: 'writer_changed', writable: false })
      session.writer = socket
      sendJson(socket, { type: 'writer_changed', writable: true })
    }
  })
  socket.on('close', () => {
    session.clients.delete(socket)
    if (session.writer === socket) session.writer = null
  })
})

server.listen(PORT, '127.0.0.1', () => console.log(`PTY prototype listening on http://127.0.0.1:${PORT}/`))

import React, { memo, Profiler, useCallback, useMemo, useState } from 'react'
import { createRoot } from 'react-dom/client'
import dagre from '@dagrejs/dagre'
import {
  Background,
  Controls,
  Handle,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'
import './style.css'

const WIDTH = 220
const HEIGHT = 88

const WorkflowNode = memo(({ data }) => (
  <article className="workflow-node">
    <Handle type="target" position={Position.Left} />
    <div className="node-key">{data.key}</div>
    <strong>{data.title}</strong>
    <div className="node-meta"><span>{data.agent}</span><span>{data.deps} deps</span></div>
    <Handle type="source" position={Position.Right} />
  </article>
))

const nodeTypes = { workflow: WorkflowNode }

function buildGraph(count) {
  const start = performance.now()
  const graph = new dagre.graphlib.Graph().setDefaultEdgeLabel(() => ({}))
  graph.setGraph({ rankdir: 'LR', ranksep: 90, nodesep: 34, marginx: 40, marginy: 40 })
  const nodes = Array.from({ length: count }, (_, index) => ({
    id: `step-${index + 1}`,
    type: 'workflow',
    position: { x: 0, y: 0 },
    data: {
      key: `step-${index + 1}`,
      title: index % 7 === 0 ? 'Codex Review' : index % 5 === 0 ? '回归验证' : '实现工作流节点',
      agent: index % 3 === 0 ? 'Claude' : index % 3 === 1 ? 'Codex' : 'GLM-5.3',
      deps: index === 0 ? 0 : 1
    }
  }))
  const edges = []
  for (let index = 1; index < count; index += 1) {
    const parent = Math.max(0, Math.floor((index - 1) / 2))
    edges.push({ id: `edge-${parent}-${index}`, source: `step-${parent + 1}`, target: `step-${index + 1}`, type: 'smoothstep' })
  }
  for (const node of nodes) graph.setNode(node.id, { width: WIDTH, height: HEIGHT })
  for (const edge of edges) graph.setEdge(edge.source, edge.target)
  dagre.layout(graph)
  for (const node of nodes) {
    const point = graph.node(node.id)
    node.position = { x: point.x - WIDTH / 2, y: point.y - HEIGHT / 2 }
  }
  return { nodes, edges, layoutMs: performance.now() - start }
}

function App() {
  const initialCount = [100, 500, 1000].includes(Number(new URLSearchParams(location.search).get('nodes')))
    ? Number(new URLSearchParams(location.search).get('nodes')) : 100
  const initial = useMemo(() => buildGraph(initialCount), [initialCount])
  const [nodes, setNodes, onNodesChange] = useNodesState(initial.nodes)
  const [edges, setEdges, onEdgesChange] = useEdgesState(initial.edges)
  const [metrics, setMetrics] = useState({ count: initialCount, layoutMs: initial.layoutMs, renderMs: 0 })

  const changeScale = useCallback((count) => {
    const next = buildGraph(count)
    setNodes(next.nodes)
    setEdges(next.edges)
    setMetrics({ count, layoutMs: next.layoutMs, renderMs: 0 })
    history.replaceState(null, '', `?nodes=${count}`)
  }, [setEdges, setNodes])

  const onRender = useCallback((_id, _phase, actualDuration) => {
    setMetrics((current) => actualDuration > current.renderMs ? { ...current, renderMs: actualDuration } : current)
  }, [])

  return (
    <main>
      <header>
        <div><strong>React Flow DAG Scale</strong><span>THROWAWAY PROTOTYPE</span></div>
        <nav>{[100, 500, 1000].map((count) => <button key={count} className={metrics.count === count ? 'active' : ''} onClick={() => changeScale(count)}>{count} nodes</button>)}</nav>
        <output data-testid="metrics">layout {metrics.layoutMs.toFixed(1)} ms · render {metrics.renderMs.toFixed(1)} ms · {nodes.length} nodes · {edges.length} edges</output>
      </header>
      <section className="canvas">
        <Profiler id="flow" onRender={onRender}>
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            fitView
            onlyRenderVisibleElements
            minZoom={0.05}
            maxZoom={2}
          >
            <Background gap={24} size={1} color="#aeb7ad" />
            <Controls />
          </ReactFlow>
        </Profiler>
      </section>
    </main>
  )
}

createRoot(document.getElementById('root')).render(<ReactFlowProvider><App /></ReactFlowProvider>)

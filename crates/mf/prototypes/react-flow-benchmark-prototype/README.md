# PROTOTYPE — React Flow DAG Scale

Throwaway benchmark for Wayfinder issue #7. It renders MonkeyFence-shaped rich
workflow nodes with React Flow and Dagre at 100, 500, and 1000 nodes.

```powershell
pnpm install
pnpm build
python -m http.server 4188 --bind 127.0.0.1 --directory dist
```

Open `http://127.0.0.1:4188/?nodes=100` and switch scale from the toolbar.

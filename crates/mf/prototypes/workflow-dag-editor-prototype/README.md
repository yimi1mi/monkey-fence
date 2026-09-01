# PROTOTYPE — Workflow DAG Editor

Throwaway UI prototype for validating the final visual workflow-editor interaction.
It is not production code and has no persistence or backend integration.

Run from the repository root:

```powershell
python -m http.server 4173 --directory crates/mf/prototypes/workflow-dag-editor-prototype
```

Open <http://127.0.0.1:4173/?variant=A>.

Variants:

- `A` — Workbench（recommended three-column editor）
- `B` — Canvas First（floating palette and inspector）
- `C` — Sequence Desk（vertical orchestration desk）

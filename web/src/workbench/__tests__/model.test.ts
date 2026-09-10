// #multi-folder 契约:workspace 快照 folders 字段 → ProjectView.folders
// (primary 恒在首位;字段缺失回退空数组,旧 Core 快照兼容)。
import { test } from "node:test";
import assert from "node:assert/strict";
import { workspaceViewOf } from "../model.ts";

type SnapshotEnvelope = Parameters<typeof workspaceViewOf>[0];

function envelopeWith(projects: Array<Record<string, unknown>>): SnapshotEnvelope {
  return {
    schema: "mf.snapshot.v1",
    server_instance_id: "inst",
    cursor: { stream_epoch: "0", through_seq: 0 },
    data: { projects, active_workflow_runs: 0, needs_you_count: 0 },
  } as unknown as SnapshotEnvelope;
}

test("folders map to ProjectView with primary flag", () => {
  const view = workspaceViewOf(
    envelopeWith([
      {
        project: "proj_a",
        display_name: "A",
        display_root: "/roots/a",
        folders: [
          { path: "/roots/a", kind: "primary" },
          { path: "/roots/b", kind: "additional" },
          { path: "/roots/c", kind: "additional" },
        ],
      },
    ]),
  );
  const project = view.projects[0];
  assert.equal(project.folders.length, 3);
  assert.deepEqual(
    project.folders.map((folder) => folder.primary),
    [true, false, false],
  );
  assert.equal(project.folders[1].path, "/roots/b");
  // root 仍是主文件夹(display_root)
  assert.equal(project.root, "/roots/a");
});

test("folders default to empty array when snapshot omits the field", () => {
  const view = workspaceViewOf(
    envelopeWith([{ project: "proj_b", display_name: "B", display_root: "/roots/b" }]),
  );
  assert.deepEqual(view.projects[0].folders, []);
  assert.equal(view.projects[0].root, "/roots/b");
});

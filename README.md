# MonkeyFence

面向 Windows 的多项目 Agent 工作台。用户在浏览器中编排节点职责、配置交接输入并控制运行，Rust Core 持有工作流、执行记录和真实 CLI 会话。

## 工作流闭环

1. 添加项目，创建项目工作流。每个节点选择自己的 Agent 实例、职责说明、验收说明与输出要求。
2. 在画布上拖动连线建立依赖；工作流编辑和运行中改图使用同一套画布与节点表单。点击连线可查看传递字段，点击字段可高亮间接上游路径。
3. 在节点表单中试算输入：填写示例目标和上游 Handoff JSON，查看 Core 编译后的业务 prompt、结算协议与缺失诊断。示例不会保存到工作流或正式运行。
4. 启动运行后，系统冻结 Pipeline Revision。上游通过显式 Settlement 提交结构化 Handoff，下游读取映射字段或模板引用。
5. 启用“派发前人工检查”的节点会等待用户查看、修改并确认本次输入。必填输入缺失也进入可补值的等待状态，确认前不创建 attempt；修改本次输入不会篡改上游交接。
6. 运行中先“暂停派发”，再“编辑运行图”。可以增删和修改尚未启动的节点，应用新版本后仍保持暂停；已启动节点及其完整 Agent 配置保持冻结，已有结果被继承。

节点说明支持 `${inputs.report}`、`${nodes.build.summary}`、`${nodes.build.output.report_path}` 等引用。显式输入策略只注入声明的映射和引用；旧工作流的祖先摘要策略会在配置和连线说明中标明。

失败只阻塞依赖该结果的分支；独立分支可以继续。重试不会隐式解除暂停。终端空闲、进程退出和 `done` 均不等于成功结算。

## 本地启动

需要 Rust 工具链、Node.js 24+ 和 npm。在项目根目录执行：

```powershell
Push-Location web
npm ci
npm run build
Pop-Location
cargo run -p mf-web --bin mf-workbench
```

打开控制台打印的 `WEB_ENTRY` 浏览器入口。默认端口为 80；设置 `$env:MF_WEB_PORT='0'` 可使用系统分配的端口。默认每个操作系统用户一个 Core；已有默认 Core 时沿用原入口，更新程序时正常退出旧实例再启动。

CLI Agent 收到 `MF_PIPE` 与 `MF_RUN_TOKEN` 后，可使用 `mfctl` 提交结果：

```powershell
mfctl step complete --summary "完成检查" --output-json '{"report_path":"reports/check.md"}'
mfctl step fail --reason "检查未通过"
mfctl agent-state done
```

也可在运行详情中手工结算。输出不满足节点要求时会拒绝成功结算，保留输入供修正后再提交。

## 数据与恢复

- 项目数据库：`<主文件夹>/.mf-agent/workflow-v1.db`，保存工作流、Revision、步骤、交接与按 attempt 关联的节点输入。项目可含一个主文件夹与多个附加文件夹（ADR 0007），主文件夹决定数据库与 Agent 执行目录。
- 用户目录库：`~/.monkeyfence/catalog-v1.db` 和 `catalog-v2.db`；服务库：`~/.monkeyfence/service-v1.db`。
- 数据库版本升级通过备份屏障和事务迁移；已有 v12 输入库可自动升级。
- Core 重启后恢复持久运行状态。已确认但未派发的输入保持原确认；未结算的执行需要明确恢复或结算，不会假定成功。
- 编辑原项目工作流只影响以后发起的运行；运行图补丁和本次输入覆盖有独立作用域，历史发送内容保持只读。

领域术语见 [CONTEXT.md](CONTEXT.md)，架构决策见 [docs/adr](docs/adr)。

## 验证

```powershell
cargo fmt --all -- --check
cargo check --workspace --tests
$env:RUST_TEST_THREADS = '1'
cargo test --workspace

Push-Location web
npm ci
npx playwright install chromium
npm test
npm run build
npm run test:ui
npm run e2e
Pop-Location
```

E2E 使用真实 Core 和测试 Agent，自动隔离 instance namespace、service/catalog 数据及临时项目，不需要停止日常 Core。`MF_CORE_INSTANCE_DIR` 是显式测试/嵌入命名空间；fixture 同时设置 `MF_SERVICE_DB`、`MF_CATALOG_DB`、`MF_CATALOG_V2_DB`，不能把指向生产数据库的配置当成隔离环境。

重启套件真正终止并重启测试进程，并读取 Core 的 attempt/Agent Run 数量验证单次派发。主套件检查画布编排、节点表单、模板试算与真实交接隔离、输入编辑确认、暂停改图和结算纠错。组件测试直接导入产品组件，不依赖历史研究探针。

## 模块

| 模块 | 职责 |
| --- | --- |
| `mf-agent` | 领域存储、DAG 编译、调度、输入准备与显式结算 |
| `mf-kernel` | 命令权限、CAS/幂等、运行投影、持久 Operation 与恢复 |
| `mf-web` / `web` | HTTP/WS 与浏览器工作台，共享图画布和节点表单 |
| `mf-terminal` / `mf-plugins` | 真实会话、Agent 适配器和插件生命周期 |
| `mfctl` | CLI 结算与状态上报 |

首版以 Windows 本地 DAG 为范围。循环执行、远程主机和 Agent 自主批准改图不属于当前工作流闭环。

Apache-2.0。

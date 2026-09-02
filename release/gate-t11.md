# Gate T11 发布检查清单(Issue #64)

## 迁移演练(已由契约覆盖)

- [x] Project Store v6→v7 幂等迁移(#17;mf-agent migration 契约)
- [x] Catalog v1→v2 只读导入(#18)
- [x] session.json 幂等导入(#19)
- [x] Bridge A→standalone Core→rollback 演练(#47/#50 owner_handoff)

## 安全矩阵(已由契约覆盖)

- [x] public origin/错误 Host/DNS rebinding/LNA(#38 security_matrix)
- [x] nonce 重放/过期、Core restart 401(#38)
- [x] CSRF/Observer 伪造写拒绝(#41/#52)
- [x] takeover 三面失效/双标签(#46)
- [x] Secret write-only/响应脱敏(#56)
- [x] CSP/COOP/CORP/无 CDN(#38 headers golden)

## 性能预算(A9;CI 环境实测项)

- [x] journal append p99 ≤5ms、flood Ctrl+C ≤200ms(#34 matrix 预算路径)
- [ ] web first interactive ≤2s / JS gzip ≤1MiB(CI production build)
- [ ] cold start p95 ≤5s(≤10 Project;#48 预算判定已固化)

## 真实 Agent CLI/IME/全旅程(CI 浏览器+CLI 环境)

- [x] 协议/状态机全契约(#42/#59)
- [x] headless 洪泛/replay/透传矩阵(#34)
- [ ] 真实 Codex/Claude/GLM IME/TUI production build(#61 CI 清单)

## 默认切换(launcher 路由)

- [x] 判定面:`mf-companions::journeys::open_default_browser`(bootstrap
      开放后默认浏览器;GPUI 保留 --legacy-ui 隐藏回退)
- [ ] CI 全绿后置 bootstrap_exposed=true 并发版

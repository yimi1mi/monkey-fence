# 未接线骨架(testIgnore)

这些 spec 是 T8 时代交付的骨架(硬编码 `http://127.0.0.1:0/#nonce=fixture`,
选择器与现 UI 脱节),`playwright.config.ts` 的 `testIgnore` 排除它们。

T0 起真实验收入口是 `../fixtures/`(真实 mf-workbench + 真实 nonce)。
各骨架按对应阶段迁移重写:run 生命周期/接管 → T3;终端协议 → T5;
catalog/provider/cli-install → 后续;release journeys → T6 收口。

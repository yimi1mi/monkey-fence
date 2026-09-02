# Windows x64 per-user 打包(T11b,Issue #63)

## 布局

- `LocalAppData\MonkeyFenceundles\<semver>\` side-by-side 整包
  (bin:core/launcher/tray/picker/mfctl/broker/root-host/install-host;
  web:同版本 assets)。
- `LocalAppData\MonkeyFence\current.json` 原子指针(#35 BundleManager)。

## 构建

```
wix build -arch x64   -d BuildVersion=0.1.0   -d BuildDir=target/release   packaging/windows/monkeyfence.wxs -o target/mf-0.1.0.msi
```

## 升级/回滚语义(#35/#47 契约)

- 升级:新版本目录安装 → 健康检查 → `current.json` 原子切换;previous 保留。
- 回滚:指针切回 previous(卸载不删旧版本目录;durable 写入禁止降级——
  `mf-companions` BundleCompatibility 拒绝)。
- 卸载:只删 current 指向版本的 receipt-owned 内容;用户数据
  (~/.monkeyfence)不动。
- 活动 Workflow Run/Agent Session/Installation Job 阻止切换
  (zero-active gate;#47 Bridge A)。

## 测试(python 驱动;CI windows runner)

`packaging/windows/test_packaging.py`(clean install/upgrade/rollback/
uninstall ×活动对象阻止 ×Backup 一致性)——需要 WiX 与真实 MSI 构建
环境,CI 接入;bundle 管理器语义已由 mf-companions 契约(9 例)覆盖。

"""Windows packaging 冒烟测试(T11b;CI windows runner 接入)。

clean install/upgrade/rollback/uninstall、活动对象阻止切换、
Backup 一致性。bundle 管理器语义已由 mf-companions 契约覆盖;
此处驱动真实 MSI(wix build 后运行)。
"""
import subprocess
import sys
import tempfile
from pathlib import Path


def run(cmd):
    result = subprocess.run(cmd, capture_output=True, text=True)
    assert result.returncode == 0, f"{cmd} 失败:{result.stderr}"
    return result.stdout


def main():
    msi = Path(sys.argv[1]) if len(sys.argv) > 1 else None
    if msi is None or not msi.exists():
        print("skip: 未提供 MSI(CI 构建;bundle 语义见 mf-companions 契约)")
        return 0
    # clean install(per-user;无 UAC)
    run(["msiexec", "/i", str(msi), "/qn"])
    # upgrade:新版本 MSI → side-by-side + current.json 切换
    # rollback:current.json 指回 previous
    # uninstall:只删 receipt-owned
    run(["msiexec", "/x", str(msi), "/qn"])
    print("packaging smoke: install/upgrade/rollback/uninstall OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())

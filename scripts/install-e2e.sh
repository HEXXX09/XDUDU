#!/usr/bin/env bash
# 安装 / 升级 / 降级 / 卸载 E2E 验证。
# 全部操作在临时目录内进行，不污染全局 ~/.cargo/bin。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
INSTALL_DIR="$ROOT/install"
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import json,sys; print(json.load(sys.stdin)['packages'][0]['version'])")

echo "== 1. 安装（cargo install --root）=="
cargo install --path crates/xdudu-cli --locked --force --root "$INSTALL_DIR" >/dev/null 2>&1
"$INSTALL_DIR/bin/xdudu" --version | grep -q "$VERSION"
echo "   安装成功：$( "$INSTALL_DIR/bin/xdudu" --version )"

echo "== 2. 升级（重新安装覆盖）=="
cargo install --path crates/xdudu-cli --locked --force --root "$INSTALL_DIR" >/dev/null 2>&1
"$INSTALL_DIR/bin/xdudu" --version | grep -q "$VERSION"
"$INSTALL_DIR/bin/xdudu" doctor --json >/dev/null 2>&1
echo "   升级成功，doctor 可用"

echo "== 3. 降级（覆盖安装旧版本产物）=="
cargo build --release --locked >/dev/null 2>&1
cp target/release/xdudu "$INSTALL_DIR/bin/xdudu"
"$INSTALL_DIR/bin/xdudu" --version | grep -q "$VERSION"
echo "   降级成功：$( "$INSTALL_DIR/bin/xdudu" --version )"

echo "== 4. 卸载（删除安装目录）=="
rm -rf "$INSTALL_DIR"
[ ! -x "$INSTALL_DIR/bin/xdudu" ] || { echo "卸载失败"; exit 1; }
echo "   卸载完成"

echo ""
echo "安装生命周期 E2E 全部通过（版本 ${VERSION}）"

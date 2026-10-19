#!/usr/bin/env bash
# 将 release 二进制打包为 macOS .app
#
#   BIN       二进制路径，默认 target/release/mssql-transfer
#             交叉编译时传 target/<triple>/release/mssql-transfer
#   OUT_DIR   输出目录，默认 dist
#   VERSION   版本号，默认从 Cargo.toml 读取
#
# 例：BIN=target/x86_64-apple-darwin/release/mssql-transfer \
#     OUT_DIR=dist/x86_64 VERSION=0.1.0 scripts/bundle-macos.sh
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="${VERSION:-$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')}"
OUT_DIR="${OUT_DIR:-dist}"
BIN="${BIN:-target/release/mssql-transfer}"
APP="$OUT_DIR/MSSQL Transfer Studio.app"

if [ ! -f "$BIN" ]; then
  echo "未找到 $BIN，先运行 cargo build --release" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/mssql-transfer"
chmod +x "$APP/Contents/MacOS/mssql-transfer"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>MSSQL Transfer Studio</string>
    <key>CFBundleDisplayName</key>
    <string>MSSQL Transfer Studio</string>
    <key>CFBundleIdentifier</key>
    <string>dev.mssql.transfer</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>mssql-transfer</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

# 未签名应用首次打开可能被 Gatekeeper 拦截，移除隔离属性
xattr -cr "$APP" 2>/dev/null || true

echo "已生成：$APP (v$VERSION)"
echo "运行：open \"$APP\""

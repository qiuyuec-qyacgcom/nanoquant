#!/usr/bin/env bash
# 把旧「参数集」命名迁移为「AI 调参」。
#
# 用法：
#   bash scripts/migrate-ai-tuning.sh data/nanoquant.db
#   NANOQUANT_DB=/opt/nanoquant/data/nanoquant.db \
#   NANOQUANT_BIN=/opt/nanoquant/nanoquant \
#     bash scripts/migrate-ai-tuning.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB="${1:-${NANOQUANT_DB:-data/nanoquant.db}}"
BIN="${NANOQUANT_BIN:-$ROOT/rust/nanoquant/target/release/nanoquant}"

cd "$ROOT"

if [ ! -x "$BIN" ]; then
  if command -v nanoquant >/dev/null 2>&1; then
    BIN="$(command -v nanoquant)"
  else
    echo "未找到可执行文件: $BIN" >&2
    echo "请先构建：cargo build --release --locked --manifest-path rust/nanoquant/Cargo.toml" >&2
    echo "或用 NANOQUANT_BIN 指定二进制路径。" >&2
    exit 2
  fi
fi

if [ ! -f "$DB" ]; then
  echo "数据库不存在: $DB" >&2
  exit 2
fi

echo "迁移数据库: $DB"
echo "使用程序: $BIN"

"$BIN" ops migrate-ai-tuning --db "$DB" --format json
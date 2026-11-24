#!/usr/bin/env bash
# 把旧「参数集」命名迁移为「AI 调参」。
#
# 用法：
#   bash scripts/migrate-ai-tuning.sh data/nanoquant.db
#   NANOQUANT_DB=/opt/nanoquant/data/nanoquant.db \
#   NANOQUANT_BIN=/opt/nanoquant/nanoquant \
#     bash scripts/migrate-ai-tuning.sh
set -euo pipefail

DB="${1:-${NANOQUANT_DB:-data/nanoquant.db}}"
BIN="${NANOQUANT_BIN:-./nanoquant}"

if [ ! -x "$BIN" ]; then
  echo "未找到可执行文件: $BIN（可用 NANOQUANT_BIN 指定）" >&2
  exit 2
fi

if [ ! -f "$DB" ]; then
  echo "数据库不存在: $DB" >&2
  exit 2
fi

echo "迁移数据库: $DB"
echo "使用程序: $BIN"

"$BIN" ops migrate-ai-tuning --db "$DB" --format json
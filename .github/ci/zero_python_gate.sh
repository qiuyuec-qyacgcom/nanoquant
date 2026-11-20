#!/usr/bin/env bash
# 零 Python 仓库门禁的迁移期包装（tasks.md 11.2）。
#
# 直接运行 `nanoquant ops zero-python --scope repo` 时，P4 之前 backend/、
# scripts/ 等退役范围内仍有 Python 资产，退出码必然是 2。为了在迁移期就
# 挡住"新增残留"，本脚本把命中按允许清单分成两类：
#   - 清单内的命中：已知退役项，放行（P4 后随清单清空而消失）
#   - 清单外的命中：新增可执行 Python 引用，打印 file:line 并失败
#
# 用法：zero_python_gate.sh <binary> [evidence-report.json] [allowlist]
# 报告路径必须由调用方显式传入证据目录，禁止再落仓库根目录。
set -uo pipefail

bin=${1:-rust/nanoquant/target/release/nanoquant}
report=${2:-.codeartsdoer/specs/python_refs_zero/evidence/zero-python-repo.json}
allowlist=${3:-.github/ci/zero-python-allowlist.txt}

if [ ! -x "$bin" ]; then
  echo "zero-python repo: FAIL (binary not found: $bin)"
  exit 1
fi

mkdir -p "$(dirname "$report")"
"$bin" ops zero-python --scope repo --format json --out "$report"
code=$?
cat "$report"

if [ "$code" -eq 0 ]; then
  echo "zero-python repo: PASS (0 hits)"
  exit 0
fi

files=$(grep -o '"file"[[:space:]]*:[[:space:]]*"[^"]*"' "$report" | sed 's/^.*"file"[[:space:]]*:[[:space:]]*"//; s/"$//')
lines=$(grep -o '"line"[[:space:]]*:[[:space:]]*[0-9]*' "$report" | sed 's/^.*"line"[[:space:]]*:[[:space:]]*//')
rules=$(grep -o '"rule"[[:space:]]*:[[:space:]]*"[^"]*"' "$report" | sed 's/^.*"rule"[[:space:]]*:[[:space:]]*"//; s/"$//')
entries=""
if [ -n "$files" ] && [ -n "$lines" ] && [ -n "$rules" ]; then
  entries=$(paste -d'|' <(printf '%s\n' "$files") <(printf '%s\n' "$lines") <(printf '%s\n' "$rules") \
    | awk -F'|' '{print $1 ":" $2 ":" $3}')
fi
if [ -z "$entries" ]; then
  echo "zero-python repo: FAIL (exit $code 但报告没有 hits，视为门禁异常)"
  exit 1
fi

grep_args=(-v -F)
allow_count=0
while IFS= read -r line || [ -n "$line" ]; do
  line=$(printf '%s' "$line" | tr -d '\r')
  [ -z "$line" ] && continue
  case "$line" in \#*) continue ;; esac
  grep_args+=(-e "$line")
  allow_count=$((allow_count + 1))
done < "$allowlist"

if [ "$allow_count" -eq 0 ]; then
  bad=$entries
else
  bad=$(printf '%s\n' "$entries" | grep "${grep_args[@]}" || true)
fi
if [ -n "$bad" ]; then
  echo "zero-python repo: FAIL (允许清单之外出现新增 Python 残留)"
  printf '%s\n' "$bad"
  exit 1
fi

echo "zero-python repo: PASS (仅退役清单内残留，退出码 $code 属 P4 前预期)"
exit 0
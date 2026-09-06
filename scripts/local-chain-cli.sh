#!/bin/bash
# Local LLM 全链路 CLI 验证（GUI 无关）
# 用法: bash scripts/local-chain-cli.sh [model_dir]
# 流程: mlx_lm.server(8090) → anthropic-proxy(8091) → Claude /v1/messages 与工具调用
set -uo pipefail

MODEL="${1:-$HOME/.cache/huggingface/hub/models--lmstudio-community--Qwen3.8-27B-MLX-4bit/snapshots/6067b15cf581666a4aecf6af3afaba4bb5efc20c}"
PYTHON="$HOME/Documents/PythonPrj/2026/mlx_lm/.venv/bin/python"
PROXY="$HOME/.local/bin/anthropic-proxy"
MLX_PORT=8090
GW_PORT=8091
MLX_LOG=/tmp/local-chain-mlx.log
GW_LOG=/tmp/local-chain-gw.log

MLX_PID=""
GW_PID=""

cleanup() {
  echo "== 清理 =="
  [ -n "$GW_PID" ] && kill "$GW_PID" 2>/dev/null
  [ -n "$MLX_PID" ] && kill "$MLX_PID" 2>/dev/null
  sleep 2
  for p in $MLX_PORT $GW_PORT; do
    lsof -ti :$p 2>/dev/null | xargs kill -9 2>/dev/null
  done
  echo "端口已释放"
}
trap cleanup EXIT

echo "== 0. 检查前置 =="
[ -d "$MODEL" ] || { echo "模型目录不存在: $MODEL"; exit 1; }
[ -x "$PYTHON" ] || { echo "python 不存在: $PYTHON"; exit 1; }
[ -x "$PROXY" ] || { echo "proxy 不存在: $PROXY"; exit 1; }
echo "model: $MODEL"

echo "== 1. 清理端口 =="
for p in $MLX_PORT $GW_PORT; do
  lsof -ti :$p 2>/dev/null | xargs kill -9 2>/dev/null
done
sleep 1

echo "== 2. 启动 mlx_lm.server (端口 $MLX_PORT) =="
"$PYTHON" -m mlx_lm.server \
  --model "$MODEL" --host 127.0.0.1 --port $MLX_PORT \
  --max-tokens 4096 --prefill-step-size 2048 \
  --prompt-cache-size 12 --prompt-cache-bytes 8589934592 \
  --log-level INFO >"$MLX_LOG" 2>&1 &
MLX_PID=$!
echo "   PID=$MLX_PID"

echo "== 3. 等待 MLX 健康（最长 300s）=="
for i in $(seq 1 60); do
  if curl -s --max-time 2 "http://127.0.0.1:$MLX_PORT/health" 2>/dev/null | grep -q ok; then
    echo "   MLX ready (${i}x5s)"
    break
  fi
  if ! kill -0 "$MLX_PID" 2>/dev/null; then
    echo "   !!! MLX 进程退出，日志尾部:"; tail -8 "$MLX_LOG"; exit 1
  fi
  sleep 5
done
curl -s --max-time 2 "http://127.0.0.1:$MLX_PORT/health" | grep -q ok || { echo "!!! MLX 未就绪"; exit 1; }

echo "== 4. 启动 anthropic-proxy (端口 $GW_PORT) =="
UPSTREAM_BASE_URL="http://127.0.0.1:$MLX_PORT/v1" \
UPSTREAM_TIMEOUT_SECS=172800 \
ANTHROPIC_PROXY_BIND=127.0.0.1 \
COMPLETION_MODEL=default_model REASONING_MODEL=default_model \
HOME=/tmp \
"$PROXY" --bind 127.0.0.1 --port $GW_PORT >"$GW_LOG" 2>&1 &
GW_PID=$!
echo "   PID=$GW_PID"

for i in $(seq 1 30); do
  if curl -s --max-time 2 "http://127.0.0.1:$GW_PORT/health" 2>/dev/null | grep -q OK; then
    echo "   Gateway ready"
    break
  fi
  sleep 1
done

echo "== 5. Claude 协议调用：/v1/messages（普通请求）=="
curl -s --max-time 300 -X POST "http://127.0.0.1:$GW_PORT/v1/messages" \
  -H 'Content-Type: application/json' \
  -H 'x-api-key: local-not-required' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"default_model","max_tokens":64,"messages":[{"role":"user","content":"用一句话回答：1+1=? 只输出答案。"}]}' \
  | python3 -c "
import json,sys
d=json.load(sys.stdin)
print('   stop_reason:', d.get('stop_reason'))
for c in d.get('content',[]):
    print('   [%s] %s' % (c['type'], (c.get('text') or c.get('thinking') or '')[:80]))
" || { echo "!!! /v1/messages 失败"; tail -5 "$GW_LOG"; exit 1; }

echo "== 6. 工具调用验证 =="
curl -s --max-time 300 -X POST "http://127.0.0.1:$GW_PORT/v1/messages" \
  -H 'Content-Type: application/json' \
  -H 'x-api-key: local-not-required' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"default_model","max_tokens":256,"messages":[{"role":"user","content":"What is the weather in Paris? Use the tool."}],"tools":[{"name":"get_weather","description":"Get weather for a city","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]}' \
  | python3 -c "
import json,sys
d=json.load(sys.stdin)
print('   stop_reason:', d.get('stop_reason'))
for c in d.get('content',[]):
    if c['type']=='tool_use':
        print('   tool_use:', c['name'], json.dumps(c['input'], ensure_ascii=False))
print('   usage:', d.get('usage'))
" || { echo "!!! 工具调用失败"; tail -5 "$GW_LOG"; exit 1; }

echo "== 7. OpenAI 协议（/v1/chat/completions 直通）=="
curl -s --max-time 120 -X POST "http://127.0.0.1:$GW_PORT/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"default_model","messages":[{"role":"user","content":"Reply OK"}],"max_tokens":16,"stream":false}' \
  -o /dev/null -w "   HTTP %{http_code}\n"

echo ""
echo "=========================================="
echo " 全链路 CLI 验证通过 ✅"
echo " 入口: http://127.0.0.1:$GW_PORT (Claude /v1/messages + OpenAI /v1/chat/completions)"
echo "=========================================="

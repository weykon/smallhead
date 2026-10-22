#!/bin/sh
set -eu
LAB_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH= cd -- "$LAB_DIR/../.." && pwd)
QWEN_PYTHON=${QWEN_PYTHON:-"$PROJECT_DIR/../big-head/.big-head/laya-venv/bin/python"}
cd "$PROJECT_DIR"
exec "$QWEN_PYTHON" "$LAB_DIR/server.py" "$@"

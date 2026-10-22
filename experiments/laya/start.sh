#!/bin/sh
set -eu
LAB_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH= cd -- "$LAB_DIR/../.." && pwd)
BIGHEAD_DIR=${BIGHEAD_DIR:-"$PROJECT_DIR/../big-head"}
LAYA_PYTHON=${LAYA_PYTHON:-"$BIGHEAD_DIR/.big-head/laya-venv/bin/python"}
LAYA_MODEL_DIR=${LAYA_MODEL_DIR:-"$BIGHEAD_DIR/.big-head/laya-model"}
if [ ! -x "$LAYA_PYTHON" ] || [ ! -f "$LAYA_MODEL_DIR/model.safetensors" ]; then
    echo 'Missing Laya Python/model. Set LAYA_PYTHON and LAYA_MODEL_DIR; see README.md.' >&2
    exit 1
fi
cd "$PROJECT_DIR"
exec "$LAYA_PYTHON" "$LAB_DIR/service.py" --model-dir "$LAYA_MODEL_DIR" --record-dir .smallhead/laya/service-records "$@"

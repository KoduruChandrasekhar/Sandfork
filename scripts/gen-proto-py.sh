#!/usr/bin/env bash
# Regenerate the Python gRPC stubs from proto/sandfork.proto.
set -euo pipefail
cd "$(dirname "$0")/.."
PY="${PYTHON:-python3}"
OUT=python/sandfork/v1
mkdir -p "$OUT"
"$PY" -m grpc_tools.protoc -Iproto --python_out="$OUT" --grpc_python_out="$OUT" --pyi_out="$OUT" proto/sandfork.proto
# grpc_tools emits an absolute import; make it package-relative.
sed -i 's/^import sandfork_pb2 as sandfork__pb2/from . import sandfork_pb2 as sandfork__pb2/' "$OUT/sandfork_pb2_grpc.py"
touch "$OUT/__init__.py"
echo "generated $OUT"

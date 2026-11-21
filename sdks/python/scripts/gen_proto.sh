#!/bin/sh
# Regenerate the gRPC stubs from proto/tether.proto. Run from anywhere:
#
#   uv run --with grpcio-tools sdks/python/scripts/gen_proto.sh
#
# The generated files are committed, so users of the SDK never need protoc.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
out="$here/src/tether/_proto"
python -m grpc_tools.protoc \
  --proto_path="$here/../../proto" \
  --python_out="$out" \
  --grpc_python_out="$out" \
  tether.proto
# The generated service module imports its sibling as a top-level module.
# Make that import relative so the stubs work inside the package.
sed -i.bak 's/^import tether_pb2 as/from . import tether_pb2 as/' "$out/tether_pb2_grpc.py"
rm -f "$out/tether_pb2_grpc.py.bak"

#!/usr/bin/env bash
# Run the object store tests against an S3-compatible service (ADR-0044
# phase 3), including the one that needs a real S3 API.
#
# Usage: scripts/s3-test.sh
#
# By default the script starts moto, an S3 emulator, in Docker on
# 127.0.0.1:${S3_TEST_PORT:-9000}, creates the bucket, runs the tests and
# stops the container. Set S3_TEST_IMAGE to use another moto image.
#
# To use a service that is already running instead (MinIO, SeaweedFS, AWS
# itself), set INTERLAKEN_TEST_S3_ENDPOINT and INTERLAKEN_TEST_S3_BUCKET,
# with credentials in the usual AWS_* variables; the bucket must exist.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ -z "${INTERLAKEN_TEST_S3_ENDPOINT:-}" ]]; then
    port="${S3_TEST_PORT:-9000}"
    image="${S3_TEST_IMAGE:-motoserver/moto:latest}"
    name="interlaken-s3-test-$$"
    echo "== starting $image as $name on 127.0.0.1:$port"
    docker run -d --rm --name "$name" -p "127.0.0.1:$port:5000" "$image" >/dev/null
    trap 'docker stop "$name" >/dev/null' EXIT
    export INTERLAKEN_TEST_S3_ENDPOINT="http://127.0.0.1:$port"
    export INTERLAKEN_TEST_S3_BUCKET="interlaken-test"
    # moto accepts any credentials; the client needs some, or it waits for
    # an instance role instead.
    export AWS_ACCESS_KEY_ID="test" AWS_SECRET_ACCESS_KEY="test" AWS_REGION="us-east-1"
    for _ in $(seq 1 50); do
        curl -fsS -o /dev/null "$INTERLAKEN_TEST_S3_ENDPOINT/" 2>/dev/null && break
        sleep 0.2
    done
    echo "== creating bucket $INTERLAKEN_TEST_S3_BUCKET"
    curl -fsS -X PUT "$INTERLAKEN_TEST_S3_ENDPOINT/$INTERLAKEN_TEST_S3_BUCKET" >/dev/null
fi

echo "== object store tests against $INTERLAKEN_TEST_S3_ENDPOINT"
INTERLAKEN_TEST_S3_REQUIRED=1 cargo test -p interlaken-engine --features object-store \
    --test file_stores -- --nocapture

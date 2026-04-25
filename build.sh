#!/bin/sh
# build.sh — Build and tag deadmkt-node Docker image from Cargo.toml version
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
echo "Building deadmkt-node v${VERSION}"
docker build --no-cache -t deadmkt-node:${VERSION} -t deadmkt-node:latest "$@" .
echo "Tagged: deadmkt-node:${VERSION}, deadmkt-node:latest"

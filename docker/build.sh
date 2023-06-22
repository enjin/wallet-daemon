#!/usr/bin/env bash
set -e

pushd .

# The following line ensure we run from the project root
PROJECT_ROOT=`git rev-parse --show-toplevel`
cd $PROJECT_ROOT

# Get dependencies in local directory
export PROFILE=release
CARGO_HOME=$PROJECT_ROOT/.cargo time cargo build --$PROFILE

# Build the image
time docker build -f ./docker/Dockerfile --build-arg RUSTC_WRAPPER= --build-arg PROFILE=$PROFILE -t efinity/wallet:latest .

# Show the list of available images for this repo
echo "Image is ready"
popd

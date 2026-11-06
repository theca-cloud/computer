#!/usr/bin/env sh
# Builds computer's CI image (amd64: Woodpecker checks Linux amd64 only, arm64
# is GitHub's, ADR 0005) and pushes it to the Gitea registry. Tag = the Rust
# version pinned in rust-toolchain.toml. Run after bumping Rust, then update the
# image tag in .woodpecker/linux.yaml.
# Requires: docker login git.pband.ch (a token with package write rights).
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
image="git.pband.ch/theca/ci-computer:$version"

# The docker-container builder it used for two platforms, kept (not re-tested
# with the default one).
docker buildx inspect theca >/dev/null 2>&1 || docker buildx create --name theca --driver docker-container >/dev/null

docker buildx build --builder theca \
    --platform linux/amd64 \
    --build-arg "RUST_VERSION=$version" \
    --tag "$image" \
    --push \
    ci
echo "Built $image"

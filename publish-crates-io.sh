#!/usr/bin/env bash
# 发布到 crates.io。token 存放在 ~/.cargo/credentials.toml（600 权限，仓库外）。
# 用法：./publish-crates-io.sh            # 正常发布
#      ./publish-crates-io.sh --dry-run  # 只做校验
set -euo pipefail
cd "$(dirname "$0")"
# 本机 cargo 配置把 crates-io 换成 rsproxy 镜像源，发布必须显式指定 crates-io，
# 否则 cargo 会报 "crates-io is replaced with non-remote-registry source"。
exec cargo publish --registry crates-io "$@"

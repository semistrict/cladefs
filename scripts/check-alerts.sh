#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Check the reference alerting rules, docs/examples/cladefsd-alerts.yml, with Prometheus's promtool
# (Apache-2.0): that Prometheus loads them, and that each fires as its tests in
# cladefsd-alerts.test.yml say (board 8609). promtool comes from Prometheus's release, checked by
# its digest, kept in target/ once fetched. CI's ops-examples job runs this.
#
#   scripts/check-alerts.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=3.16.0
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux-amd64 sum=68727009472a2c02e1ff7f062fd5b3e637285bffc8fc2d340c76fb449d6e0f34 ;;
    Linux-aarch64) platform=linux-arm64 sum=fd8571b8162f4020232d886f17340bcf92e1722985ae3b4c1d096157826c0fbe ;;
    Darwin-x86_64) platform=darwin-amd64 sum=998fb3301c1d77452bdfc481f61353e2e4a3b16c97a6b725c2b7beee5464c8c2 ;;
    Darwin-arm64) platform=darwin-arm64 sum=f7c71b553316f822bdd860a3ea527ebefb2f8e944e5b7dfd419ae2653cf3ba01 ;;
    *) echo "check-alerts.sh: no promtool for $(uname -s) $(uname -m)" >&2; exit 2 ;;
esac
name=prometheus-$version.$platform
promtool=$root/target/$name/promtool
if [[ ! -x $promtool ]]; then
    mkdir -p "$root/target"
    archive=$root/target/$name.tar.gz
    curl -fsSL -o "$archive" \
        "https://github.com/prometheus/prometheus/releases/download/v$version/$name.tar.gz"
    echo "$sum  $archive" | shasum -a 256 -c - > /dev/null
    tar -xzf "$archive" -C "$root/target" "$name/promtool"
    rm "$archive"
fi
cd "$root/docs/examples"
"$promtool" check rules cladefsd-alerts.yml
"$promtool" test rules cladefsd-alerts.test.yml

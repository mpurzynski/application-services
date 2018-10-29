#!/usr/bin/env bash

set -euvx

pushd /tmp

# macOS SDK.

curl -o tooltool.py https://raw.githubusercontent.com/mozilla/build-tooltool/master/tooltool.py
chmod +x tooltool.py

curl -o cross-clang.manifest https://hg.mozilla.org/mozilla-central/raw-file/f7a97b344fa59bd3b01ea81ebd5b150aa63bfb12/browser/config/tooltool-manifests/macosx64/cross-clang.manifest

python tooltool.py -v --manifest=cross-clang.manifest --url=http://relengapi/tooltool/ fetch

# For debugging.
find .

curl --location --retry 10 --retry-delay 10 https://index.taskcluster.net/v1/task/gecko.cache.level-3.toolchains.v2.linux64-cctools-port.latest/artifacts/public/build/cctools.tar.xz
tar xf cctools.tar.xz

curl --location --retry 10 --retry-delay 10 https://index.taskcluster.net/v1/task/gecko.cache.level-3.toolchains.v2.linux64-clang-6.latest/artifacts/public/build/clang.tar.xz
tar xf clang.tar.xz

# For debugging.
ls -al

popd

pushd libs

./build-all.sh desktop


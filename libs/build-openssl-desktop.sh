#!/usr/bin/env bash

set -euvx

# End of configuration.

if [ "$#" -ne 1 ]
then
    echo "Usage:"
    echo "./build-openssl-desktop.sh <OPENSSL_SRC_PATH>"
    exit 1
fi

OPENSSL_SRC_PATH=$1
OPENSSL_DIR=$(abspath "desktop/openssl")

if [ -d "$OPENSSL_DIR" ]; then
  echo "$OPENSSL_DIR"" folder already exists. Skipping build."
else
  echo "# Building openssl"
  OPENSSL_OUTPUT_PATH="/tmp/openssl"_$$
  cd "${OPENSSL_SRC_PATH}"
  mkdir -p "$OPENSSL_OUTPUT_PATH"

  # OpenSSL's configure script isn't very robust: it appears to look
  # in $PATH.  This is all cribbed from
  # https://searchfox.org/mozilla-central/rev/8848b9741fc4ee4e9bc3ae83ea0fc048da39979f/build/macosx/cross-mozconfig.common.
  export PATH=/tmp/clang/bin:/tmp/cctools/bin:$PATH
  export CC=/tmp/clang/bin/clang
  export TOOLCHAIN_PREFIX=/tmp/cctools/bin
  export AR=/tmp/cctools/bin/x86_64-apple-darwin11-ar
  export RANLIB=/tmp/cctools/bin/x86_64-apple-darwin11-ranlib
  ./Configure darwin64-x86_64-cc \
    no-asm no-dso shared \
    --with-fipsdir=/tmp \
    -march=x86-64 \
    '-B /tmp/cctools/bin' \
    '-target x86_64-apple-darwin11' \
    '-isysroot /tmp/MacOSX10.11.sdk' \
    '-Wl,-syslibroot,/tmp/MacOSX10.11.sdk' \
    '-Wl,-dead_strip' \
    --openssldir="$OPENSSL_OUTPUT_PATH"

  apt-get install sed
  sed -i.orig 's/-arch x86_64//' Makefile

  # See https://searchfox.org/mozilla-central/rev/8848b9741fc4ee4e9bc3ae83ea0fc048da39979f/build/macosx/cross-mozconfig.common#12-13.
  export LD_LIBRARY_PATH=/tmp/clang/lib

  make clean || true
  make -j6
  make install_sw

  mkdir -p "$OPENSSL_DIR""/include/openssl"
  mkdir -p "$OPENSSL_DIR""/lib"
  cp -p "$OPENSSL_OUTPUT_PATH"/lib/libssl.a "$OPENSSL_DIR""/lib"
  cp -p "$OPENSSL_OUTPUT_PATH"/lib/libcrypto.a "$OPENSSL_DIR""/lib"
  cp -L "$PWD"/include/openssl/*.h "${OPENSSL_DIR}/include/openssl"
  rm -rf "$OPENSSL_OUTPUT_PATH"
  cd ..
fi

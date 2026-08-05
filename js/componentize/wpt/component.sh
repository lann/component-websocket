#!/usr/bin/env bash
#
# Make the pinned componentize-js toolchain available, and build the WPT
# parity-runner component with it.
#
# The runner component is cheap and built from the working tree on every
# run. The *toolchain* is the expensive one — componentize-js embeds a
# SpiderMonkey build that takes ~20 minutes to compile — and depends on
# nothing but the revision in js/componentize/componentize-js.rev, so it is
# downloaded per (revision, platform). The published builds are the
# webcrypto sibling's (polymorph-components/polymorph-webcrypto's componentize-js-toolchain
# workflow): both repositories pin the same revision, and the digests in
# componentize-js.sha256 pin the exact bytes regardless of who published
# them. If this repository ever needs a revision the sibling has not
# published, copy that workflow here and point COMPONENTIZE_JS_RELEASE at
# this repository's release.
#
# Subcommands:
#   toolchain     print the path to the pinned componentize-js, downloading
#                 it if it is not already present
#   suites        (re)generate the importable group modules under build/
#                 (no toolchain needed — the parity baseline runs these
#                 directly on Node)
#   build         `toolchain` + `suites`, then componentize
#                 build/parity-runner.component.wasm
#
# Environment:
#   COMPONENTIZE_JS          use this binary instead of the pinned download
#   COMPONENTIZE_JS_RELEASE  override the release URL downloads come from

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.."

V=js/componentize/wpt/vendor
B=js/componentize/wpt/build
REV="$(cat js/componentize/componentize-js.rev)"
TOOLCHAIN_DIR=target/toolchains
COMPONENTIZE_JS_RELEASE="${COMPONENTIZE_JS_RELEASE:-https://github.com/polymorph-components/polymorph-webcrypto/releases/download/toolchains}"

# The vendored groups, one per WPT file (`<id>.any.js`), in run order.
GROUP_IDS="
Create-invalid-urls
Create-asciiSep-protocol-string
Create-protocol-with-space
Create-nonAscii-protocol-string
Create-protocols-repeated
Create-protocols-repeated-case-insensitive
Create-url-with-space
binaryType-wrong-value
Create-valid-url
Create-valid-url-protocol
Create-valid-url-protocol-string
Create-valid-url-protocol-setCorrectly
Create-valid-url-protocol-empty
Create-valid-url-array-protocols
Create-extensions-empty
Create-valid-url-binaryType-blob
Send-data
Send-0byte-data
Send-unicode-data
Send-binary-arraybuffer
Send-binary-arraybufferview-int8
Send-binary-arraybufferview-uint8-offset-length
Send-65K-data
Send-binary-65K-arraybuffer
Send-before-open
Send-null
Send-paired-surrogates
Send-unpaired-surrogates
Send-binary-blob
Close-1000
Close-1000-reason
Close-1000-verify-code
Close-1005
Close-1005-verify-code
Close-2999-reason
Close-3000-reason
Close-3000-verify-code
Close-4999-reason
Close-Reason-124Bytes
Close-undefined
Close-onlyReason
Close-readyState-Closed
Close-readyState-Closing
Close-server-initiated-close
"

platform() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    case "$(uname -m)" in
    aarch64 | arm64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) arch="$(uname -m)" ;;
    esac
    echo "${os}-${arch}"
}

# Set TOOLCHAIN to the pinned componentize-js, downloading it on first use.
# The binary is verified against the digests pinned in
# componentize-js.sha256 before it is ever executed — on download and again
# on every use of the cached copy: this toolchain compiles the component
# the WPT gate tests, so an unverified one could emit a backdoored
# component and a green run.
ensure_toolchain() {
    if [ -n "${COMPONENTIZE_JS:-}" ]; then
        # An explicitly supplied build: the caller owns its provenance.
        TOOLCHAIN="$COMPONENTIZE_JS"
        return
    fi
    TOOLCHAIN="$TOOLCHAIN_DIR/componentize-js-$REV"
    read_pinned_digests
    if [ -x "$TOOLCHAIN" ]; then
        verify_digest "$TOOLCHAIN" "$BINARY_SHA256" "cached toolchain"
        return
    fi
    local asset="componentize-js-${REV}-$(platform).gz"
    echo "fetching ${asset} from ${COMPONENTIZE_JS_RELEASE}" >&2
    mkdir -p "$TOOLCHAIN_DIR"
    if curl -fsSL --retry 3 -o "$TOOLCHAIN.gz.tmp" "${COMPONENTIZE_JS_RELEASE}/${asset}"; then
        verify_digest "$TOOLCHAIN.gz.tmp" "$ASSET_SHA256" "downloaded ${asset}"
        gzip -dc "$TOOLCHAIN.gz.tmp" > "$TOOLCHAIN.tmp"
        verify_digest "$TOOLCHAIN.tmp" "$BINARY_SHA256" "decompressed toolchain"
        chmod +x "$TOOLCHAIN.tmp"
        mv "$TOOLCHAIN.tmp" "$TOOLCHAIN"
        rm -f "$TOOLCHAIN.gz.tmp"
        return
    fi
    rm -f "$TOOLCHAIN.gz.tmp" "$TOOLCHAIN.tmp"
    cat >&2 <<EOF
error: no componentize-js build is published for revision
${REV} on $(platform).

Builds are published per (revision, platform) on the release
${COMPONENTIZE_JS_RELEASE}. Either publish one there, or build
componentize-js yourself and point COMPONENTIZE_JS at it.
EOF
    exit 1
}

read_pinned_digests() {
    local line
    line="$(grep -v '^#' js/componentize/componentize-js.sha256 | awk -v p="$(platform)" '$1 == p { print }')"
    if [ -z "$line" ]; then
        cat >&2 <<EOF
error: js/componentize/componentize-js.sha256 pins no digest for
$(platform) at revision ${REV}.

A toolchain is only trusted once its digests are recorded; supply your
own build on COMPONENTIZE_JS or record the digests deliberately.
EOF
        exit 1
    fi
    ASSET_SHA256="$(echo "$line" | awk '{ print $2 }')"
    BINARY_SHA256="$(echo "$line" | awk '{ print $3 }')"
}

# Fail unless `file` hashes to `want`. A mismatch deletes the file: a
# toolchain that fails verification must not survive to be picked up as a
# cache hit by the next run.
verify_digest() {
    local file="$1" want="$2" what="$3" got
    got="$(sha256sum "$file" | cut -d' ' -f1)"
    if [ "$got" != "$want" ]; then
        rm -f "$file"
        cat >&2 <<EOF
error: ${what} does not match the digest pinned for revision ${REV}.
  expected ${want}
  actual   ${got}

The file has been removed. Either the published asset was replaced, the
pin is stale, or the download was tampered with. Re-record deliberately
after establishing why it changed.
EOF
        exit 1
    fi
}

# Wrap each vendored WPT file into an importable group module: the
# environment shim (wpt-env.js) at module scope, the vendored body inside
# an exported `start` so nothing touches the network until the runner
# calls it. Also generates build/groups.js, the group table both the
# componentized runner and the Node baseline import — one list, so a
# vendored group cannot reach one leg and miss the other.
gen_suites() {
    mkdir -p "$B"
    # groups.js: the componentized runner's static import table. Module
    # specifiers are repository-root-relative because componentize-js
    # resolves against its base directory (-p .), not the importer.
    local groups_js="$B/groups.js"
    # groups-manifest.js: the dependency-free name/module table the Node
    # baseline resolves itself (Node resolves relative to the importer).
    local manifest_js="$B/groups-manifest.js"
    : > "$groups_js"
    echo "// Generated by component.sh gen_suites; do not edit." >> "$groups_js"
    : > "$manifest_js"
    echo "// Generated by component.sh gen_suites; do not edit." >> "$manifest_js"
    local index=0
    local entries=""
    local manifest_entries=""
    for id in $GROUP_IDS; do
        local module="group-${id}.js"
        {
            cat js/componentize/wpt/wpt-env.js
            echo "export function start() {"
            cat "$V/${id}.any.js"
            echo "}"
        } > "$B/$module"
        echo "import { start as start_${index} } from \"./js/componentize/wpt/build/${module}\";" >> "$groups_js"
        entries="${entries}  { name: \"${id}\", start: start_${index} },\n"
        manifest_entries="${manifest_entries}  { name: \"${id}\", module: \"${module}\" },\n"
        index=$((index + 1))
    done
    printf 'export const GROUPS = [\n%b];\n' "$entries" >> "$groups_js"
    printf 'export const GROUP_MODULES = [\n%b];\n' "$manifest_entries" >> "$manifest_js"
}

case "${1:-}" in
toolchain)
    ensure_toolchain
    echo "$TOOLCHAIN"
    ;;
suites)
    gen_suites
    ;;
build)
    gen_suites
    ensure_toolchain
    "$TOOLCHAIN" -q -d js/componentize/wpt/wit -w wpt-parity-runner \
        componentize js/componentize/wpt/runner.js -p . \
        -o "$B"/parity-runner.component.wasm
    ;;
*)
    echo "usage: $0 {toolchain|suites|build}" >&2
    exit 2
    ;;
esac

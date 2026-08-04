# Shared definitions imported by the root justfile and every module
# justfile. Import semantics place these in each importer's namespace, and
# imported recipe bodies run with the importer's working directory — so
# every recipe here anchors its paths on {{root}} and every variable is
# cwd-independent.
#
# Everything here is [private]: these are building blocks, not entry
# points, so they stay out of every module's `just --list`.

root := `git rev-parse --show-toplevel`

# Build a wasm32-unknown-unknown crate and wrap it into a component at
# `{{root}}/target/components/<crate>.wasm` (cargo's module name is the
# crate name with dashes as underscores). The write goes through a temp
# file: `-o` truncates in place, so a direct write would expose an empty or
# partial component to a concurrent reader.
[private]
build-component crate:
    cargo build --release -p {{crate}} --target wasm32-unknown-unknown
    mkdir -p {{root}}/target/components
    wasm-tools component new \
        {{root}}/target/wasm32-unknown-unknown/release/{{replace(crate, "-", "_")}}.wasm \
        -o {{root}}/target/components/{{crate}}.wasm.tmp
    mv -f {{root}}/target/components/{{crate}}.wasm.tmp {{root}}/target/components/{{crate}}.wasm

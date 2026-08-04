//! End-to-end demo test: the built echo-demo component round-trips
//! messages through the wasmtime host against the in-process echo server.
//!
//! The component is built by `just demo::build-component`; when it has not
//! been built, the test states so and skips rather than failing `cargo
//! test` runs that never built wasm artifacts.

#[tokio::test(flavor = "multi_thread")]
async fn echo_demo_round_trips() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/components/echo-demo.wasm"
    );
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: {path} not built (run `just demo::build-component`)");
        return;
    }
    let received = wasmtime_demo::run_demo(path, 25).await.expect("demo runs");
    assert_eq!(received, 25);
}

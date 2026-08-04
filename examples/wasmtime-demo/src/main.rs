//! CLI entry: `wasmtime-demo <component.wasm> [count]`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let component = args
        .next()
        .unwrap_or_else(|| "target/components/echo-demo.wasm".to_string());
    let count: u32 = args.next().map(|v| v.parse()).transpose()?.unwrap_or(100);
    let received = wasmtime_demo::run_demo(&component, count).await?;
    println!("round-tripped {received}/{count} messages");
    Ok(())
}

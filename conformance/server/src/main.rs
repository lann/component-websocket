//! The standalone `conformance-echod` binary, for adapters that cannot embed
//! the server in-process (the JS adapters). Prints one `LISTENING <url>`
//! line once bound; runs until killed.

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--addr needs a value"))?;
                addr = value.parse()?;
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let server = conformance_echod::spawn(addr).await?;
    // The LISTENING line is the startup contract adapters scrape; the
    // first token stays the ws: base URL, the second is the wss: one
    // (terminated with the committed test PKI — see PROTOCOL.md).
    println!("LISTENING {} {}", server.base_url(), server.tls_base_url());
    std::future::pending::<()>().await;
    Ok(())
}

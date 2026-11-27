use std::net::SocketAddr;
use std::path::PathBuf;

use pig_relay::{RelayServerConfig, serve_with_ready};
use tokio::sync::oneshot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bind: SocketAddr = "0.0.0.0:8787".parse()?;
    let mut data_dir = pig_utils::data_dir().join("relay");
    let mut public_url: Option<String> = None;
    let mut allow_registration = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                if let Some(value) = args.next() {
                    bind = value.parse()?;
                }
            }
            "--data-dir" => {
                if let Some(value) = args.next() {
                    data_dir = PathBuf::from(value);
                }
            }
            "--public-url" => {
                public_url = args.next();
            }
            "--no-register" => {
                allow_registration = false;
            }
            "--help" | "-h" => {
                println!(
                    "pig-relay — zero-retention account & remote-control relay for Pig Code\n\n\
                     USAGE:\n  pig-relay [OPTIONS]\n\n\
                     OPTIONS:\n  \
                       --bind <ADDR>         Listen address (default: 0.0.0.0:8787)\n  \
                       --data-dir <DIR>      SQLite metadata directory (default: ~/.pigcode/relay)\n  \
                       --public-url <URL>    Public base URL for device-code verification links\n  \
                       --no-register         Disable new account registration\n  \
                       -h, --help            Print this help message"
                );
                return Ok(());
            }
            other => {
                if let Some(value) = other.strip_prefix("--bind=") {
                    bind = value.parse()?;
                } else if let Some(value) = other.strip_prefix("--data-dir=") {
                    data_dir = PathBuf::from(value);
                } else if let Some(value) = other.strip_prefix("--public-url=") {
                    public_url = Some(value.to_string());
                } else {
                    eprintln!("unknown argument: {other}");
                    std::process::exit(2);
                }
            }
        }
    }

    let (ready_tx, ready_rx) = oneshot::channel();
    let config = RelayServerConfig {
        bind,
        data_dir,
        public_url,
        allow_registration,
    };
    let server = tokio::spawn(serve_with_ready(config, Some(ready_tx)));
    if let Ok(addr) = ready_rx.await {
        println!("pig-relay listening on http://{addr}");
    }
    match server.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(error.into()),
    }
}

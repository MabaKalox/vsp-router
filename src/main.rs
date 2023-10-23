mod cli;

use crate::cli::Cli;

#[cfg(unix)]
use vsp_router::create_virtual_serial_port;

use vsp_router::{handle_receive, handle_send, open_physical_serial_port, Sink};

use clap::Parser;
use futures_util::future::{try_join_all, AbortHandle, Abortable, Aborted};
use tokio::sync::broadcast;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use std::collections::HashMap;
use std::sync::Arc;

type AppError = anyhow::Error;
type AppResult<T> = anyhow::Result<T>;

#[tokio::main]
async fn main() -> AppResult<()> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();
    args.validate()?;
    // TODO: Warn on non-routed sources

    let mut sources = HashMap::new();
    let mut sinks = HashMap::new();
    #[cfg(unix)]
    let mut links = Vec::new();

    #[cfg(unix)]
    for virtual_ in args.virtuals {
        let (port, link) = create_virtual_serial_port(&virtual_.path)?;
        let (reader, writer) = tokio::io::split(port);
        sources.insert(virtual_.id.clone(), ReaderStream::new(reader));
        sinks.insert(virtual_.id.clone(), Arc::new(Sink::new(writer)));
        links.push(link);
    }

    for physical in args.physicals {
        let port = open_physical_serial_port(&physical.path, physical.baud_rate)?;
        let (reader, writer) = tokio::io::split(port);
        sources.insert(physical.id.clone(), ReaderStream::new(reader));
        sinks.insert(physical.id.clone(), Arc::new(Sink::new(writer)));
    }

    let mut transfer_tasks = Vec::new();
    let sources: HashMap<_, _> = sources
        .into_iter()
        .map(|(k, r)| {
            let sender = broadcast::channel(1024).0;
            transfer_tasks.push(handle_receive(r, sender.clone()));
            (k, sender)
        })
        .collect();
    for route in args.routes {
        let receiver = sources.get(&route.src).unwrap().subscribe();
        transfer_tasks.push(handle_send(
            receiver,
            sinks.get(&route.dst).unwrap().clone(),
        ));
    }

    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => info!("received ctrl-c, shutting down"),
            Err(e) => error!(?e, "unable to listen for shutdown signal"),
        }

        abort_handle.abort();
        info!("waiting for graceful shutdown");
    });

    let abort_result = Abortable::new(try_join_all(transfer_tasks), abort_registration).await;
    match abort_result {
        Ok(transfer_result) => {
            let _ = transfer_result?;
        }
        Err(Aborted) => {}
    }

    Ok(())
}

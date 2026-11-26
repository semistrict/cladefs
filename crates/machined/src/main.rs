// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined, the multi-VM server (crates/machined/README.md): serves its API until SIGTERM or
//! SIGINT, then stops every machine and exits.
//!
//!   machined --images DIR --state DIR [--socket PATH] [--tcp ADDR --tls-cert PEM --tls-key PEM
//!            --tls-client-ca PEM] [--hotbed-controller URL --hotbed-token-file PATH
//!            --hotbed-node NAME --hotbed-kernel KERNEL [--hotbed-initramfs ...]]

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
mod run {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use clap::Parser;
    use machined::hotbed::Hotbed;
    use machined::{Config, Tcp};

    /// machined: many libmachine machines in one process, managed over gRPC.
    #[derive(Parser)]
    #[command(version)]
    struct Flags {
        /// The Unix socket to serve on: made mode 0600, machined's user's alone.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// The TCP address to serve on, with mutual TLS (the --tls flags).
        #[arg(long, requires_all = ["tls_cert", "tls_key", "tls_client_ca"])]
        tcp: Option<SocketAddr>,
        /// machined's TLS certificate (PEM), for --tcp.
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        /// Its private key (PEM).
        #[arg(long)]
        tls_key: Option<PathBuf>,
        /// The CA (PEM) whose certificates TCP's callers must present.
        #[arg(long)]
        tls_client_ca: Option<PathBuf>,
        /// The directory machines' kernels and initramfses are named in.
        #[arg(long)]
        images: PathBuf,
        /// machined's own directory (vsock sockets, snapshots, suspensions): on disk, short.
        #[arg(long)]
        state: PathBuf,
        /// Be a node of the hotbed controller at this URL (https://...): its machines then
        /// hotbed's workspaces, as the --hotbed flags say.
        #[arg(long, requires_all = ["hotbed_token_file", "hotbed_node", "hotbed_kernel"])]
        hotbed_controller: Option<String>,
        /// A file holding the token the node joins with: hotbed's join token (hbj_...), or a
        /// node's worker token (hbw_...). Read until machined has joined.
        #[arg(long)]
        hotbed_token_file: Option<PathBuf>,
        /// The node's name in hotbed (machined1).
        #[arg(long)]
        hotbed_node: Option<String>,
        /// Each workspace's machine's kernel, in --images.
        #[arg(long)]
        hotbed_kernel: Option<String>,
        /// Its initramfs, in --images: one whose init runs libmachine's agent.
        #[arg(long, default_value = "")]
        hotbed_initramfs: String,
        /// Its kernel command line.
        #[arg(long, default_value = "libmachine_action=agent")]
        hotbed_command_line: String,
        /// Its vCPUs.
        #[arg(long, default_value_t = 2)]
        hotbed_vcpus: u32,
        /// Its memory, in MiB.
        #[arg(long, default_value_t = 1024)]
        hotbed_memory_mib: u64,
    }

    /// A logger writing to standard error: what machined says, at `RUST_LOG`'s level (`info`
    /// unless it says `debug`, `warn`, ...).
    struct Stderr;

    impl log::Log for Stderr {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{} {}", record.level(), record.args());
            }
        }

        fn flush(&self) {}
    }

    pub(crate) fn main() -> std::io::Result<()> {
        let flags = Flags::parse();
        let level = std::env::var("RUST_LOG")
            .ok()
            .and_then(|level| level.parse().ok())
            .unwrap_or(log::LevelFilter::Info);
        log::set_logger(&Stderr).map_err(std::io::Error::other)?;
        log::set_max_level(level);
        let tcp = match (flags.tcp, flags.tls_cert, flags.tls_key, flags.tls_client_ca) {
            (Some(address), Some(cert), Some(key), Some(client_ca)) => {
                Some(Tcp { address, cert, key, client_ca })
            }
            _ => None,
        };
        let hotbed = match (flags.hotbed_controller, flags.hotbed_token_file, flags.hotbed_node) {
            (Some(controller), Some(token_file), Some(node)) => {
                let workspace = machined::proto::MachineSpec {
                    vcpus: flags.hotbed_vcpus,
                    memory_bytes: flags.hotbed_memory_mib.saturating_mul(1 << 20),
                    boot: Some(machined::proto::Boot {
                        kernel: flags.hotbed_kernel.unwrap_or_default(),
                        initramfs: flags.hotbed_initramfs,
                        command_line: flags.hotbed_command_line,
                    }),
                    ..Default::default()
                };
                Some(Hotbed::new(controller, token_file, node, workspace))
            }
            _ => None,
        };
        let config =
            Config { socket: flags.socket, tcp, images: flags.images, state: flags.state, hotbed };
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
            let server = machined::start(config).await?;
            if let Some(socket) = server.socket() {
                log::info!("machined: serving on {}", socket.display());
            }
            if let Some(tcp) = server.tcp_address() {
                log::info!("machined: serving on {tcp}, with mutual TLS");
            }
            use tokio::signal::unix::{SignalKind, signal};
            let (mut term, mut int) =
                (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?);
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            log::info!("machined: stopping every machine");
            server.shutdown().await
        })
    }
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
fn main() -> std::io::Result<()> {
    run::main()
}

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("machined runs on x86-64 Linux (KVM) and arm64 macOS (Hypervisor.framework)");
    std::process::exit(1);
}

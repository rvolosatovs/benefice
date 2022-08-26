// SPDX-FileCopyrightText: 2022 Profian Inc. <opensource@profian.com>
// SPDX-License-Identifier: AGPL-3.0-only

use super::Workload;

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::net::Ipv4Addr;
use std::ops::Range;
use std::process::Stdio;
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{anyhow, Context};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::future::{AbortHandle, Abortable};
use rand::RngCore;
use tokio::process::{Child, Command};
use tracing::{debug, error};

/// Next free IP LSB.
static IP_LSB: AtomicU16 = AtomicU16::new(0);

#[derive(Debug)]
pub(crate) struct Job {
    destructor: AbortHandle,
    workload: Workload,

    pub(crate) id: String,
    pub(crate) exec: Child,
    pub(crate) mapped_ports: HashMap<u16, u16>,
}

#[cfg(target_os = "linux")]
async fn used_ports<T: FromIterator<u16>>(ss: impl AsRef<OsStr>) -> anyhow::Result<T> {
    use std::io::{BufRead, BufReader};
    use std::net::SocketAddr;

    let out = Command::new(ss)
        .arg("-ltnH")
        .output()
        .await
        .context("failed to run `ss`")?;
    BufReader::new(out.stdout.as_slice())
        .lines()
        .map(|s| {
            s.context("failed to read line")?
                .split_whitespace()
                .nth(3)
                .ok_or_else(|| anyhow!("address column missing"))?
                .replace('*', &Ipv4Addr::UNSPECIFIED.to_string())
                .parse()
                .context("failed to parse socket address")
                .map(|addr| match addr {
                    SocketAddr::V4(addr) => addr.port(),
                    SocketAddr::V6(addr) => addr.port(),
                })
        })
        .collect()
}

impl Job {
    /// Spawns a new job via selected OCI engine, it is not safe for concurrent use.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn(
        id: String,
        workload: Workload,
        ip_command: impl AsRef<OsStr>,
        iptables_command: impl AsRef<OsStr>,
        ss_command: impl AsRef<OsStr>,
        enarx_command: impl AsRef<OsStr>,
        port_range: Range<u16>,
        ports: impl IntoIterator<Item = u16>,
        net_device: String,
        devices: impl IntoIterator<Item = impl AsRef<OsStr>>,
        destructor: impl Future<Output = ()> + Send + 'static,
    ) -> Result<Self, Response> {
        debug!("spawning a job. id={id} workload={:?}", workload);

        let ports: Vec<_> = ports.into_iter().collect();
        let port_count = ports.len();
        let mapped_ports = if port_count > 0 {
            let used: HashSet<_> = used_ports(ss_command).await.map_err(|e| {
                error!("failed to lookup used ports: {e}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;
            let start = port_range.start
                + (rand::thread_rng().next_u32() as usize % port_range.len()) as u16;
            let mapped: HashMap<_, _> = (start..port_range.end)
                .chain(port_range.start..start)
                .into_iter()
                .filter(|p| !used.contains(p))
                .zip(ports)
                .collect();
            if mapped.len() < port_count {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Insufficient amount of open ports on the system, try again later",
                )
                    .into_response());
            }

            let ip_command = ip_command
                .as_ref()
                .to_str()
                .ok_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;

            let host_iface = format!("{id}-host");
            let guest_iface = format!("{id}-guest");

            _ = Command::new(ip_command)
                .args(["netns", "add", &id])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to create a network namespace");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args([
                    "link",
                    "add",
                    &host_iface,
                    "type",
                    "veth",
                    "peer",
                    "name",
                    &guest_iface,
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to create a network device");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args(["link", "set", &guest_iface, "netns", &id])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to move network device");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            let lsb = IP_LSB.fetch_add(2, Ordering::SeqCst);
            let host_ip = Ipv4Addr::new(192, 168, lsb >> 8 as _, lsb as _).to_string();
            let guest_ip = Ipv4Addr::new(192, 168, lsb >> 8 as _, lsb as _ + 1).to_string();

            _ = Command::new(ip_command)
                .args(["addr", "add", &format!("{host_ip}/24"), "dev", &host_iface])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to assign host device IP");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args(["link", "set", "dev", &host_iface, "up"])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to enable host network device");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args(["netns", &id, ip_command, "link", "set", "dev", "lo", "up"])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to enable guest localhost device");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args([
                    "netns",
                    &id,
                    ip_command,
                    "addr",
                    "add",
                    &guest_ip,
                    "dev",
                    &guest_iface,
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to assign guest device IP");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args(["netns", &id, ip_command, "link", "set", &guest_iface, "up"])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to enable guest network device");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(ip_command)
                .args([
                    "netns", &id, ip_command, "route", "add", "default", "via", &host_ip,
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to set default guest gateway");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(iptables_command)
                .args([
                    //iptables -t nat -A POSTROUTING -s 192.168.0.0/255.255.255.0 -o ens5 -j MASQUERADE
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-s",
                    "127.0.0.1",
                    "-j",
                    "MASQUERADE",
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to set masquerade rule");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(iptables_command)
                .args([
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "!",
                    "-s",
                    "127.0.0.1",
                    "-o",
                    &net_device,
                    "-j",
                    "MASQUERADE",
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to set masquerade rule");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(iptables_command)
                .args([
                    "-t",
                    "nat",
                    "-A",
                    "FORWARD",
                    "-i",
                    &guest_iface,
                    "-o",
                    &host_iface,
                    "-j",
                    "ACCEPT",
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to set guest->host forwarding rule");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            _ = Command::new(iptables_command)
                .args([
                    "-t",
                    "nat",
                    "-A",
                    "FORWARD",
                    "-i",
                    &host_iface,
                    "-o",
                    &guest_iface,
                    "-j",
                    "ACCEPT",
                ])
                .output()
                .await
                .map_err(|e| {
                    error!("failed to set host->guest forwarding rule");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                });

            for (host, guest) in &mapped {
                _ = Command::new(iptables_command)
                    .args([
                        "-t",
                        "nat",
                        "-A",
                        "PREROUTING",
                        "-p",
                        "tcp",
                        "-i",
                        &host_iface,
                        "--dport",
                        &host.to_string(),
                        "-j",
                        "DNAT",
                        "--to-destination",
                        &format!("{guest_ip}:{guest}"),
                    ])
                    .output()
                    .await
                    .map_err(|e| StatusCode::INTERNAL_SERVER_ERROR.into_response());

                _ = Command::new(iptables_command)
                    .args([
                        "-A",
                        "FORWARD",
                        "-p",
                        "tcp",
                        "-d",
                        &guest_ip,
                        "--dport",
                        &guest.to_string(),
                        "-m",
                        "state",
                        "--state",
                        "NEW,ESTABLISHED,RELATED",
                        "-j",
                        "ACCEPT",
                    ])
                    .output()
                    .await
                    .map_err(|e| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }

            // ip netns add {id}
            // ip link add {id}-host type veth peer name {id}-guest
            // ip link set {id}-guest netns {id}
            // ip addr add 192.168.1.1/24 dev {id}-host
            // ip link set dev {id}-host up
            // iptables -t nat -A POSTROUTING -s 192.168.1.0/255.255.255.0 -o {net-device} -j MASQUERADE
            // iptables -A FORWARD -i {net-device} -o {id}-host -j ACCEPT
            // iptables -A FORWARD -o {net-device} -i {id}-host -j ACCEPT
            // iptables -t nat -A PREROUTING -p tcp -i {net-device} --dport 6001 -j DNAT --to-destination 192.168.1.2:8080
            // iptables -A FORWARD -p tcp -d 192.168.1.2 --dport 8080 -m state --state NEW,ESTABLISHED,RELATED -j ACCEPT
            //
            // ip netns exec netns2 /bin/bash
            // ip link set dev lo up
            // ip addr add 192.168.1.2/24 dev veth3
            // ip link set dev veth3 up
            // ip route add default via 192.168.1.1

            mapped
        } else {
            Default::default()
        };

        let mut cmd = Command::new(enarx_command);
        let cmd = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let cmd = match &workload {
            Workload::Drawbridge { slug } => cmd.args(["deploy", slug.as_str()]),
            Workload::Upload { wasm, conf } => {
                cmd.args(["run", "--wasmcfgfile", "/app/Enarx.toml", "/app/main.wasm"])
            }
        };
        debug!("spawning a job run command. cmd={:?}", cmd);
        let exec = cmd.spawn().map_err(|e| {
            error!("failed to start job: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

        let (destructor_tx, destructor_rx) = AbortHandle::new_pair();
        _ = tokio::spawn(Abortable::new(destructor, destructor_rx));
        Ok(Self {
            id,
            exec,
            mapped_ports,
            workload,
            destructor: destructor_tx,
        })
    }

    pub(crate) async fn kill(mut self) {
        self.destructor.abort();
        if let Err(e) = self.exec.kill().await {
            error!("failed to kill job: {e} job_id={}", self.id);
        }
        if let Workload::Upload { wasm, conf } = self.workload {
            debug!("closing `main.wasm`");
            if let Err(e) = wasm.close() {
                error!("failed to close `main.wasm`: {e}. job_id={}", self.id);
            };
            debug!("closing `Enarx.toml`");
            if let Err(e) = conf.close() {
                error!("failed to close `Enarx.toml`: {e}. job_id={}", self.id);
            };
        }
    }
}

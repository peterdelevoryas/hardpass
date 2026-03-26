use std::collections::BTreeSet;

use anyhow::{Result, bail};
use tokio::net::TcpListener;

use crate::state::PortForward;

#[derive(Debug)]
pub struct PortReservation {
    pub ssh_port: u16,
    _listeners: Vec<TcpListener>,
}

pub async fn reserve_ports(
    requested_forwards: &[PortForward],
    occupied: &BTreeSet<u16>,
) -> Result<PortReservation> {
    let mut listeners = Vec::new();
    for forward in requested_forwards {
        if occupied.contains(&forward.host) {
            bail!(
                "host port {} is already reserved by another Hardpass VM",
                forward.host
            );
        }
        let listener = TcpListener::bind(("127.0.0.1", forward.host))
            .await
            .map_err(|err| anyhow::anyhow!("host port {} is unavailable: {err}", forward.host))?;
        listeners.push(listener);
    }

    let excluded = requested_forwards
        .iter()
        .map(|forward| forward.host)
        .collect::<BTreeSet<_>>();
    let ssh_port = loop {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        if excluded.contains(&port) || occupied.contains(&port) {
            drop(listener);
            continue;
        }
        listeners.push(listener);
        break port;
    };

    Ok(PortReservation {
        ssh_port,
        _listeners: listeners,
    })
}

pub fn validate_forwards(forwards: &[PortForward], ssh_port: u16) -> Result<()> {
    let mut seen = BTreeSet::from([ssh_port]);
    for forward in forwards {
        if !seen.insert(forward.host) {
            bail!("duplicate host port requested: {}", forward.host);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{reserve_ports, validate_forwards};
    use crate::state::PortForward;

    #[test]
    fn duplicate_host_ports_fail() {
        let err = validate_forwards(
            &[
                PortForward {
                    host: 8080,
                    guest: 8080,
                },
                PortForward {
                    host: 8080,
                    guest: 9000,
                },
            ],
            2222,
        )
        .expect_err("should fail");
        assert!(err.to_string().contains("duplicate host port"));
    }

    #[tokio::test]
    async fn reserve_ports_rejects_occupied_host_forward() {
        let err = reserve_ports(
            &[PortForward {
                host: 8080,
                guest: 8080,
            }],
            &BTreeSet::from([8080]),
        )
        .await
        .expect_err("should fail");
        assert!(err.to_string().contains("already reserved"));
    }
}

use error::Error;
use futures::{self, stream, Future, Stream};
use io::NetSyncIo;
use parking_lot::Mutex;
use primitives::traits::GenericResult;
use protocol::{self, Protocol, ProtocolConfig, Transaction};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use substrate_network_libp2p::{
    start_service, NetworkConfiguration, ProtocolId, RegisteredProtocol, Service as NetworkService,
    ServiceEvent,
};
use tokio::timer::Interval;

const TICK_TIMEOUT: Duration = Duration::from_millis(1000);

#[allow(dead_code)]
pub struct Service<T> {
    network: Arc<Mutex<NetworkService>>,
    protocol: Arc<Protocol<T>>,
}

impl<T: Transaction> Service<T> {
    pub fn new(
        config: ProtocolConfig,
        net_config: NetworkConfiguration,
        protocol_id: ProtocolId,
        tx_callback: fn(T) -> GenericResult,
    ) -> Result<(Service<T>, impl Future<Item = (), Error = ()>), Error> {
        let version = [protocol::CURRENT_VERSION as u8];
        let registered = RegisteredProtocol::new(protocol_id, &version);
        let protocol = Arc::new(Protocol::new(config, tx_callback));
        let service = match start_service(net_config, Some(registered)) {
            Ok(s) => Arc::new(Mutex::new(s)),
            Err(e) => return Err(e.into()),
        };
        let task = service_task(service.clone(), protocol.clone(), protocol_id).map_err(|e| {
            debug!(target: "sub-libp2p", "service error: {:?}", e);
        });
        Ok((
            Service {
                network: service,
                protocol,
            },
            task,
        ))
    }
}

pub fn service_task<T: Transaction>(
    network_service: Arc<Mutex<NetworkService>>,
    protocol: Arc<Protocol<T>>,
    protocol_id: ProtocolId,
) -> impl Future<Item = (), Error = io::Error> {
    // Interval for performing maintenance on the protocol handler.
    let timer = Interval::new_interval(TICK_TIMEOUT)
        .for_each({
            let protocol = protocol.clone();
            let network_service = network_service.clone();
            move |_| {
                protocol.maintain_peers(&mut NetSyncIo::new(&network_service, protocol_id));
                Ok(())
            }
        }).then(|res| {
            match res {
                Ok(()) => (),
                Err(err) => error!("Error in the propagation timer: {:?}", err),
            };
            Ok(())
        });
    let network_service1 = network_service.clone();
    let network = stream::poll_fn(move || network_service1.lock().poll()).for_each(move |event| {
        let mut net_sync = NetSyncIo::new(&network_service, protocol_id);
        debug!(target: "sub-libp2p", "event: {:?}", event);
        match event {
            ServiceEvent::CustomMessage {
                node_index, data, ..
            } => {
                protocol.on_message(&mut net_sync, node_index, &data);
            }
            ServiceEvent::OpenedCustomProtocol { node_index, .. } => {
                protocol.on_peer_connected(&mut net_sync, node_index);
            }
            ServiceEvent::ClosedCustomProtocol { node_index, .. } => {
                protocol.on_peer_disconnected(node_index);
            }
            _ => {
                debug!("TODO");
                ()
            }
        };
        Ok(())
    });
    let futures: Vec<Box<Future<Item = (), Error = io::Error> + Send>> =
        vec![Box::new(timer), Box::new(network)];
    futures::select_all(futures)
        .and_then(move |_| {
            info!("Networking ended");
            Ok(())
        }).map_err(|(r, _, _)| r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;
    use std::thread;
    use std::time;
    use test_utils::*;

    fn create_services<T: Transaction>(
        num_services: u32,
    ) -> Vec<(Service<T>, impl Future<Item = (), Error = ()>)> {
        let base_address = "/ip4/127.0.0.1/tcp/".to_string();
        let base_port = rand::thread_rng().gen_range(30000, 60000);
        let mut addresses = Vec::new();
        for i in 0..num_services {
            let port = base_port + i;
            addresses.push(base_address.clone() + &port.to_string());
        }
        // spin up a root service that does not have bootnodes and
        // have other services have this service as their boot node
        // may want to abstract this out to enable different configurations
        let secret = create_secret();
        let root_config = test_config_with_secret(&addresses[0], vec![], secret);
        let tx_callback = |_| Ok(());
        let root_service = Service::new(
            ProtocolConfig::default(),
            root_config,
            ProtocolId::default(),
            tx_callback,
        ).unwrap();
        let boot_node = addresses[0].clone() + "/p2p/" + &raw_key_to_peer_id_str(secret);
        let mut services = vec![root_service];
        for i in 1..num_services {
            let config = test_config(&addresses[i as usize], vec![boot_node.clone()]);
            let service = Service::new(
                ProtocolConfig::default(),
                config,
                ProtocolId::default(),
                tx_callback,
            ).unwrap();
            services.push(service);
        }
        services
    }

    #[test]
    fn test_send_message() {
        let services = create_services(2);
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        let (services, tasks): (Vec<_>, Vec<_>) = services.into_iter().unzip();
        for task in tasks {
            runtime.spawn(task);
        }
        thread::sleep(time::Duration::from_millis(1000));
        for service in services {
            for peer in service.protocol.sample_peers(1) {
                let message = fake_tx_message();
                let mut net_sync = NetSyncIo::new(&service.network, ProtocolId::default());
                service.protocol.send_message(&mut net_sync, peer, &message);
            }
        }
        thread::sleep(time::Duration::from_millis(1000));
    }
}
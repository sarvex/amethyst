// CLIENT
use std::time::Duration;

use amethyst::{
    core::{ecs::SystemBundle, frame_limiter::FrameRateLimitStrategy, Time},
    network::simulation::{
        tcp::TcpNetworkBundle, NetworkSimulationEvent, NetworkSimulationTime, TransportResource,
    },
    prelude::*,
    shrev::{EventChannel, ReaderId},
    utils::application_root_dir,
    Result,
};
use log::{error, info};
use systems::ParallelRunnable;

fn main() -> Result<()> {
    amethyst::start_logger(Default::default());

    let assets_dir = application_root_dir()?.join("examples/net_client/");

    let mut game_data = DispatcherBuilder::default();
    game_data
        .add_bundle(TcpNetworkBundle::new(None, 2048))
        .add_bundle(SpamBundle);

    let mut game = Application::build(assets_dir, GameState)?
        .with_frame_limit(
            FrameRateLimitStrategy::SleepAndYield(Duration::from_millis(2)),
            144,
        )
        .build(game_data)?;
    game.run();
    Ok(())
}

pub struct GameState;
impl SimpleState for GameState {}

#[derive(Debug)]
struct SpamBundle;

impl SystemBundle for SpamBundle {
    fn load(
        &mut self,
        _world: &mut World,
        resources: &mut Resources,
        builder: &mut DispatcherBuilder,
    ) -> Result<()> {
        let mut chan = EventChannel::<NetworkSimulationEvent>::default();
        let reader = chan.register_reader();
        resources.insert(chan);

        resources.insert(TransportResource::default());
        resources.insert(NetworkSimulationTime::default());

        builder.add_system(build_network_simulation_system(reader));

        Ok(())
    }
}

fn build_network_simulation_system(
    mut reader: ReaderId<NetworkSimulationEvent>,
) -> impl ParallelRunnable {
    SystemBuilder::new("TransformSystem")
        .read_resource::<NetworkSimulationTime>()
        .read_resource::<Time>()
        .read_resource::<EventChannel<NetworkSimulationEvent>>()
        .write_resource::<TransportResource>()
        .build(move |_commands, _, (sim_time, time, event, net), _| {
            let server_addr = "127.0.0.1:3457".parse().unwrap();
            for frame in sim_time.sim_frames_to_run() {
                info!("Sending message for sim frame {}.", frame);
                let payload = format!(
                    "CL: sim_frame:{},abs_time:{}",
                    frame,
                    time.absolute_time_seconds()
                );
                net.send(server_addr, payload.as_bytes());
            }

            for event in event.read(&mut reader) {
                match event {
                    NetworkSimulationEvent::Message(_addr, payload) => {
                        info!("Payload: {:?}", payload)
                    }
                    NetworkSimulationEvent::Connect(addr) => {
                        info!("New client connection: {}", addr)
                    }
                    NetworkSimulationEvent::Disconnect(addr) => {
                        info!("Server Disconnected: {}", addr)
                    }
                    NetworkSimulationEvent::RecvError(e) => {
                        error!("Recv Error: {:?}", e);
                    }
                    NetworkSimulationEvent::SendError(e, msg) => {
                        error!("Send Error: {:?}, {:?}", e, msg);
                    }
                    _ => {}
                }
            }
        })
}

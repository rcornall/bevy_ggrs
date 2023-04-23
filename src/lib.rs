//! bevy_ggrs is a bevy plugin for the P2P rollback networking library GGRS.
#![forbid(unsafe_code)] // let us try

use crate::{world_snapshot::WorldSnapshot};
use ggrs::{
    GGRSError, GGRSRequest, GameStateCell, SessionState,
};
use instant::{Duration, Instant};

use bevy::{
    ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel},
    prelude::*,
    reflect::{FromType, GetTypeRegistration, TypeRegistry, TypeRegistryInternal},
};
use ggrs::{Config, InputStatus, P2PSession, PlayerHandle, SpectatorSession, SyncTestSession};
// use ggrs_stage::GGRSStage;
use parking_lot::RwLock;
use std::sync::Arc;

pub use ggrs;

pub(crate) mod ggrs_stage;
pub(crate) mod world_snapshot;

const DEFAULT_FPS: usize = 60;

#[derive(ScheduleLabel, Debug, Hash, PartialEq, Eq, Clone)]
pub struct GGRSSchedule;

/// Defines the Session that the GGRS Plugin should expect as a resource.
#[derive(Resource)]
pub enum Session<T: Config> {
    SyncTestSession(SyncTestSession<T>),
    P2PSession(P2PSession<T>),
    SpectatorSession(SpectatorSession<T>),
}

// TODO: more specific name to avoid conflicts?
#[derive(Resource, Deref, DerefMut)]
pub struct PlayerInputs<T: Config>(Vec<(T::Input, InputStatus)>);

#[derive(Resource, Deref, DerefMut)]
pub struct RollbackFrameType {
    rolled: bool,
}

/// Add this component to all entities you want to be loaded/saved on rollback.
/// The `id` has to be unique. Consider using the `RollbackIdProvider` resource.
#[derive(Component)]
pub struct Rollback {
    id: u32,
}

impl Rollback {
    /// Creates a new rollback tag with the given id.
    pub fn new(id: u32) -> Self {
        Self { id }
    }

    /// Returns the rollback id.
    pub const fn id(&self) -> u32 {
        self.id
    }
}

/// Provides unique ids for your Rollback components.
/// When you add the GGRS Plugin, this should be available as a resource.
#[derive(Resource, Default)]
pub struct RollbackIdProvider {
    next_id: u32,
}

impl RollbackIdProvider {
    /// Returns an unused, unique id.
    pub fn next_id(&mut self) -> u32 {
        if self.next_id == u32::MAX {
            // TODO: do something smart?
            panic!("RollbackIdProvider: u32::MAX has been reached.");
        }
        let ret = self.next_id;
        self.next_id += 1;
        ret
    }

    /// Returns a `Rollback` component with the next unused id
    ///
    /// Convenience for `Rollback::new(rollback_id_provider.next_id())`.
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// use bevy_ggrs::{RollbackIdProvider};
    ///
    /// fn system_in_rollback_schedule(mut commands: Commands, mut rip: RollbackIdProvider) {
    ///     commands.spawn((
    ///         SpatialBundle::default(),
    ///         rip.next(),
    ///     ));
    /// }
    /// ```
    pub fn next(&mut self) -> Rollback {
        Rollback::new(self.next_id())
    }
}

/// A builder to configure GGRS for a bevy app.
pub struct GGRSPlugin<T: Config + Send + Sync> {
    input_system: Option<Box<dyn System<In = PlayerHandle, Out = T::Input>>>,
    fps: usize,
    type_registry: TypeRegistry,
}

impl<T: Config + Send + Sync> Default for GGRSPlugin<T> {
    fn default() -> Self {
        Self {
            input_system: None,
            fps: DEFAULT_FPS,
            type_registry: TypeRegistry {
                internal: Arc::new(RwLock::new({
                    let mut r = TypeRegistryInternal::empty();
                    // `Parent` and `Children` must be registered so that their `ReflectMapEntities`
                    // data may be used.
                    //
                    // While this is a little bit of a weird spot to register these, are the only
                    // Bevy core types implementing `MapEntities`, so for now it's probably fine to
                    // just manually register these here.
                    //
                    // The user can still register any custom types with `register_rollback_type()`.
                    r.register::<Parent>();
                    r.register::<Children>();
                    r
                })),
            },
        }
    }
}

impl<T: Config + Send + Sync> GGRSPlugin<T> {
    /// Create a new instance of the builder.
    pub fn new() -> Self {
        Default::default()
    }

    /// Change the update frequency of the rollback stage.
    pub fn with_update_frequency(mut self, fps: usize) -> Self {
        self.fps = fps;
        self
    }

    /// Registers a system that takes player handles as input and returns the associated inputs for that player.
    pub fn with_input_system<Params>(
        mut self,
        input_fn: impl IntoSystem<PlayerHandle, T::Input, Params>,
    ) -> Self {
        self.input_system = Some(Box::new(IntoSystem::into_system(input_fn)));
        self
    }

    /// Registers a type of component for saving and loading during rollbacks.
    pub fn register_rollback_component<Type>(self) -> Self
    where
        Type: GetTypeRegistration + Reflect + Default + Component,
    {
        let mut registry = self.type_registry.write();
        registry.register::<Type>();

        let registration = registry.get_mut(std::any::TypeId::of::<Type>()).unwrap();
        registration.insert(<ReflectComponent as FromType<Type>>::from_type());
        drop(registry);
        self
    }

    /// Registers a type of resource for saving and loading during rollbacks.
    pub fn register_rollback_resource<Type>(self) -> Self
    where
        Type: GetTypeRegistration + Reflect + Default + Resource,
    {
        let mut registry = self.type_registry.write();
        registry.register::<Type>();

        let registration = registry.get_mut(std::any::TypeId::of::<Type>()).unwrap();
        registration.insert(<ReflectResource as FromType<Type>>::from_type());
        drop(registry);
        self
    }

    /// Consumes the builder and makes changes on the bevy app according to the settings.
    pub fn build(self, app: &mut App) {
        let mut input_system = self
            .input_system
            .expect("Adding an input system through GGRSBuilder::with_input_system is required");
        // ggrs stage
        input_system.initialize(&mut app.world);
        let mut stage = GGRSStage::<T>::new(input_system);
        stage.set_update_frequency(self.fps);

        let mut schedule = Schedule::default();
        schedule.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            ..default()
        });
        app.add_schedule(GGRSSchedule, schedule);

        stage.set_type_registry(self.type_registry);
        app.add_system(GGRSStage::<T>::run.in_base_set(CoreSet::PreUpdate));
        app.insert_resource(stage);
        // other resources
        app.insert_resource(RollbackIdProvider::default());
    }
}

#[derive(Resource)]
/// The GGRSStage handles updating, saving and loading the game state.
pub struct GGRSStage<T>
where
    T: Config,
{
    /// Used to register all types considered when loading and saving
    pub(crate) type_registry: TypeRegistry,
    /// This system is used to get an encoded representation of the input that GGRS can handle
    pub(crate) input_system: Box<dyn System<In = PlayerHandle, Out = T::Input>>,
    /// Instead of using GGRS's internal storage for encoded save states, we save the world here, avoiding serialization into `Vec<u8>`.
    snapshots: Vec<WorldSnapshot>,
    /// fixed FPS our logic is running with
    update_frequency: usize,
    /// counts the number of frames that have been executed
    frame: i32,
    /// internal time control variables
    last_update: Instant,
    /// accumulated time. once enough time has been accumulated, an update is executed
    accumulator: Duration,
    /// boolean to see if we should run slow to let remote clients catch up
    run_slow: bool,
}

impl<T: Config + Send + Sync> GGRSStage<T> {
    pub(crate) fn run(world: &mut World) {
        let mut stage = world
            .remove_resource::<GGRSStage<T>>()
            .expect("failed to extract ggrs schedule");

        // get delta time from last run() call and accumulate it
        let delta = Instant::now().duration_since(stage.last_update);
        let mut fps_delta = 1. / stage.update_frequency as f64;
        if stage.run_slow {
            fps_delta *= 1.1;
        }
        stage.accumulator = stage.accumulator.saturating_add(delta);
        stage.last_update = Instant::now();

        // no matter what, poll remotes and send responses
        if let Some(mut session) = world.get_resource_mut::<Session<T>>() {
            match &mut *session {
                Session::P2PSession(session) => {
                    session.poll_remote_clients();
                }
                Session::SpectatorSession(session) => {
                    session.poll_remote_clients();
                }
                _ => {}
            }
        }

        // if we accumulated enough time, do steps
        while stage.accumulator.as_secs_f64() > fps_delta {
            // decrease accumulator
            stage.accumulator = stage
                .accumulator
                .saturating_sub(Duration::from_secs_f64(fps_delta));

            // depending on the session type, doing a single update looks a bit different
            let session = world.get_resource::<Session<T>>();
            match session {
                Some(&Session::SyncTestSession(_)) => stage.run_synctest(world),
                Some(&Session::P2PSession(_)) => stage.run_p2p(world),
                Some(&Session::SpectatorSession(_)) => stage.run_spectator(world),
                _ => stage.reset(), // No session has been started yet
            }
        }

        world.insert_resource(stage);
    }
}

impl<T: Config> GGRSStage<T> {
    pub(crate) fn new(input_system: Box<dyn System<In = PlayerHandle, Out = T::Input>>) -> Self {
        Self {
            type_registry: TypeRegistry::default(),
            input_system,
            snapshots: Vec::new(),
            frame: 0,
            update_frequency: 60,
            last_update: Instant::now(),
            accumulator: Duration::ZERO,
            run_slow: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.last_update = Instant::now();
        self.accumulator = Duration::ZERO;
        self.frame = 0;
        self.run_slow = false;
        self.snapshots = Vec::new();
    }

    pub(crate) fn run_synctest(&mut self, world: &mut World) {
        // let ses = world.get_resource::<Session<T>>().expect("lol");
        let Some(Session::SyncTestSession(sess)) = world.get_resource::<Session<T>>() else {
            // TODO: improve error message for new API
            panic!("No GGRS SyncTestSession found. Please start a session and add it as a resource.");
        };

        // if our snapshot vector is not initialized, resize it accordingly
        if self.snapshots.is_empty() {
            for _ in 0..sess.max_prediction() {
                self.snapshots.push(WorldSnapshot::default());
            }
        }

        // get inputs for all players
        let mut inputs = Vec::new();
        for handle in 0..sess.num_players() {
            inputs.push(self.input_system.run(handle, world));
        }

        let mut sess = world.get_resource_mut::<Session<T>>();
        let Some(Session::SyncTestSession(ref mut sess)) = sess.as_deref_mut() else {
            panic!("No GGRS SyncTestSession found. Please start a session and add it as a resource.");
        };
        for (player_handle, &input) in inputs.iter().enumerate() {
            sess.add_local_input(player_handle, input)
                .expect("All handles between 0 and num_players should be valid");
        }
        match sess.advance_frame() {
            Ok(requests) => self.handle_requests(requests, world),
            Err(e) => warn!("{}", e),
        }
    }

    pub(crate) fn run_spectator(&mut self, world: &mut World) {
        // run spectator session, no input necessary
        let mut sess = world.get_resource_mut::<Session<T>>();
        let Some(Session::SpectatorSession(ref mut sess)) = sess.as_deref_mut() else {
            // TODO: improve error message for new API
            panic!("No GGRS P2PSpectatorSession found. Please start a session and add it as a resource.");
        };

        // if session is ready, try to advance the frame
        if sess.current_state() == SessionState::Running {
            match sess.advance_frame() {
                Ok(requests) => self.handle_requests(requests, world),
                Err(GGRSError::PredictionThreshold) => {
                    info!("P2PSpectatorSession: Waiting for input from host.")
                }
                Err(e) => warn!("{}", e),
            };
        }
    }

    pub(crate) fn run_p2p(&mut self, world: &mut World) {
        let sess = world.get_resource::<Session<T>>();
        let Some(Session::P2PSession(ref sess)) = sess else {
            // TODO: improve error message for new API
            panic!("No GGRS P2PSession found. Please start a session and add it as a resource.");
        };

        // if our snapshot vector is not initialized, resize it accordingly
        if self.snapshots.is_empty() {
            // find out what the maximum prediction window is in this synctest
            for _ in 0..sess.max_prediction() {
                self.snapshots.push(WorldSnapshot::default());
            }
        }

        // if we are ahead, run slow
        self.run_slow = sess.frames_ahead() > 0;

        // get local player handles
        let local_handles = sess.local_player_handles();

        // get local player inputs
        let mut local_inputs = Vec::new();
        for &local_handle in &local_handles {
            let input = self.input_system.run(local_handle, world);
            local_inputs.push(input);
        }

        // if session is ready, try to advance the frame
        let mut sess = world.get_resource_mut::<Session<T>>();
        let Some(Session::P2PSession(ref mut sess)) = sess.as_deref_mut() else {
            // TODO: improve error message for new API
            panic!("No GGRS P2PSession found. Please start a session and add it as a resource.");
        };
        if sess.current_state() == SessionState::Running {
            for i in 0..local_inputs.len() {
                sess.add_local_input(local_handles[i], local_inputs[i])
                    .expect("All handles in local_handles should be valid");
            }
            match sess.advance_frame() {
                Ok(requests) => self.handle_requests(requests, world),
                Err(GGRSError::PredictionThreshold) => {
                    info!("Skipping a frame: PredictionThreshold.")
                }
                Err(e) => warn!("{}", e),
            };
        }
    }

    pub(crate) fn handle_requests(&mut self, requests: Vec<GGRSRequest<T>>, world: &mut World) {
        for request in requests {
            match request {
                GGRSRequest::SaveGameState { cell, frame } => self.save_world(cell, frame, world),
                GGRSRequest::LoadGameState { frame, .. } => self.load_world(frame, world),
                GGRSRequest::AdvanceFrame { inputs } => self.advance_frame(inputs, world),
            }
        }
    }

    pub(crate) fn save_world(
        &mut self,
        cell: GameStateCell<T::State>,
        frame: i32,
        world: &mut World,
    ) {
        debug!("saving snapshot for frame {frame}");
        assert_eq!(self.frame, frame);

        // we make a snapshot of our world
        let snapshot = WorldSnapshot::from_world(world, &self.type_registry);

        // we don't really use the buffer provided by GGRS
        cell.save(self.frame, None, Some(snapshot.checksum as u128));

        // store the snapshot ourselves (since the snapshots don't implement clone)
        let pos = frame as usize % self.snapshots.len();
        self.snapshots[pos] = snapshot;
    }

    pub(crate) fn load_world(&mut self, frame: i32, world: &mut World) {
        debug!("restoring snapshot for frame {frame}");
        self.frame = frame;

        // we get the correct snapshot
        let pos = frame as usize % self.snapshots.len();
        let snapshot_to_load = &self.snapshots[pos];

        // load the entities
        snapshot_to_load.write_to_world(world, &self.type_registry);
    }

    pub(crate) fn advance_frame(
        &mut self,
        inputs: Vec<(T::Input, InputStatus)>,
        world: &mut World,
    ) {
        debug!("advancing to frame: {}", self.frame + 1);
        world.insert_resource(PlayerInputs::<T>(inputs));
        // world.insert_resource(RollbackFrameType);
        world.run_schedule(GGRSSchedule);
        world.remove_resource::<PlayerInputs<T>>();
        self.frame += 1;
        debug!("frame {} completed", self.frame);
    }

    pub fn set_update_frequency(&mut self, update_frequency: usize) {
        self.update_frequency = update_frequency
    }

    pub(crate) fn set_type_registry(&mut self, type_registry: TypeRegistry) {
        self.type_registry = type_registry;
    }
}

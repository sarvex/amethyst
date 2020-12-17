use std::{
    hash::Hash,
    time::Duration
};
use amethyst_assets::{AssetStorage, Handle};
use amethyst_core::{ecs::*, timing::secs_to_duration};
use fnv::FnvHashMap;
use log::error;
use minterpolate::InterpolationPrimitive;
#[cfg(feature = "profiler")]
use thread_profiler::profile_scope;

use crate::resources::{
    Animation, AnimationCommand, AnimationControl, AnimationControlSet, AnimationHierarchy,
    AnimationSampling, ControlState, DeferStartRelation, RestState, Sampler, SamplerControl,
    SamplerControlSet, StepDirection,
};

/// System for setting up animations, should run before `SamplerInterpolationSystem`.
///
/// Will process all active `AnimationControl` + `AnimationHierarchy`, and do processing of the
/// animations they describe. If an animation only targets a single node/entity, there is no need
/// for `AnimationHierarchy`.
///
/// ### Type parameters:
///
/// - `I`: identifier type for running animations, only one animation can be run at the same time
///        with the same id
/// - `T`: the component type that the animation should be applied to
pub fn build_animation_control_system<
    I: PartialEq + Eq + Hash + Copy + Send + Sync + 'static,
    T: AnimationSampling + Clone,
>() -> impl systems::ParallelRunnable {
    SystemBuilder::new("AnimationControlSystem")
        .read_resource::<AssetStorage<Animation<T>>>()
        .read_resource::<AssetStorage<Sampler<T::Primitive>>>()
        .read_component::<AnimationHierarchy<T>>()
        .read_component::<T>()
        .write_component::<AnimationControlSet<I, T>>()
        .write_component::<SamplerControlSet<T>>()
        .write_component::<RestState<T>>()
        .with_query(<(Read<Entity>, Write<AnimationControlSet<I, T>>, TryRead<AnimationHierarchy<T>>)>::query())
        .build(|mut buffer,  world, (animation_storage, sampler_storage),  query| {
            let mut remove_sets = Vec::default();
            let mut next_id = 1;

            let (mut query_world, mut world) = world.split_for_query(&query);

            for (entity, control_set, hierarchy) in query.iter_mut(&mut query_world) {
                let mut remove_ids = Vec::default();
                let mut state_set = FnvHashMap::default();

                // process each animation in control set
                for  (ref id, ref mut control) in control_set.animations.iter_mut() {
                    let mut remove = false;
                    if let Some(state) =
                        animation_storage
                            .get(&control.animation)
                            .and_then(|animation| {
                                process_animation_control(
                                    *entity,
                                    &mut world,
                                    animation,
                                    control,
                                    hierarchy,
                                    &*sampler_storage,
                                    buffer,
                                    &mut remove,
                                    &mut next_id,
                                )
                            })
                    {
                        // update the current state of this animation
                        control.state = state;
                    }

                    // update command for next iteration
                    if let AnimationCommand::Step(_) = control.command {
                        control.command = AnimationCommand::Start;
                    }
                    if let AnimationCommand::SetInputValue(_) = control.command {
                        control.command = AnimationCommand::Start;
                    }
                    
                    // remove completed animations
                    if remove {
                        remove_ids.push(*id);
                    } else {
                        // record current position of running animations to know when to trigger deferred
                        state_set.insert(
                            *id,
                            match control.state {
                                ControlState::Running(_) => find_max_duration(
                                    control.id,
                                    world
                                        .entry_ref(
                                            *hierarchy
                                                .and_then(|h| h.nodes.values().next())
                                                .unwrap_or(&entity),
                                        )
                                        .unwrap()
                                        .get_component::<SamplerControlSet<T>>()
                                        .ok(),
                                ),
                                _ => -1.0,
                            }
                                                );
                    }
                }

                // record deferred animation as not started
                for deferred_animation in &control_set.deferred_animations {
                    state_set.insert(deferred_animation.animation_id, -1.0);
                }
                
                let mut deferred_start = Vec::default();

                for deferred_animation in &control_set.deferred_animations {
                    let (start, start_dur) =
                        if let Some(dur) = state_set.get(&deferred_animation.relation.0) {
                            if *dur < 0. {
                                (false, 0.)
                            } else if let DeferStartRelation::Start(start_dur) =
                                deferred_animation.relation.1
                            {
                                let remain_dur = dur - start_dur;
                                (remain_dur >= 0., remain_dur)
                            } else {
                                (false, 0.)
                            }
                        } else {
                            (true, 0.)
                        };
                    if start {
                        deferred_start
                            .push((deferred_animation.animation_id, start_dur));
                        state_set
                            .insert(deferred_animation.animation_id, start_dur);
                    }
                }
                
                for &(id, start_dur) in &deferred_start {
                    let index = control_set
                        .deferred_animations
                        .iter()
                        .position(|a| a.animation_id == id)
                        .expect("Unreachable: Id of current `deferred_start` was taken from previous loop over `deferred_animations`");

                    let mut def = control_set.deferred_animations.remove(index);
                    def.control.state = ControlState::Deferred(secs_to_duration(start_dur));
                    def.control.command = AnimationCommand::Start;
                    let mut remove = false;
                    if let Some(state) =
                        animation_storage
                            .get(&def.control.animation)
                            .and_then(|animation| {
                                process_animation_control(
                                    *entity,
                                    &mut world,
                                    animation,
                                    &mut def.control,
                                    hierarchy,
                                    &*sampler_storage,
                                    &mut buffer,
                                    &mut remove,
                                    &mut next_id,
                                )
                            })
                    {
                        def.control.state = state;
                    }
                    control_set.insert(id, def.control);
                }
                next_id = next_id;
                for id in &remove_ids {
                    control_set.remove(*id);
                    if control_set.is_empty() {
                        remove_sets.push(entity);
                    }
                }
            }
            for entity in remove_sets {
                todo!("remove control set from entity");
            }
             })
}

fn find_max_duration<T>(control_id: u64, samplers: Option<&SamplerControlSet<T>>) -> f32
where
    T: AnimationSampling,
{
    samplers
        .and_then(|set| set.get_running_duration(control_id))
        .unwrap_or(0.)
}

/// Check if the given animation list is for a single node. If so, we don't need an
/// `AnimationHierarchy`.
fn only_one_index<C, P>(nodes: &[(usize, C, Handle<Sampler<P>>)]) -> bool
where
    P: InterpolationPrimitive,
{
    if nodes.is_empty() {
        true
    } else {
        let first = nodes[0].0;
        nodes.iter().all(|&(ref i, _, _)| *i == first)
    }
}

/// Process a single animation control object.
///
/// ## Parameters:
///
/// - `entity`: the entity the control object is active for
/// - `animation`: the animation the control is for
/// - `control`: animation control object
/// - `hierarchy`: the animation node hierarchy for the entity hierarchy the animation instance is
///                active for, if this is None the animation must be for a single node, which is the
///                local entity. If the animation contains more than a single node index, the
///                animation will be silently dropped.
/// - `sampler_storage`: `AssetStorage` for all `Sampler`s
/// - `samplers`: the active sampler sets
/// - `targets`: Target components, used to retrieve the rest pose before animation starts.
/// - `remove`: all entities pushed here will have the control object removed at the end of the system execution
/// - `next_id`: next id to use for the animation control id
///
/// ##
///
/// Optionally returns a new `ControlState` for the animation. This will be the new state of the
/// control object.
fn process_animation_control<T>(
    entity: Entity,
    world: &mut SubWorld,
    animation: &Animation<T>,
    control: &mut AnimationControl<T>,
    hierarchy: Option<&AnimationHierarchy<T>>,
    sampler_storage: &AssetStorage<Sampler<T::Primitive>>,
    buffer: &mut CommandBuffer,
    remove: &mut bool,
    next_id: &mut u64,
) -> Option<ControlState>
where
    T: AnimationSampling + Clone,
{
    // Checking hierarchy
    let h_fallback = AnimationHierarchy::new_single(animation.nodes[0].0, entity);
    let hierarchy = match hierarchy {
        Some(h) => h,
        None => {
            if only_one_index(&animation.nodes) {
                &h_fallback
            } else {
                error!(
                "Animation control which target multiple nodes without a hierarchy detected, dropping"
            );
                *remove = true;
                return None;
            }
        }
    };
    match (&control.state, &control.command) {
        // Check for aborted or done animation
        (_, &AnimationCommand::Abort) | (&ControlState::Abort, _) | (&ControlState::Done, _) => {
            // signal samplers to abort, and remove control object if all samplers are done and removed
            if check_and_terminate_animation(control.id, hierarchy, world, buffer) {
                *remove = true;
            }
            Some(ControlState::Abort)
        }

        // Animation was just requested, start it
        // We ignore the command here because we need the animation to be
        // started before we can pause it, and to avoid a lot of checks for
        // abort. The command will be processed next frame.
        (&ControlState::Requested, &AnimationCommand::Start) => {
            control.id = *next_id;
            *next_id += 1;
            if start_animation(
                animation,
                sampler_storage,
                control,
                world,
                buffer,
                hierarchy,
            ) {
                Some(ControlState::Running(Duration::from_secs(0)))
            } else {
                None // Try again next frame, might just be that samplers haven't finished loading
            }
        }

        (&ControlState::Deferred(..), &AnimationCommand::Start) => {
            control.id = *next_id;
            *next_id += 1;
            if start_animation(
                animation,
                sampler_storage,
                control,
                world,
                buffer,
                hierarchy,
            ) {
                Some(ControlState::Running(Duration::from_secs(0)))
            } else {
                None // Try again next frame, might just be that samplers haven't finished loading
            }
        }

        // If pause was requested on a running animation, pause it
        (&ControlState::Running(..), &AnimationCommand::Pause) => {
            pause_animation(control.id, hierarchy, world);
            Some(ControlState::Paused(Duration::from_secs(0)))
        }

        // If start was requested on a paused animation, unpause it
        (&ControlState::Paused(_), &AnimationCommand::Start) => {
            unpause_animation(control.id, hierarchy, world);
            Some(ControlState::Running(Duration::from_secs(0)))
        }

        (&ControlState::Running(..), &AnimationCommand::Step(ref dir)) => {
            step_animation(control.id, hierarchy, world, sampler_storage, dir);
            None
        }

        (&ControlState::Running(..), &AnimationCommand::SetInputValue(value)) => {
            set_animation_input(control.id, hierarchy, world, value);
            None
        }

        (&ControlState::Running(..), &AnimationCommand::SetBlendWeights(ref weights)) => {
            set_blend_weights(control.id, hierarchy, world, weights);
            None
        }

        // check for finished/aborted animations, wait for samplers to signal done,
        // then remove control objects
        (&ControlState::Running(..), _) => {
            if check_termination(control.id, hierarchy, world) {
                // Do termination
                for node_entity in hierarchy.nodes.values() {
                    let empty = world
                        .entry_mut(entity)
                        .unwrap()
                        .get_component_mut::<SamplerControlSet<T>>()
                        .map(|sampler| {
                            sampler.clear(control.id);
                            sampler.is_empty()
                        })
                        .unwrap_or(false);
                    if empty {
                        buffer.remove_component::<SamplerControlSet<T>>(*node_entity);
                        //samplers.remove(*node_entity);
                    }
                }
                *remove = true;
            } else {
                update_animation_rate(control.id, hierarchy, world, control.rate_multiplier);
            }
            None
        }

        _ => None,
    }
}

/// Process animation creation request.
/// Will build `SamplerControlSet`s for the `AnimationHierarchy` given, based on the `Sampler`s in
/// the given `Animation`.
///
/// ## Parameters
///
/// - `animation`: the animation to start
/// - `sampler_storage`: all samplers
/// - `control`: the control object for the animation instance
/// - `hierarchy`: the animation node hierarchy for the entity hierarchy the animation instance is active for
/// - `samplers`: the active sampler sets
/// - `targets`: Target components, used to retrieve the rest pose before animation starts.
///
/// ## Returns
///
/// True if the animation was started, false if it wasn't.
fn start_animation<T>(
    animation: &Animation<T>,
    sampler_storage: &AssetStorage<Sampler<T::Primitive>>,
    control: &AnimationControl<T>,
    world: &mut SubWorld,
    buffer: &mut CommandBuffer,
    hierarchy: &AnimationHierarchy<T>,
) -> bool
where
    T: AnimationSampling + Clone,
{
    // check that hierarchy is valid, and all samplers exist
    let valid = animation
        .nodes
        .iter()
        .all(|&(ref node_index, _, ref sampler_handle)| {
            hierarchy.nodes.contains_key(node_index)
                && sampler_storage.get(sampler_handle).is_some()
        });

    if !valid {
        return false;
    }

    hierarchy.rest_state(world, buffer);

    let start_state = if let ControlState::Deferred(dur) = control.state {
        ControlState::Deferred(dur)
    } else {
        ControlState::Requested
    };

    // setup sampler tree
    for &(ref node_index, ref channel, ref sampler_handle) in &animation.nodes {
        let node_entity = hierarchy.nodes.get(node_index).expect(
            "Unreachable: Existence of all nodes are checked in validation of hierarchy above",
        );
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(component) = entry
                .get_component::<RestState<T>>()
                .map(RestState::state)
                .or_else(|x| entry.get_component::<T>())
            {
                let sampler_control = SamplerControl::<T> {
                    control_id: control.id,
                    channel: channel.clone(),
                    state: start_state.clone(),
                    sampler: sampler_handle.clone(),
                    end: control.end.clone(),
                    after: component.current_sample(channel),
                    rate_multiplier: control.rate_multiplier,
                    blend_weight: 1.0,
                };
                if let Ok(ref mut set) = entry.get_component_mut::<SamplerControlSet<T>>() {
                    set.add_control(sampler_control);
                } else {
                    let mut set = SamplerControlSet::default();
                    set.add_control(sampler_control);
                    buffer.add_component(*node_entity, set);
                }
            } else {
                error!("Failed to acquire animated component. Is the component you are trying to animate present on the target entity: {:?}", node_entity);
                return false;
            }
        }
    }
    true
}

fn pause_animation<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
) where
    T: AnimationSampling,
{
    for node_entity in hierarchy.nodes.values() {
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                s.pause(control_id);
            }
        }
    }
}

fn unpause_animation<T>(control_id: u64, hierarchy: &AnimationHierarchy<T>, world: &mut SubWorld)
where
    T: AnimationSampling,
{
    for node_entity in hierarchy.nodes.values() {
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                s.unpause(control_id);
            }
        }
    }
}

fn step_animation<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
    sampler_storage: &AssetStorage<Sampler<T::Primitive>>,
    direction: &StepDirection,
) where
    T: AnimationSampling,
{
    for node_entity in hierarchy.nodes.values() {
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                s.step(control_id, sampler_storage, direction);
            }
        }
    }
}

fn set_animation_input<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
    input: f32,
) where
    T: AnimationSampling,
{
    for node_entity in hierarchy.nodes.values() {
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                s.set_input(control_id, input);
            }
        }
    }
}

fn set_blend_weights<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
    weights: &[(usize, T::Channel, f32)],
) where
    T: AnimationSampling,
{
    for &(node_index, ref channel, weight) in weights {
        if let Some(node_entity) = hierarchy.nodes.get(&node_index) {
            if let Ok(mut entry) = world.entry_mut(*node_entity) {
                if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                    s.set_blend_weight(control_id, channel, weight);
                }
            }
        }
    }
}

fn update_animation_rate<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
    rate_multiplier: f32,
) where
    T: AnimationSampling,
{
    for node_entity in hierarchy.nodes.values() {
        if let Ok(mut entry) = world.entry_mut(*node_entity) {
            if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                s.set_rate_multiplier(control_id, rate_multiplier);
            }
        }
    }
}

/// Check if all nodes in an `AnimationHierarchy` are ready for termination, if so remove all
/// `SamplerControlSet`s for the hierarchy, if not request termination on all sampler controls
fn check_and_terminate_animation<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
    buffer: &mut CommandBuffer,
) -> bool
where
    T: AnimationSampling,
{
    // Check for termination
    if check_termination(control_id, hierarchy, world) {
        // Do termination
        for node_entity in hierarchy.nodes.values() {
            let empty = world
                .entry_mut(*node_entity)
                .unwrap()
                .get_component_mut::<SamplerControlSet<T>>()
                .map(|sampler| {
                    sampler.clear(control_id);
                    sampler.is_empty()
                })
                .unwrap_or(false);
            if empty {
                buffer.remove_component::<SamplerControlSet<T>>(*node_entity);
                // samplers.remove(*node_entity);
            }
        }
        true
    } else {
        // Request termination of samplers
        for node_entity in hierarchy.nodes.values() {
            if let Ok(mut entry) = world.entry_mut(*node_entity) {
                if let Ok(ref mut s) = entry.get_component_mut::<SamplerControlSet<T>>() {
                    s.abort(control_id);
                }
            }
        }
        false
    }
}

/// Check if all nodes in an `AnimationHierarcy` are ready for termination.
fn check_termination<T>(
    control_id: u64,
    hierarchy: &AnimationHierarchy<T>,
    world: &mut SubWorld,
) -> bool
where
    T: AnimationSampling,
{
    hierarchy
        .nodes
        .iter()
        .map(|(_, node_entity)| {
            world
                .entry_ref(*node_entity)
                .unwrap()
                .get_component::<SamplerControlSet<T>>()
                .unwrap()
                .check_termination(control_id)
        })
        .all(|s| s)
}
